//! Shared body for the app's file-backed JSON stores: read and write under one
//! lock per store, parse-with-backup and tmp + fsync + rename on save. This is
//! the single implementation of those rules (V-31): `traits.rs`, `settings.rs`,
//! `relationships.rs`, `bonds.rs` and all `rpg/*.rs` build on it instead of
//! keeping byte-identical copies. Without one shared lock a read-modify-write
//! from two callers (for example the post-reply evaluator and a "Save" button)
//! loses the other's edit; that race was already fixed once for traits.json and
//! relationships.json.
//!
//! Error contract (V-09): a file that exists but cannot be read or parsed is an
//! error, not an empty store — `update`/`update_if_ok` refuse to write over it.
//! A file that was read but failed to parse is preserved as `<name>.json.corrupt`.
//!
//! Each calling module still declares its own `static STORE` (locks of
//! different stores are independent) and its own `store_path()`.
//!
//! Execution: blocking file IO on the caller's thread — sync Tauri commands run
//! it on the main thread, async paths must go through `spawn_blocking`.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use serde::de::DeserializeOwned;
use serde::Serialize;

fn lock(guard: &'static Mutex<()>) -> MutexGuard<'static, ()> {
    guard.lock().unwrap_or_else(|e| e.into_inner())
}

/// Read state of a store file: `Ok(None)` — the file is missing, `Ok(Some(v))`
/// — it was read and parsed, `Err` — it exists but could not be read (IO error,
/// sharing violation, non-UTF-8) or parsed. A file that was read but failed to
/// parse is copied to `<name>.json.corrupt` first, so the bytes survive even
/// when the caller refuses to write (V-09). Call only under `lock()` or from a
/// path that holds no store lock.
pub(crate) fn read_state<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            // The file is there (or unreachable) and its bytes are unknown:
            // treating this as "no file" would let the next write clobber it.
            let msg = format!("{} не читается ({e})", path.display());
            crate::boot_log(&msg);
            return Err(msg);
        }
    };
    match serde_json::from_str(&text) {
        Ok(value) => Ok(Some(value)),
        Err(e) => {
            let backup = path.with_extension("json.corrupt");
            let _ = std::fs::write(&backup, &text);
            let msg = format!(
                "{} повреждён ({e}), копия: {}",
                path.display(),
                backup.display()
            );
            crate::boot_log(&msg);
            Err(msg)
        }
    }
}

/// Read-only load with the fallback semantics callers rely on: a missing,
/// unreadable or corrupt file yields `T::default()` (a corrupt file is still
/// preserved as `<name>.json.corrupt`). Everyone who writes must go through
/// `update`/`update_if_ok`, which refuse those states instead of clobbering
/// real data. Blocking IO on the caller's thread.
pub fn load<T: DeserializeOwned + Default>(guard: &'static Mutex<()>, path: &Path) -> T {
    let _guard = lock(guard);
    match read_state(path) {
        Ok(Some(value)) => value,
        _ => T::default(),
    }
}

/// Atomic write primitive shared by all stores and by
/// `sillytavern::write_if_changed`: tmp file next to the target + fsync +
/// rename over the target. The tmp file is removed on every failure path.
/// fsync runs on a write-opened handle — `FlushFileBuffers` on Windows rejects
/// read-only ones (gotcha 25.09). Blocking IO on the caller's thread.
pub(crate) fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    let tmp = unique_tmp(path);
    let result = write_via_tmp(path, contents, &tmp);
    if result.is_err() {
        // Deterministic cleanup: a failed write must not leave a stray tmp file.
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Unique tmp name per call. A fixed one is truncated in place by a concurrent
/// writer before its own rename, and the interleaved bytes get renamed over the
/// target (the documented "truncated tmp" race). Callers still serialize
/// read-modify-write per file (store lock or module lock); this makes concurrent
/// whole-file writes safe on its own — `Settings::save`, for one, has no module
/// lock and is called from both the main thread and gateway handlers.
fn unique_tmp(path: &Path) -> std::path::PathBuf {
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}-{seq}.tmp", std::process::id()));
    std::path::PathBuf::from(name)
}

fn write_via_tmp(path: &Path, contents: &str, tmp: &Path) -> Result<(), String> {
    use std::io::Write;
    let mut file = std::fs::File::create(tmp).map_err(|e| e.to_string())?;
    file.write_all(contents.as_bytes()).map_err(|e| e.to_string())?;
    // Without fsync the rename can overtake the data on disk.
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    std::fs::rename(tmp, path).map_err(|e| e.to_string())
}

/// Serialize and persist a store value — the one implementation of the
/// "tmp + fsync + rename" save rule (V-31).
pub(crate) fn save_value<T: Serialize>(value: &T, path: &Path) -> Result<(), String> {
    crate::paths::ensure_dirs().map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    write_atomic(path, &text)
}

/// Change the store under its shared lock: read, change and write as one
/// operation. Refuses to write when the file exists but could not be read or
/// parsed — that state is an error, not an empty store (V-09). Blocking IO on
/// the caller's thread.
pub fn update<T, R>(
    guard: &'static Mutex<()>,
    path: &Path,
    change: impl FnOnce(&mut T) -> R,
) -> Result<R, String>
where
    T: Serialize + DeserializeOwned + Default,
{
    let _guard = lock(guard);
    let mut value = read_state(path)?.unwrap_or_default();
    let result = change(&mut value);
    save_value(&value, path)?;
    Ok(result)
}

/// Like `update`, but the file is written only when `change` returns `Ok` —
/// `update` always writes, so a refused change still created the file: a failed
/// `world/move` used to spawn `world.json` and raise `world::started()` forever.
/// Same V-09 rule as `update`: refuses to write over an existing file that
/// could not be read or parsed. Blocking IO on the caller's thread.
pub fn update_if_ok<T, R, E>(
    guard: &'static Mutex<()>,
    path: &Path,
    change: impl FnOnce(&mut T) -> Result<R, E>,
) -> Result<Result<R, E>, String>
where
    T: Serialize + DeserializeOwned + Default,
{
    let _guard = lock(guard);
    let mut value = read_state(path)?.unwrap_or_default();
    let result = change(&mut value);
    if result.is_ok() {
        save_value(&value, path)?;
    }
    Ok(result)
}

/// Remove a store file under its lock: otherwise an `update` that read the file
/// before the removal would write it back.
pub fn remove(guard: &'static Mutex<()>, path: &Path) -> Result<(), String> {
    let _guard = lock(guard);
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn temp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("cloufam_test_json_store");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(name)
    }

    #[test]
    fn update_persists_and_returns_change_result() {
        let path = temp_path("update_roundtrip.json");
        let _ = std::fs::remove_file(&path);
        let n: i32 = update(&TEST_LOCK, &path, |m: &mut BTreeMap<String, i32>| {
            m.insert("a".into(), 1);
            *m.get("a").unwrap()
        })
        .unwrap();
        assert_eq!(n, 1);
        let loaded: BTreeMap<String, i32> = load(&TEST_LOCK, &path);
        assert_eq!(loaded.get("a"), Some(&1));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let path = temp_path("does_not_exist.json");
        let _ = std::fs::remove_file(&path);
        let loaded: BTreeMap<String, i32> = load(&TEST_LOCK, &path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_corrupt_json_returns_default() {
        let path = temp_path("corrupt.json");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
        std::fs::write(&path, "not valid json {{{").unwrap();
        let loaded: BTreeMap<String, i32> = load(&TEST_LOCK, &path);
        assert!(loaded.is_empty());
        let backup = path.with_extension("json.corrupt");
        assert!(backup.exists(), "копия повреждённого файла не создана");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "not valid json {{{");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&backup);
    }

    /// save_value writes via fsync + rename — the file must read back correctly.
    #[test]
    fn save_roundtrips() {
        let path = temp_path("save_roundtrip.json");
        let _ = std::fs::remove_file(&path);
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), 7i32);
        save_value(&m, &path).unwrap();
        let loaded: BTreeMap<String, i32> = load(&TEST_LOCK, &path);
        assert_eq!(loaded.get("k"), Some(&7));
        let _ = std::fs::remove_file(&path);
    }

    /// Concurrent `update`s on one file do not lose another's edit — exactly the
    /// property the shared lock exists for.
    #[test]
    fn concurrent_updates_do_not_lose_writes() {
        let path = temp_path("concurrent.json");
        let _ = std::fs::remove_file(&path);
        static LOCK: Mutex<()> = Mutex::new(());
        const THREADS: usize = 20;
        let handles: Vec<_> = (0..THREADS)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    update(&LOCK, &path, |m: &mut BTreeMap<String, i32>| {
                        m.insert(format!("k{i}"), i as i32);
                    })
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let loaded: BTreeMap<String, i32> = load(&LOCK, &path);
        assert_eq!(loaded.len(), THREADS, "часть параллельных записей потеряна");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_if_ok_does_not_create_file_on_err() {
        let path = temp_path("update_if_ok_err.json");
        let _ = std::fs::remove_file(&path);
        let result = update_if_ok(&TEST_LOCK, &path, |m: &mut BTreeMap<String, i32>| {
            m.insert("a".into(), 1);
            Err::<(), _>("отказ")
        })
        .unwrap();
        assert_eq!(result, Err("отказ"));
        assert!(!path.exists(), "отказ в правке не должен создавать файл");
    }

    #[test]
    fn update_if_ok_keeps_existing_file_on_err_and_saves_on_ok() {
        let path = temp_path("update_if_ok_keep.json");
        let _ = std::fs::remove_file(&path);
        update(&TEST_LOCK, &path, |m: &mut BTreeMap<String, i32>| {
            m.insert("a".into(), 1);
        })
        .unwrap();
        let _ = update_if_ok(&TEST_LOCK, &path, |m: &mut BTreeMap<String, i32>| {
            m.insert("a".into(), 2);
            Err::<(), _>(())
        })
        .unwrap();
        let loaded: BTreeMap<String, i32> = load(&TEST_LOCK, &path);
        assert_eq!(loaded.get("a"), Some(&1), "отказ перезаписал хранилище");

        let ok = update_if_ok(&TEST_LOCK, &path, |m: &mut BTreeMap<String, i32>| {
            m.insert("a".into(), 3);
            Ok::<_, ()>(7)
        })
        .unwrap();
        assert_eq!(ok, Ok(7));
        let loaded: BTreeMap<String, i32> = load(&TEST_LOCK, &path);
        assert_eq!(loaded.get("a"), Some(&3));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_deletes_file_and_tolerates_missing() {
        let path = temp_path("remove.json");
        std::fs::write(&path, "{}").unwrap();
        remove(&TEST_LOCK, &path).unwrap();
        assert!(!path.exists());
        remove(&TEST_LOCK, &path).expect("отсутствующий файл — не ошибка");
    }

    /// V-09: a file that exists but cannot be read is an error, not an empty
    /// store — `update` must refuse and leave the bytes alone. Non-UTF-8
    /// content fails `read_to_string` with `InvalidData`, the same non-NotFound
    /// branch a Windows sharing violation (os error 32) or AccessDenied takes.
    #[test]
    fn update_refuses_when_file_is_not_utf8() {
        let path = temp_path("not_utf8.json");
        let raw = vec![0xff, 0xfe, 0x7b, 0x7d];
        std::fs::write(&path, &raw).unwrap();
        let result = update(&TEST_LOCK, &path, |m: &mut BTreeMap<String, i32>| {
            m.insert("a".into(), 1);
        });
        assert!(result.is_err(), "нечитаемый файл — ошибка, а не пустое хранилище");
        assert_eq!(std::fs::read(&path).unwrap(), raw, "файл перезаписан поверх неизвестного содержимого");
        let _ = std::fs::remove_file(&path);
    }

    /// V-09: a corrupt file must not be overwritten either — and its bytes are
    /// preserved as `.json.corrupt` before the refusal.
    #[test]
    fn update_refuses_on_corrupt_json_and_keeps_backup() {
        let path = temp_path("corrupt_update.json");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
        std::fs::write(&path, "not valid json {{{").unwrap();
        let result = update(&TEST_LOCK, &path, |m: &mut BTreeMap<String, i32>| {
            m.insert("a".into(), 1);
        });
        assert!(result.is_err(), "битый файл — ошибка, а не пустое хранилище");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "not valid json {{{",
            "исходник перезаписан"
        );
        let backup = path.with_extension("json.corrupt");
        assert!(backup.exists(), "копия повреждённого файла не создана");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "not valid json {{{");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&backup);
    }

    /// `update_if_ok` follows the same V-09 rule: the refusal is the outer
    /// `Err`, the file is untouched.
    #[test]
    fn update_if_ok_refuses_on_corrupt_json() {
        let path = temp_path("corrupt_update_if_ok.json");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
        std::fs::write(&path, "oops {{{").unwrap();
        let result: Result<Result<(), ()>, String> =
            update_if_ok(&TEST_LOCK, &path, |m: &mut BTreeMap<String, i32>| {
                m.insert("a".into(), 1);
                Ok(())
            });
        assert!(result.is_err(), "битый файл — ошибка, а не пустое хранилище");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "oops {{{");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
    }

    /// write_atomic: tmp + fsync + rename, and no tmp file outlives the call —
    /// success or failure (including a failed rename).
    #[test]
    fn write_atomic_roundtrips_and_leaves_no_tmp() {
        let path = temp_path("write_atomic.json");
        let _ = std::fs::remove_file(&path);
        write_atomic(&path, r#"{"k": 7}"#).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"k": 7}"#);
        assert!(tmp_leftovers(&path).is_empty(), "tmp-файл остался после успешной записи");

        // Target is a directory: the rename must fail and the tmp file must be
        // removed deterministically.
        let dir_target = temp_path("write_atomic_dir.json");
        let _ = std::fs::remove_file(&dir_target);
        let _ = std::fs::remove_dir(&dir_target);
        std::fs::create_dir(&dir_target).unwrap();
        assert!(write_atomic(&dir_target, "{}").is_err());
        assert!(tmp_leftovers(&dir_target).is_empty(), "tmp-файл остался после ошибки");
        let _ = std::fs::remove_dir(&dir_target);
        let _ = std::fs::remove_file(&path);
    }

    /// Tmp names are unique per call (`<name>.<pid>-<seq>.tmp`), so leftovers
    /// are found by prefix + `.tmp` suffix.
    fn tmp_leftovers(path: &std::path::Path) -> Vec<std::path::PathBuf> {
        let dir = path.parent().unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let n = p.file_name().unwrap().to_string_lossy().into_owned();
                n.starts_with(&name) && n.ends_with(".tmp")
            })
            .collect()
    }
}

