//! Socket-facing model catalog, download, and the selected ASR id.
//!
//! The CLI `voxkit model install` is unchanged. This module is what the
//! version-1 `model.*` commands call, so every frontend shares one download.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::anyhow;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::models::{self, Kind};

struct Job {
    state: String,
    model: String,
    received: u64,
    bytes: u64,
    message: String,
}

fn job() -> &'static Mutex<Job> {
    static JOB: OnceLock<Mutex<Job>> = OnceLock::new();
    JOB.get_or_init(|| {
        Mutex::new(Job {
            state: "idle".into(),
            model: String::new(),
            received: 0,
            bytes: 0,
            message: String::new(),
        })
    })
}

fn cancel_flag() -> &'static AtomicBool {
    static CANCEL: AtomicBool = AtomicBool::new(false);
    &CANCEL
}

static SELECTED: OnceLock<Mutex<String>> = OnceLock::new();

pub fn init_selected(store: &Path, fallback: &str) {
    let from_disk = fs::read_to_string(store.join("selected.txt"))
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|id| models::get(id).is_some());
    let id = from_disk.unwrap_or_else(|| fallback.to_string());
    let _ = SELECTED.set(Mutex::new(id));
}

pub fn selected() -> String {
    SELECTED
        .get()
        .and_then(|slot| slot.lock().ok())
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

pub fn select(store: &Path, id: &str) -> Result<(), (String, String)> {
    match models::get(id) {
        None => return Err(("bad-model".into(), format!("unknown model '{id}'"))),
        Some(spec) if spec.kind != Kind::Asr => {
            return Err(("bad-model".into(), format!("'{id}' is not a speech model")))
        }
        Some(_) => {}
    }
    fs::create_dir_all(store).map_err(|err| ("io".into(), err.to_string()))?;
    fs::write(store.join("selected.txt"), format!("{id}\n"))
        .map_err(|err| ("io".into(), err.to_string()))?;
    if let Some(slot) = SELECTED.get() {
        if let Ok(mut guard) = slot.lock() {
            *guard = id.to_string();
        }
    }
    Ok(())
}

pub fn begin(store: PathBuf, id: String) -> Result<(), (String, String)> {
    if models::get(&id).is_none() {
        return Err(("bad-model".into(), format!("unknown model '{id}'")));
    }
    {
        let guard = job().lock().unwrap_or_else(|err| err.into_inner());
        if guard.state == "downloading" {
            return Err(("busy".into(), "a download is already running".into()));
        }
    }
    cancel_flag().store(false, Ordering::SeqCst);
    {
        let spec = models::get(&id).expect("checked");
        let mut guard = job().lock().unwrap_or_else(|err| err.into_inner());
        guard.state = "downloading".into();
        guard.model = id.clone();
        guard.received = 0;
        guard.bytes = spec.bytes;
        guard.message.clear();
    }
    thread::spawn(move || run_download(store, id));
    Ok(())
}

/// True while `id` is being downloaded.
pub fn downloading(id: &str) -> bool {
    let guard = job().lock().unwrap_or_else(|err| err.into_inner());
    guard.state == "downloading" && guard.model == id
}

pub fn cancel() {
    cancel_flag().store(true, Ordering::SeqCst);
    let mut guard = job().lock().unwrap_or_else(|err| err.into_inner());
    if guard.state != "downloading" {
        guard.state = "idle".into();
        guard.message = "nothing to cancel".into();
    }
}

pub fn remove(store: &Path, id: &str) -> Result<(), (String, String)> {
    let spec =
        models::get(id).ok_or_else(|| ("bad-model".into(), format!("unknown model '{id}'")))?;
    {
        let guard = job().lock().unwrap_or_else(|err| err.into_inner());
        if guard.state == "downloading" && guard.model == id {
            return Err((
                "busy".into(),
                "cancel the download before deleting it".into(),
            ));
        }
    }
    let dest = models::path_for(store, spec);
    if !dest.is_file() {
        if let Some(models::Location::Borrowed(path)) = models::locate(store, spec) {
            return Err((
                "read-only".into(),
                format!(
                    "{id} is provided read-only by {}; voxkit does not delete it",
                    path.display()
                ),
            ));
        }
    }
    if dest.is_file() {
        fs::remove_file(&dest).map_err(|err| ("io".into(), err.to_string()))?;
    }
    let partial = store.join(format!("{}.partial", spec.filename));
    if partial.is_file() {
        let _ = fs::remove_file(partial);
    }
    Ok(())
}

fn run_download(store: PathBuf, id: String) {
    let outcome = (|| -> Result<(), String> {
        let spec = models::get(&id).ok_or_else(|| format!("unknown model '{id}'"))?;
        fs::create_dir_all(&store).map_err(|err| err.to_string())?;
        if let Some(found) = models::locate(&store, spec) {
            if file_ok(found.path(), spec.bytes, spec.sha256)? {
                return Ok(()); // already usable (owned or read-only)
            }
        }
        let dest = models::path_for(&store, spec);
        let partial = store.join(format!("{}.partial", spec.filename));
        download(&spec.url, &partial, spec.bytes)?;
        if cancel_flag().load(Ordering::SeqCst) {
            let _ = fs::remove_file(&partial);
            return Err("cancelled".into());
        }
        if !file_ok(&partial, spec.bytes, spec.sha256)? {
            let _ = fs::remove_file(&partial);
            return Err(format!("checksum mismatch for {id}"));
        }
        fs::rename(&partial, &dest).map_err(|err| err.to_string())?;
        Ok(())
    })();
    let mut guard = job().lock().unwrap_or_else(|err| err.into_inner());
    match outcome {
        Ok(()) => {
            guard.state = "done".into();
            guard.received = guard.bytes;
            guard.message.clear();
        }
        Err(message) if message == "cancelled" || cancel_flag().load(Ordering::SeqCst) => {
            guard.state = "cancelled".into();
            guard.message = message;
        }
        Err(message) => {
            guard.state = "error".into();
            guard.message = message;
        }
    }
}

fn file_ok(path: &Path, bytes: u64, sha256: &str) -> Result<bool, String> {
    let size = fs::metadata(path).map_err(|err| err.to_string())?.len();
    if size != bytes {
        return Ok(false);
    }
    Ok(sha256_file(path)?.eq_ignore_ascii_case(sha256))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|err| err.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|err| err.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn download(url: &str, dest: &Path, expected: u64) -> Result<(), String> {
    if dest.exists() {
        let _ = fs::remove_file(dest);
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(600))
        .redirects(10)
        .build();
    let response = agent
        .get(url)
        .set("User-Agent", "voxkit/0.1")
        .call()
        .map_err(|err| anyhow!("download {url}: {err}").to_string())?;
    let mut reader = response.into_reader();
    let mut file = File::create(dest).map_err(|err| err.to_string())?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut got = 0u64;
    loop {
        if cancel_flag().load(Ordering::SeqCst) {
            drop(file);
            let _ = fs::remove_file(dest);
            return Err("cancelled".into());
        }
        let n = reader.read(&mut buf).map_err(|err| err.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|err| err.to_string())?;
        got += n as u64;
        if let Ok(mut guard) = job().lock() {
            guard.received = got;
            guard.bytes = expected;
        }
    }
    file.flush().map_err(|err| err.to_string())?;
    Ok(())
}

fn directory_bytes(store: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(store) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum()
}

fn row(store: &Path, spec: &models::ModelSpec, active: &str) -> Value {
    let found = models::locate(store, spec);
    let size = found
        .as_ref()
        .and_then(|f| fs::metadata(f.path()).ok())
        .map(|meta| meta.len())
        .unwrap_or(0);
    let installed = found.is_some() && size == spec.bytes;
    let source = match &found {
        Some(models::Location::Owned(_)) => "store",
        Some(models::Location::Borrowed(_)) => "read-only",
        None => "",
    };
    // Disk use counts only files voxkit owns.
    let owned_size = match &found {
        Some(models::Location::Owned(_)) => size,
        _ => 0,
    };
    json!({
        "id": spec.id,
        "kind": spec.kind.as_str(),
        "bytes": spec.bytes,
        "size_on_disk": owned_size,
        "installed": installed,
        "source": source,
        "removable": source == "store",
        "active": spec.id == active,
        "blurb": spec.blurb,
    })
}

pub fn list_value(req: u64, store: &Path) -> Value {
    let active = selected();
    let models: Vec<Value> = models::specs()
        .iter()
        .map(|spec| row(store, spec, &active))
        .collect();
    json!({
        "v": 1,
        "type": "model.list",
        "req": req,
        "daemon": "running",
        "state": job_state(),
        "store": store.display().to_string(),
        "store_bytes": directory_bytes(store),
        "model": active,
        "models": models,
        "capabilities": capabilities(),
    })
}

pub fn status_value(req: u64, store: &Path) -> Value {
    // Before taking the job lock: translate_status() reads it too.
    let translate = translate_status(store);
    let guard = job().lock().unwrap_or_else(|err| err.into_inner());
    json!({
        "v": 1,
        "type": "model.status",
        "req": req,
        "daemon": "running",
        "state": guard.state,
        "store": store.display().to_string(),
        "store_bytes": directory_bytes(store),
        "model": selected(),
        "received": guard.received,
        "bytes": guard.bytes,
        "message": guard.message,
        "capabilities": capabilities(),
        "translate": translate,
    })
}

/// Features this daemon understands. Frontends must see "translate" here
/// before they send `start` with `mode: "translate"`: an older daemon would
/// ignore the unknown fields and silently dictate instead.
pub fn capabilities() -> Value {
    json!(["translate"])
}

/// Whether translation can run now (plugin found, model installed). A
/// missing piece is not fatal: the request falls back to the transcript.
pub fn translate_status(store: &Path) -> Value {
    let id = std::env::var("VOXKIT_MT_MODEL")
        .ok()
        .filter(|id| models::get(id).is_some_and(|s| s.kind == Kind::Mt))
        .unwrap_or_else(|| crate::translate::DEFAULT_MT_MODEL.to_string());
    let installed = models::get(&id)
        .and_then(|spec| models::locate(store, spec))
        .is_some();
    let plugin = crate::translate::plugin_path();
    json!({
        "plugin": plugin.is_some(),
        "plugin_path": plugin.map(|p| p.display().to_string()).unwrap_or_default(),
        "model": id,
        "installed": installed,
        "downloading": downloading(&id),
        "targets": crate::translate::target_codes(),
    })
}

pub fn progress_value(req: u64) -> Value {
    let guard = job().lock().unwrap_or_else(|err| err.into_inner());
    json!({
        "v": 1,
        "type": "model.progress",
        "req": req,
        "daemon": "running",
        "state": guard.state,
        "model": guard.model,
        "received": guard.received,
        "bytes": guard.bytes,
        "message": guard.message,
    })
}

fn job_state() -> String {
    job()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .state
        .clone()
}

#[cfg(test)]
mod tests {
    //! A read-only model directory (e.g. Hex's) must never be written to:
    //! not by remove, select, list, or a download that finds a usable copy.
    use super::*;
    use std::collections::BTreeMap;

    /// Tiny fake spec matching a real catalog entry is not possible (sizes are
    /// pinned), so these tests drive the code paths with real specs and
    /// sparse files of the exact size.
    fn snapshot(dir: &Path) -> BTreeMap<String, (u64, std::time::SystemTime)> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| {
                let m = e.metadata().unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    (m.len(), m.modified().unwrap()),
                )
            })
            .collect()
    }

    // One test: VOXKIT_READONLY_MODELS_DIRS is process-global.
    #[test]
    fn readonly_dir_is_never_modified() {
        let root = std::env::temp_dir().join(format!("voxkit-ro-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = root.join("store");
        let hex = root.join("hex");
        fs::create_dir_all(&hex).unwrap();
        let spec = models::get("silero-vad").unwrap();
        // Same name and size as the real file (content does not matter here).
        let borrowed = hex.join(spec.filename);
        File::create(&borrowed)
            .unwrap()
            .set_len(spec.bytes)
            .unwrap();
        std::env::set_var("VOXKIT_READONLY_MODELS_DIRS", &hex);
        let before = snapshot(&hex);

        // Located as a read-only copy, reported as such, not removable.
        assert_eq!(
            models::locate(&store, spec),
            Some(models::Location::Borrowed(borrowed.clone()))
        );
        let list = list_value(1, &store);
        let row = list["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == spec.id)
            .unwrap()
            .clone();
        assert_eq!(row["installed"], true);
        assert_eq!(row["source"], "read-only");
        assert_eq!(row["removable"], false);
        assert_eq!(row["size_on_disk"], 0);

        // Remove refuses and leaves the file alone.
        let err = remove(&store, spec.id).unwrap_err();
        assert_eq!(err.0, "read-only");
        assert!(borrowed.is_file());

        // Select writes only into the store.
        init_selected(&store, "qwen3-asr-0.6b");
        select(&store, "qwen3-asr-0.6b").unwrap();
        assert!(store.join("selected.txt").is_file());

        // An owned copy shadows the read-only one; removing deletes only it.
        fs::create_dir_all(&store).unwrap();
        let owned = models::path_for(&store, spec);
        File::create(&owned).unwrap().set_len(spec.bytes).unwrap();
        assert_eq!(
            models::locate(&store, spec),
            Some(models::Location::Owned(owned.clone()))
        );
        remove(&store, spec.id).unwrap();
        assert!(!owned.exists());
        assert!(borrowed.is_file());

        assert_eq!(snapshot(&hex), before, "read-only dir changed");
        std::env::remove_var("VOXKIT_READONLY_MODELS_DIRS");
        let _ = fs::remove_dir_all(&root);
    }
}
