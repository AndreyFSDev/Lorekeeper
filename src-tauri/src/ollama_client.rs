use reqwest::Client;
use serde_json::{json, Value};

/// Thin client for the Ollama HTTP API. The app only needs model
/// management (list, pull, delete) — generation is handled by SillyTavern.
#[derive(Clone)]
pub struct OllamaClient {
    client: Client,
    pub base_url: String,
}

/// Idle timeout of the `/api/pull` stream. After all layers Ollama silently verifies
/// sha256 for longer than a minute — 60 s broke the stream and classified the check as
/// Network, so large models never finished downloading.
const PULL_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// A refused local socket on Windows otherwise takes ~2 s per request:
/// a down Ollama held the settings screen for seconds. The same threshold as the runtime
/// readiness probes.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

impl OllamaClient {
    /// Build the client. The only failure is initializing reqwest's TLS backend
    /// (the environment, not data): on such a machine there is no network at all, so
    /// the error goes to the caller instead of a panic (V-35).
    ///
    /// Threading: the constructor is cheap and non-blocking; it is called from async tasks
    /// and from synchronous places alike.
    pub fn new(base_url: String) -> Result<Self, String> {
        // Loopback Ollama (127.0.0.1): bypass the system proxy (otherwise the
        // user's machine proxy answers 503 instead of the local server).
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(std::time::Duration::from_secs(600))
            .no_proxy()
            .build()
            .map_err(|e| format!("http-клиент Ollama недоступен: {e}"))?;
        Ok(Self { client, base_url })
    }

    /// GET /api/tags — list of installed models (name).
    pub async fn list_models(&self) -> Result<Vec<String>, String> {
        let resp = self
            .client
            .get(format!("{}/api/tags", self.base_url))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let v: Value = resp.json().await.map_err(|e| e.to_string())?;
        let names = v["models"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(names)
    }

    /// POST /api/pull — model download with progress streaming.
    /// Each NDJSON line contains {"status": "...", "completed": n, "total": n}.
    ///
    /// Stopping (pause or cancel) aborts the request but does not delete what was downloaded:
    /// Ollama keeps partially downloaded layers in `-partial`, and the next pull continues them.
    pub async fn pull(
        &self,
        model: &str,
        flags: crate::downloads::FlagsRef,
        on_progress: crate::downloads::ProgressFn,
    ) -> Result<(), String> {
        let body = json!({ "model": model, "stream": true });
        // The client's shared timeout (600 s) is meant for generation, not for a download
        // stream: a 6 GB vision model on a slow link would break
        // mid-way. The download stream lives up to 3 hours.
        let request = self
            .client
            .post(format!("{}/api/pull", self.base_url))
            .timeout(std::time::Duration::from_secs(3 * 3600))
            .json(&body);
        let resp = crate::paths::with_download_control(&flags, request.send())
            .await?
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("pull {model}: HTTP {}", resp.status()));
        }

        use futures_util::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();

        // Speed/ETA from completed deltas over ~2 s — on a slow link
        // a download looks stuck without this.
        let mut last_completed: u64 = 0;
        let mut last_total: u64 = 0;
        let mut last_at = std::time::Instant::now();
        let mut speed: u64 = 0;
        let mut eta: u64 = 0;

        while let Some(chunk) = crate::paths::with_download_control(
            &flags,
            tokio::time::timeout(PULL_IDLE_TIMEOUT, stream.next()),
        )
            .await?
            .map_err(|_| {
                format!(
                    "таймаут ожидания данных от Ollama при скачивании модели ({} с)",
                    PULL_IDLE_TIMEOUT.as_secs()
                )
            })?
        {
            if let Some(reason) = flags.stop() {
                return Err(reason.to_string());
            }
            let chunk = chunk.map_err(|e| e.to_string())?;
            buf.extend_from_slice(&chunk);
            // NDJSON: lines are separated by \n.
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line).trim().to_string();
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if let Some(status) = v["status"].as_str() {
                        let mut completed = v["completed"].as_u64().unwrap_or(0);
                        let mut total = v["total"].as_u64().unwrap_or(0);

                        if total > 0 {
                            last_total = total;
                        }

                        if completed > 0 {
                            if completed < last_completed {
                                // Ollama moved to a new layer or restarted — reset the counters.
                                last_completed = completed;
                                last_at = std::time::Instant::now();
                                speed = 0;
                                eta = 0;
                            } else {
                                let elapsed = last_at.elapsed().as_secs_f64();
                                if elapsed >= 2.0 {
                                    speed = ((completed - last_completed) as f64 / elapsed) as u64;
                                    last_completed = completed;
                                    last_at = std::time::Instant::now();
                                }
                            }
                        } else if status == "success" {
                            completed = last_total.max(1);
                            total = last_total.max(1);
                            speed = 0;
                            eta = 0;
                        } else if last_total > 0 && (status.contains("digest") || status.contains("manifest")) {
                            completed = last_completed.max(last_total);
                            total = last_total;
                        }

                        if total > 0 && completed < total && speed > 0 {
                            eta = (total - completed) / speed.max(1);
                        }
                        on_progress(completed, total, status.to_string(), speed, eta);
                    }
                    if v["error"].is_string() {
                        return Err(v["error"].as_str().unwrap_or("pull error").into());
                    }
                }
            }
        }
        if !buf.is_empty() {
            let line = String::from_utf8_lossy(&buf).trim().to_string();
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if let Some(status) = v["status"].as_str() {
                    let completed = v["completed"].as_u64().unwrap_or(last_total.max(1));
                    let total = v["total"].as_u64().unwrap_or(last_total.max(1));
                    on_progress(completed, total, status.to_string(), 0, 0);
                }
                if let Some(err) = v["error"].as_str() {
                    return Err(err.into());
                }
            }
        }
        Ok(())
    }

    /// GET /api/ps — models currently in memory (name, size, VRAM share).
    pub async fn running_models(&self) -> Vec<(String, u64, u64)> {
        let Ok(resp) = self
            .client
            .get(format!("{}/api/ps", self.base_url))
            .send()
            .await
        else {
            return Vec::new();
        };
        let Ok(v) = resp.json::<Value>().await else {
            return Vec::new();
        };
        v["models"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| {
                        Some((
                            m["name"].as_str()?.to_string(),
                            m["size"].as_u64().unwrap_or(0),
                            m["size_vram"].as_u64().unwrap_or(0),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// POST /api/show — model metadata: architecture, layers, number of KV heads.
    pub async fn show(&self, model: &str) -> Result<Value, String> {
        let resp = self
            .client
            .post(format!("{}/api/show", self.base_url))
            .json(&json!({ "model": model }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("show {model}: HTTP {}", resp.status()));
        }
        resp.json::<Value>().await.map_err(|e| e.to_string())
    }

    /// Model weight size on disk (bytes) from /api/tags.
    pub async fn model_size(&self, model: &str) -> Option<u64> {
        let resp = self
            .client
            .get(format!("{}/api/tags", self.base_url))
            .send()
            .await
            .ok()?;
        let v: Value = resp.json().await.ok()?;
        v["models"].as_array()?.iter().find_map(|m| {
            let name = m["name"].as_str()?;
            if crate::models::is_model_installed(&[name.to_string()], model) {
                m["size"].as_u64()
            } else {
                None
            }
        })
    }

    /// DELETE /api/delete — delete a model from disk to free space.
    pub async fn delete(&self, model: &str) -> Result<(), String> {
        let body = json!({ "name": model });
        let resp = self
            .client
            .delete(format!("{}/api/delete", self.base_url))
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            Err(format!("delete {model}: HTTP {status}: {text}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn audit_silent_pull_can_be_cancelled() {
        use axum::{body::Body, response::Response, routing::post, Router};
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = OllamaClient::new(format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let app = Router::new().route("/api/pull", post(|| async {
            let chunks = futures_util::stream::pending::<Result<bytes::Bytes, std::io::Error>>();
            Response::new(Body::from_stream(chunks))
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cancel = Arc::new(AtomicBool::new(false));
        let flags = crate::downloads::Flags::with_cancel(cancel.clone());
        let operation = client.pull("test", flags, Arc::new(|_, _, _, _, _| {}));
        tokio::pin!(operation);
        assert!(tokio::time::timeout(std::time::Duration::from_millis(100), &mut operation).await.is_err());
        cancel.store(true, Ordering::Relaxed);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), &mut operation).await;
        server.abort();
        assert_eq!(outcome.expect("cancellation waited for SHA verification").unwrap_err(), crate::downloads::CANCELLED);
    }

    /// The `/api/pull` stream idle timeout must be at least five minutes: Ollama
    /// silently verifies sha256 for longer than 60 s, and the old timeout broke the connection.
    #[test]
    fn test_pull_idle_timeout_covers_quiet_verify() {
        assert!(PULL_IDLE_TIMEOUT >= std::time::Duration::from_secs(5 * 60));
    }
}
