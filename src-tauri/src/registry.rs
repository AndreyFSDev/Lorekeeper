//! Direct model download from the Ollama registry.
//!
//! A fallback for machines where the native Ollama client cannot reach the registry:
//! the provider's DNS does not resolve `registry.ollama.ai`, a VPN or antivirus interferes.
//! The Ollama client's resolver cannot be changed, so we fetch the manifest and layers
//! ourselves and lay them out in its store in the same form it would have done
//! itself: `blobs/sha256-<hex>` and `manifests/registry.ollama.ai/<ns>/<name>/<tag>`.
//! Each layer is verified against the sha256 from the manifest, unfinished ones are resumed.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::Client;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// Ollama registry host. It is also the first path component in the model store.
pub const REGISTRY_HOST: &str = "registry.ollama.ai";
const REGISTRY_BASE: &str = "https://registry.ollama.ai";

const MANIFEST_ACCEPT: &str = "application/vnd.docker.distribution.manifest.v2+json";

/// How many times we retry a layer before giving up (resume continues from the break point).
const BLOB_ATTEMPTS: u32 = 4;
/// Socket idle timeout: the connection to the registry can go silent without a disconnect.
const BLOB_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// DoH endpoints in order of preference. The last two are given by IP: they
/// respond even when system DNS resolves nothing at all.
const DOH_ENDPOINTS: [&str; 4] = [
    "https://cloudflare-dns.com/dns-query?name={host}&type=A",
    "https://dns.google/resolve?name={host}&type=A",
    "https://1.1.1.1/dns-query?name={host}&type=A",
    "https://8.8.8.8/resolve?name={host}&type=A",
];

pub type ProgressFn = crate::downloads::ProgressFn;

/// Model name split into registry parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub namespace: String,
    pub name: String,
    pub tag: String,
}

pub fn parse_tag(raw: &str) -> Result<Repo, String> {
    let cleaned = raw.trim().trim_start_matches("https://").trim();
    let cleaned = cleaned
        .strip_prefix(REGISTRY_HOST)
        .map(|rest| rest.trim_start_matches('/'))
        .unwrap_or(cleaned);
    let (path, tag) = match cleaned.rsplit_once(':') {
        Some((path, tag)) if !tag.contains('/') => (path, tag),
        _ => (cleaned, "latest"),
    };
    let (namespace, name) = match path.split_once('/') {
        Some((namespace, name)) => (namespace, name),
        None => ("library", path),
    };
    // A part becomes a path component of `manifest_path`, so it must not be
    // able to walk the store: all-dot parts (`.`/`..`/`...`) are rejected —
    // `..` used to place the manifest one level up inside the store (V-36).
    // Trailing dots are rejected too: Win32 trims them, so `x.` and `x` would
    // be the same file and one tag could overwrite another's manifest.
    let valid_part = |s: &str| {
        !s.is_empty()
            && !s.ends_with('.')
            && !s.chars().all(|c| c == '.')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !valid_part(namespace) || !valid_part(name) || !valid_part(tag) {
        return Err(format!("неразборчивое имя модели: {raw}"));
    }
    Ok(Repo {
        namespace: namespace.to_ascii_lowercase(),
        name: name.to_ascii_lowercase(),
        tag: tag.to_string(),
    })
}

/// Manifest path in the Ollama store — exactly the one Ollama itself reads.
pub fn manifest_path(store: &Path, repo: &Repo) -> PathBuf {
    store
        .join("manifests")
        .join(REGISTRY_HOST)
        .join(&repo.namespace)
        .join(&repo.name)
        .join(&repo.tag)
}

/// Expected digest hex in lowercase: `{:x}` is always lowercase, while the manifest
/// may return uppercase — otherwise a valid layer was rejected by the checksum.
fn expected_hex(digest: &str) -> String {
    digest.trim_start_matches("sha256:").to_ascii_lowercase()
}

/// Blob file name: the colon in the digest is replaced with a hyphen, as Ollama does.
/// Hex in lowercase: the registry may return uppercase, and one layer would land
/// under two different names.
pub fn blob_file_name(digest: &str) -> String {
    format!("sha256-{}", expected_hex(digest))
}

/// Fetch the model directly from the registry into the `store`.
pub async fn pull(
    store: &Path,
    raw_tag: &str,
    flags: crate::downloads::FlagsRef,
    on_progress: ProgressFn,
) -> Result<(), String> {
    let repo = parse_tag(raw_tag)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(3 * 3600))
        .build()
        .map_err(|e| e.to_string())?;
    match pull_from(&client, REGISTRY_BASE, store, &repo, flags.clone(), on_progress.clone()).await {
        Ok(()) => Ok(()),
        // Pause and cancel are no reason to look for another registry address: the unfinished
        // layer is on disk and waits for continuation.
        Err(error) if error == crate::downloads::PAUSED || error == crate::downloads::CANCELLED => {
            Err(error)
        }
        Err(error) => {
            // The name may have failed to resolve or resolved to a substituted address — that is
            // what provider filtering looks like. We take addresses via DoH and try
            // them in turn; the TLS name stays the same.
            on_progress(0, 0, "беру адрес реестра через DoH…".into(), 0, 0);
            let clients = crate::paths::with_download_control(&flags, build_doh_clients())
                .await?
                .map_err(|e| format!("{error}; обойти DNS не удалось: {e}"))?;
            let mut last = String::new();
            for pinned in &clients {
                match pull_from(pinned, REGISTRY_BASE, store, &repo, flags.clone(), on_progress.clone()).await {
                    Ok(()) => return Ok(()),
                    Err(e) if e == crate::downloads::PAUSED || e == crate::downloads::CANCELLED => {
                        return Err(e);
                    }
                    Err(e) => last = e,
                }
            }
            Err(format!("{error}; через DoH: {last}"))
        }
    }
}

/// Clients with registry addresses obtained via DoH — for when the
/// system DNS returns no name or returns a known-broken address.
/// One client per address: one failed edge must not sink the download.
async fn build_doh_clients() -> Result<Vec<Client>, String> {
    let ips = resolve_via_doh(REGISTRY_HOST).await?;
    let mut clients = Vec::with_capacity(ips.len());
    for ip in ips {
        let client = Client::builder()
            .timeout(Duration::from_secs(3 * 3600))
            .resolve(REGISTRY_HOST, SocketAddr::new(ip, 443))
            // The registry serves layers via a redirect to a CDN: its name is also taken via DoH,
            // otherwise the manifest would pass but the first layer would fail on system DNS.
            .dns_resolver(std::sync::Arc::new(DohResolver))
            .build()
            .map_err(|e| e.to_string())?;
        clients.push(client);
    }
    if clients.is_empty() {
        return Err("DoH не вернул ни одного адреса".into());
    }
    Ok(clients)
}

/// Resolver for DoH-branch clients for all names except the pinned registry.
struct DohResolver;

impl reqwest::dns::Resolve for DohResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let ips = resolve_via_doh(&host).await?;
            // Port 0 — "port from the request address".
            let addrs: reqwest::dns::Addrs =
                Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(addrs)
        })
    }
}

/// Resolve a name over DNS-over-HTTPS: the system resolver has already failed.
async fn resolve_via_doh(host: &str) -> Result<Vec<IpAddr>, String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| e.to_string())?;
    let mut last = String::from("ни один эндпоинт не ответил");
    for template in DOH_ENDPOINTS {
        let url = template.replace("{host}", host);
        let Ok(resp) = client
            .get(&url)
            .header("accept", "application/dns-json")
            .send()
            .await
        else {
            continue;
        };
        let Ok(body) = resp.json::<Value>().await else {
            continue;
        };
        let ips = doh_answers(&body);
        if !ips.is_empty() {
            return Ok(ips);
        }
        last = format!("{url}: пустой ответ");
    }
    Err(format!(
        "имя {host} не разрешается ни системным DNS, ни через DoH ({last})"
    ))
}

/// A records from the DoH response (`Answer[].type == 1`).
fn doh_answers(body: &Value) -> Vec<IpAddr> {
    body["Answer"]
        .as_array()
        .map(|answers| {
            answers
                .iter()
                .filter(|record| record["type"].as_u64() == Some(1))
                .filter_map(|record| record["data"].as_str()?.parse::<IpAddr>().ok())
                .collect()
        })
        .unwrap_or_default()
}

async fn pull_from(
    client: &Client,
    base: &str,
    store: &Path,
    repo: &Repo,
    flags: crate::downloads::FlagsRef,
    on_progress: ProgressFn,
) -> Result<(), String> {
    on_progress(0, 0, "запрашиваю манифест модели…".into(), 0, 0);
    let manifest_url = format!(
        "{base}/v2/{}/{}/manifests/{}",
        repo.namespace, repo.name, repo.tag
    );
    let request = client
        .get(&manifest_url)
        .header("accept", MANIFEST_ACCEPT);
    let resp = crate::paths::with_download_control(&flags, request.send())
        .await?
        .map_err(|e| format!("реестр недоступен: {e}"))?;
    let status = resp.status();
    if status.as_u16() == 404 {
        return Err(format!(
            "модель {}:{} не найдена в реестре Ollama",
            repo.name, repo.tag
        ));
    }
    if !status.is_success() {
        return Err(format!("реестр ответил HTTP {status} на запрос манифеста"));
    }
    let manifest_bytes = crate::paths::with_download_control(&flags, resp.bytes())
        .await?
        .map_err(|e| format!("манифест не прочитан: {e}"))?
        .to_vec();
    let manifest: Value =
        serde_json::from_slice(&manifest_bytes).map_err(|e| format!("манифест повреждён: {e}"))?;

    let mut items: Vec<(String, u64)> = Vec::new();
    let config = &manifest["config"];
    let config_digest = config["digest"].as_str().unwrap_or_default().to_string();
    if !config_digest.is_empty() {
        items.push((config_digest, config["size"].as_u64().unwrap_or(0)));
    }
    for layer in manifest["layers"].as_array().unwrap_or(&Vec::new()) {
        if let Some(digest) = layer["digest"].as_str() {
            items.push((digest.to_string(), layer["size"].as_u64().unwrap_or(0)));
        }
    }
    if items.is_empty() {
        return Err("в манифесте нет слоёв модели".into());
    }
    let total: u64 = items.iter().map(|(_, size)| size).sum();

    let blobs = store.join("blobs");
    tokio::fs::create_dir_all(&blobs)
        .await
        .map_err(|e| format!("не создан каталог хранилища: {e}"))?;

    let mut completed = 0u64;
    for (digest, size) in &items {
        let done = download_blob(
            client, base, repo, digest, *size, &blobs, &flags, completed, total, &on_progress,
        )
        .await?;
        completed += done;
    }

    let target = manifest_path(store, repo);
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("не создан каталог манифестов: {e}"))?;
    }
    write_manifest(&target, &manifest_bytes).await?;
    on_progress(total, total, "модель скачана напрямую".into(), 0, 0);
    Ok(())
}

/// Write the model manifest atomically (V-37): tmp + fsync + rename. A crash
/// mid-write used to leave a truncated manifest in the Ollama store, and the
/// model counts as installed only when its manifest is readable — a re-pull
/// then never ran. Same rule as `json_store::write_atomic`, kept async here:
/// the pull task must not block its worker on disk IO. The tmp file is removed
/// on every failure path.
async fn write_manifest(target: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut tmp_name = target.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    let result = write_manifest_via_tmp(target, bytes, &tmp).await;
    if result.is_err() {
        // Deterministic cleanup: a failed write must not leave a stray tmp file.
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

async fn write_manifest_via_tmp(target: &Path, bytes: &[u8], tmp: &Path) -> Result<(), String> {
    let mut file = tokio::fs::File::create(tmp)
        .await
        .map_err(|e| format!("манифест не записан: {e}"))?;
    file.write_all(bytes)
        .await
        .map_err(|e| format!("манифест не записан: {e}"))?;
    // Without fsync the rename can overtake the data on disk.
    file.sync_all()
        .await
        .map_err(|e| format!("манифест не записан: {e}"))?;
    drop(file);
    // `tokio::fs::rename` replaces an existing target (MoveFileEx with
    // REPLACE_EXISTING on Windows), so the manifest is never half-written:
    // readers see either the previous bytes or the new ones.
    tokio::fs::rename(tmp, target)
        .await
        .map_err(|e| format!("манифест не записан: {e}"))
}

/// Download one layer, resuming the unfinished file. Returns the layer size.
#[allow(clippy::too_many_arguments)]
async fn download_blob(
    client: &Client,
    base: &str,
    repo: &Repo,
    digest: &str,
    size: u64,
    blobs: &Path,
    flags: &crate::downloads::Flags,
    completed: u64,
    total: u64,
    on_progress: &ProgressFn,
) -> Result<u64, String> {
    // Digest validation: protection from path traversal through a compromised manifest
    let hex_part = digest.strip_prefix("sha256:").ok_or_else(|| format!("неожиданный формат digest: {digest}"))?;
    if hex_part.len() != 64 || !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("невалидный digest: {digest}"));
    }
    let name = blob_file_name(digest);
    let final_path = blobs.join(&name);
    if matches!(std::fs::metadata(&final_path), Ok(meta) if meta.len() == size) {
        return Ok(size);
    }
    let partial = blobs.join(format!("{name}-partial"));
    let url = format!(
        "{base}/v2/{}/{}/blobs/{}",
        repo.namespace, repo.name, digest
    );

    let mut attempt = 0;
    let mut got;
    let mut hasher;
    loop {
        attempt += 1;
        let mut have = std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(0);
        if have > size {
            let _ = std::fs::remove_file(&partial);
            have = 0;
        }

        // Resuming is folded into one hash: bytes already on disk are mixed
        // into it before reading the remainder, otherwise the integrity check would be incomplete.
        let mut state = Sha256::new();
        if have > 0 {
            let mut existing = tokio::fs::File::open(&partial)
                .await
                .map_err(|e| format!("недокачанный слой не открыт: {e}"))?;
            let mut buf = vec![0u8; 1 << 20];
            loop {
                use tokio::io::AsyncReadExt;
                if let Some(reason) = flags.stop() { return Err(reason.to_string()); }
                let read = existing
                    .read(&mut buf)
                    .await
                    .map_err(|e| format!("недокачанный слой не прочитан: {e}"))?;
                if read == 0 {
                    break;
                }
                state.update(&buf[..read]);
            }
        }
        got = have;

        // The layer is already on disk in full — only the hash check and rename remain.
        // Without this branch the code requested `Range: bytes={size}-`, i.e. a range past
        // the end of the file: the registry returned 416, attempts ended in an error, and
        // a stub remained — installation could not progress on any retry.
        if have == size {
            hasher = state;
            break;
        }

        let mut request = client.get(&url);
        if have > 0 {
            request = request.header("range", format!("bytes={have}-"));
        }
        let resp = match crate::paths::with_download_control(flags, request.send()).await? {
            Ok(resp) => resp,
            Err(e) if attempt < BLOB_ATTEMPTS => {
                let _ = e;
                crate::paths::with_download_control(flags, tokio::time::sleep(Duration::from_secs(2))).await?;
                continue;
            }
            Err(e) => return Err(format!("слой {digest} не скачан: {e}")),
        };
        // The registry may ignore Range: then the file starts over.
        let appending = have > 0 && resp.status().as_u16() == 206;
        if !resp.status().is_success() {
            if attempt < BLOB_ATTEMPTS {
                crate::paths::with_download_control(flags, tokio::time::sleep(Duration::from_secs(2))).await?;
                continue;
            }
            return Err(format!("слой {digest}: HTTP {}", resp.status()));
        }
        if have > 0 && !appending {
            let _ = std::fs::remove_file(&partial);
            have = 0;
            got = 0;
            state = Sha256::new();
        }
        let mut handle = if have > 0 {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(&partial)
                .await
                .map_err(|e| format!("недокачанный слой не открыт на запись: {e}"))?
        } else {
            tokio::fs::File::create(&partial)
                .await
                .map_err(|e| format!("слой не создан: {e}"))?
        };

        // Silence on the socket is also a break: the connection to the registry can go silent
        // without RST, and without an idle timeout the download hangs forever. Resume with
        // Range continues from the last written byte.
        let mut stream = resp.bytes_stream();
        let mut last_emit = Instant::now();
        let mut failed = None;
        loop {
            let next = crate::paths::with_download_control(flags, tokio::time::timeout(BLOB_IDLE_TIMEOUT, stream.next())).await;
            let chunk = match next {
                Err(reason) => {
                    handle.flush().await.map_err(|e| format!("слой не записан: {e}"))?;
                    return Err(reason);
                }
                Ok(chunk) => match chunk {
                Err(_) => {
                    failed = Some("таймаут ожидания данных (60 с)".to_string());
                    break;
                }
                Ok(None) => break,
                Ok(Some(Ok(chunk))) => chunk,
                Ok(Some(Err(e))) => {
                    failed = Some(e.to_string());
                    break;
                }
                },
            };
            if let Some(reason) = flags.stop() {
                // The unfinished layer stays on disk: continuation rehashes
                // it and appends the remainder via Range.
                let _ = handle.flush().await;
                return Err(reason.to_string());
            }
            if let Err(e) = handle.write_all(&chunk).await {
                return Err(format!("слой не записан: {e}"));
            }
            state.update(&chunk);
            got += chunk.len() as u64;
            // The server (or a substituted CDN) returned more than the manifest — break the stream,
            // otherwise endless writing fills the disk before the timeout fires.
            if got > size {
                let _ = handle.flush().await;
                return Err(format!("получено {got} байт при ожидании {size}"));
            }
            if last_emit.elapsed() >= Duration::from_millis(500) {
                last_emit = Instant::now();
                on_progress(
                    completed + got,
                    total,
                    format!("скачиваю {} ({})…", short_digest(digest), layer_note(size)),
                    0,
                    0,
                );
            }
        }
        let _ = handle.flush().await;
        drop(handle);
        hasher = state;
        match failed {
            None => break,
            Some(e) if attempt < BLOB_ATTEMPTS => {
                on_progress(
                    completed + got,
                    total,
                    format!("связь прервалась ({e}) — докачиваю {}…", short_digest(digest)),
                    0,
                    0,
                );
                crate::paths::with_download_control(flags, tokio::time::sleep(Duration::from_secs(2))).await?;
                continue;
            }
            Some(e) => return Err(format!("слой {digest} оборвался: {e}")),
        }
    }

    let got = std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(got);
    if got != size {
        return Err(format!(
            "слой {digest} скачан не полностью ({got} из {size} байт)"
        ));
    }
    let actual = format!("{:x}", hasher.finalize());
    let expected = expected_hex(digest);
    if actual != expected {
        // Delete the corrupt layer: with it, resume would start over, not from garbage.
        let _ = std::fs::remove_file(&partial);
        return Err(format!("слой {digest} не совпал по контрольной сумме"));
    }
    // On Windows rename fails if final_path already exists —
    // delete the existing file before replacing.
    if final_path.exists() {
        let _ = std::fs::remove_file(&final_path);
    }
    std::fs::rename(&partial, &final_path).map_err(|e| format!("слой не переименован: {e}"))?;
    Ok(size)
}

fn short_digest(digest: &str) -> String {
    digest
        .trim_start_matches("sha256:")
        .chars()
        .take(12)
        .collect()
}

fn layer_note(size: u64) -> String {
    format!("{:.0} МБ", size as f64 / 1024.0 / 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_parse_tag() {
        let repo = parse_tag("huihui_ai/qwen3-abliterated:8b-v2").unwrap();
        assert_eq!(repo.namespace, "huihui_ai");
        assert_eq!(repo.name, "qwen3-abliterated");
        assert_eq!(repo.tag, "8b-v2");

        // Official library: no slash and no tag.
        let repo = parse_tag("bge-m3").unwrap();
        assert_eq!(repo.namespace, "library");
        assert_eq!(repo.name, "bge-m3");
        assert_eq!(repo.tag, "latest");

        // Full name with host and tag with uppercase letters.
        let repo = parse_tag("registry.ollama.ai/huihui_ai/qwen3-vl-abliterated:8b-instruct-q4_K_M")
            .unwrap();
        assert_eq!(repo.namespace, "huihui_ai");
        assert_eq!(repo.name, "qwen3-vl-abliterated");
        assert_eq!(repo.tag, "8b-instruct-q4_K_M");

        assert!(parse_tag("").is_err());
        assert!(parse_tag("плохое имя:тег").is_err());
    }

    /// V-36: `..` in namespace/name/tag must never reach `manifest_path` — it
    /// joins the parts as path components, so `..` would write the manifest one
    /// level up inside the store. Interior dots of real names (`qwen2.5:v1.0`)
    /// stay legal; all-dot parts and trailing dots (Win32 trims them) do not.
    #[test]
    fn test_parse_tag_rejects_path_traversal() {
        for raw in [
            "..",
            "../evil:tag",
            "ns/..",
            "ns/..:tag",
            "library/..",
            "ns/name:..",
            "ns/...:tag",
            "./name:tag",
            "ns/name:tag.",
        ] {
            assert!(parse_tag(raw).is_err(), "{raw} обязан быть отвергнут");
        }
        // Legitimate dotted parts are still accepted.
        let repo = parse_tag("library/qwen2.5:v1.0").unwrap();
        assert_eq!(repo.name, "qwen2.5");
        assert_eq!(repo.tag, "v1.0");
    }

    #[test]
    fn test_manifest_path_matches_ollama_layout() {
        let repo = parse_tag("huihui_ai/qwen3-abliterated:8b-v2").unwrap();
        let path = manifest_path(Path::new("Z:\\AImodels\\ollama"), &repo);
        assert_eq!(
            path,
            PathBuf::from("Z:\\AImodels\\ollama")
                .join("manifests")
                .join("registry.ollama.ai")
                .join("huihui_ai")
                .join("qwen3-abliterated")
                .join("8b-v2")
        );
    }

    #[test]
    fn test_blob_file_name() {
        assert_eq!(
            blob_file_name("sha256:6da140c19a3625dc4e98e57a2d87cab5b5613854402f1f389def4540037541ab"),
            "sha256-6da140c19a3625dc4e98e57a2d87cab5b5613854402f1f389def4540037541ab"
        );
    }

    /// The registry may return a digest in uppercase: the file name and the
    /// checksum comparison must normalize the hex case.
    #[test]
    fn test_digest_hex_is_case_insensitive() {
        assert_eq!(
            blob_file_name("sha256:6DA140C19A3625DC4E98E57A2D87CAB5B5613854402F1F389DEF4540037541AB"),
            "sha256-6da140c19a3625dc4e98e57a2d87cab5b5613854402f1f389def4540037541ab"
        );
        assert_eq!(
            expected_hex("sha256:ABCDEF"),
            expected_hex("sha256:abcdef")
        );
    }

    #[test]
    fn test_doh_answers_reads_ipv4_only() {
        let body: Value = serde_json::from_str(
            r#"{"Status":0,"Answer":[
                {"name":"registry.ollama.ai","type":5,"data":"example.cdn.cloudflare.net"},
                {"name":"registry.ollama.ai","type":1,"data":"104.18.16.170"},
                {"name":"registry.ollama.ai","type":1,"data":"104.18.17.170"},
                {"name":"registry.ollama.ai","type":28,"data":"2606:4700::6812:11aa"}
            ]}"#,
        )
        .unwrap();
        let ips = doh_answers(&body);
        assert_eq!(ips.len(), 2);
        assert_eq!(ips[0].to_string(), "104.18.16.170");
    }

    /// Local stub registry: two OCI handlers, nothing more.
    async fn fixture_registry(manifest: Vec<u8>, blob: Vec<u8>) -> String {
        use axum::{routing::get, Router};

        let app = Router::new()
            .route(
                "/v2/{ns}/{name}/manifests/{tag}",
                get({
                    let manifest = manifest.clone();
                    move || {
                        let manifest = manifest.clone();
                        async move { manifest }
                    }
                }),
            )
            .route(
                "/v2/{ns}/{name}/blobs/{digest}",
                get({
                    let blob = blob.clone();
                    move || {
                        let blob = blob.clone();
                        async move { blob }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn fixture_manifest(digest: &str, size: u64) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": MANIFEST_ACCEPT,
            "config": {
                "mediaType": "application/vnd.ollama.image.config",
                "digest": digest,
                "size": size
            },
            "layers": [{
                "mediaType": "application/vnd.ollama.image.model",
                "digest": digest,
                "size": size
            }]
        }))
        .unwrap()
    }

    fn fixture_store(name: &str) -> PathBuf {
        let store = std::env::temp_dir().join(format!("lk-registry-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        store
    }

    async fn pull_fixture(base: &str, store: &Path, repo: &Repo) -> Result<(), String> {
        // The fixture server is loopback: bypass the system proxy, otherwise the test
        // depends on machine settings (503 from the proxy instead of the server's reply).
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .no_proxy()
            .build()
            .unwrap();
        let progress: ProgressFn = Arc::new(|_, _, _, _, _| {});
        pull_from(
            &client,
            base,
            store,
            repo,
            crate::downloads::Flags::new(),
            progress,
        )
        .await
    }

    #[tokio::test]
    async fn test_pull_from_writes_ollama_store_layout() {
        let payload = b"model weights".to_vec();
        let digest = format!("sha256:{:x}", Sha256::digest(&payload));
        let manifest = fixture_manifest(&digest, payload.len() as u64);
        let base = fixture_registry(manifest.clone(), payload.clone()).await;
        let repo = parse_tag("huihui_ai/qwen3-abliterated:8b-v2").unwrap();
        let store = fixture_store("ok");

        pull_fixture(&base, &store, &repo).await.unwrap();

        assert_eq!(
            std::fs::read(manifest_path(&store, &repo)).unwrap(),
            manifest,
            "манифест обязан лежать байт в байт под путём, который читает Ollama"
        );
        let blob = store.join("blobs").join(blob_file_name(&digest));
        assert_eq!(std::fs::read(&blob).unwrap(), payload);
        assert!(
            !store
                .join("blobs")
                .join(format!("{}-partial", blob_file_name(&digest)))
                .exists(),
            "недокачанный файл после успеха остаться не должен"
        );
        let _ = std::fs::remove_dir_all(&store);
    }

    #[tokio::test]
    async fn test_pull_from_rejects_layer_with_wrong_checksum() {
        let payload = b"model weights".to_vec();
        let wrong = format!("sha256:{:x}", Sha256::digest(b"other weights"));
        let base = fixture_registry(fixture_manifest(&wrong, payload.len() as u64), payload).await;
        let repo = parse_tag("bge-m3").unwrap();
        let store = fixture_store("bad");

        let error = pull_fixture(&base, &store, &repo).await.unwrap_err();

        assert!(error.contains("контрольной сумме"), "получено: {error}");
        assert!(
            !store.join("blobs").join(blob_file_name(&wrong)).exists(),
            "битый слой не должен попасть в хранилище как готовый"
        );
        assert!(
            !manifest_path(&store, &repo).exists(),
            "манифест пишется только после того, как все слои проверены"
        );
        let _ = std::fs::remove_dir_all(&store);
    }

    /// V-37: the manifest write is atomic (tmp + fsync + rename). A failed tmp
    /// write must leave the previous manifest untouched — a truncated manifest
    /// would make the model look installed while its bytes are wrong — and a
    /// successful one replaces it without tmp leftovers.
    #[tokio::test]
    async fn test_manifest_write_is_atomic() {
        let store = fixture_store("atomic");
        let repo = parse_tag("bge-m3").unwrap();
        let target = manifest_path(&store, &repo);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"old manifest").unwrap();

        // A directory where the tmp file belongs makes the write fail midway.
        let mut tmp_name = target.as_os_str().to_owned();
        tmp_name.push(".tmp");
        let tmp = PathBuf::from(tmp_name);
        std::fs::create_dir_all(&tmp).unwrap();
        let error = write_manifest(&target, b"new manifest").await.unwrap_err();
        assert!(!error.is_empty());
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"old manifest",
            "прежний манифест пережил неудачную запись"
        );
        std::fs::remove_dir(&tmp).unwrap();

        write_manifest(&target, b"new manifest").await.unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new manifest", "манифест заменён целиком");
        assert!(!tmp.exists(), "временный файл после успеха не остаётся");
        let _ = std::fs::remove_dir_all(&store);
    }

    /// Live check of direct download from the real registry. Skipped by default:
    /// needs network and about a gigabyte of traffic.
    /// Run: `$env:CLOUFAM_REGISTRY_TEST_STORE='<dir>';`
    /// `$env:CLOUFAM_REGISTRY_TEST_TAG='library/qwen3:1.7b';`
    /// `cargo test --lib live_pull_from_real_registry -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "live network: downloads a model from the Ollama registry"]
    async fn live_pull_from_real_registry() {
        let Ok(store) = std::env::var("CLOUFAM_REGISTRY_TEST_STORE") else {
            eprintln!("CLOUFAM_REGISTRY_TEST_STORE не задан — проверка пропущена");
            return;
        };
        let tag = std::env::var("CLOUFAM_REGISTRY_TEST_TAG")
            .unwrap_or_else(|_| "library/qwen3:1.7b".to_string());
        let store = PathBuf::from(store);
        let progress: ProgressFn = Arc::new(|completed, total, status, _, _| {
            if total > 0 {
                println!("{status}: {completed}/{total}");
            } else {
                println!("{status}");
            }
        });
        pull(&store, &tag, crate::downloads::Flags::new(), progress)
            .await
            .unwrap();

        let repo = parse_tag(&tag).unwrap();
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(manifest_path(&store, &repo)).unwrap()).unwrap();
        let layers = manifest["layers"].as_array().unwrap();
        for item in std::iter::once(&manifest["config"]).chain(layers.iter()) {
            let digest = item["digest"].as_str().unwrap();
            let size = item["size"].as_u64().unwrap();
            let meta = std::fs::metadata(store.join("blobs").join(blob_file_name(digest)))
                .unwrap_or_else(|e| panic!("слой {digest} не найден: {e}"));
            assert_eq!(meta.len(), size, "слой {digest} не того размера");
        }
        println!("{tag}: манифест и {} блобов на месте", layers.len() + 1);
    }
    #[tokio::test]
    async fn audit_silent_registry_blob_can_be_cancelled() {
        use axum::{body::Body, response::Response, routing::get, Router};
        use std::sync::atomic::{AtomicBool, Ordering};
        let digest = format!("sha256:{}", "1".repeat(64));
        let repo = Repo { namespace: "library".into(), name: "fixture".into(), tag: "latest".into() };
        let route = format!("/v2/library/fixture/blobs/{digest}");
        let app = Router::new().route(&route, get(|| async {
            Response::new(Body::from_stream(futures_util::stream::pending::<Result<bytes::Bytes, std::io::Error>>()))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let store = fixture_store("silent-blob");
        std::fs::create_dir_all(&store).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let flags = crate::downloads::Flags::with_cancel(cancel.clone());
        let client = Client::builder().no_proxy().build().unwrap();
        let progress: ProgressFn = Arc::new(|_, _, _, _, _| {});
        let operation = download_blob(&client, &base, &repo, &digest, 20, &store, &flags, 0, 20, &progress);
        tokio::pin!(operation);
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut operation).await.is_err());
        cancel.store(true, Ordering::Relaxed);
        let result = tokio::time::timeout(Duration::from_secs(1), &mut operation).await;
        server.abort();
        assert_eq!(result.expect("cancel waited for network traffic").unwrap_err(), crate::downloads::CANCELLED);
        std::fs::remove_dir_all(&store).unwrap();
    }

}



