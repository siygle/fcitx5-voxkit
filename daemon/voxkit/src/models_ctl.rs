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
    if models::get(id).is_none() {
        return Err(("bad-model".into(), format!("unknown model '{id}'")));
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
            return Err((
                "busy".into(),
                "a download is already running".into(),
            ));
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

pub fn cancel() {
    cancel_flag().store(true, Ordering::SeqCst);
    let mut guard = job().lock().unwrap_or_else(|err| err.into_inner());
    if guard.state != "downloading" {
        guard.state = "idle".into();
        guard.message = "nothing to cancel".into();
    }
}

pub fn remove(store: &Path, id: &str) -> Result<(), (String, String)> {
    let spec = models::get(id)
        .ok_or_else(|| ("bad-model".into(), format!("unknown model '{id}'")))?;
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
        let dest = models::path_for(&store, spec);
        if dest.is_file() && file_ok(&dest, spec.bytes, spec.sha256)? {
            return Ok(());
        }
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
    let path = models::path_for(store, spec);
    let size = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    let installed = path.is_file() && size == spec.bytes;
    json!({
        "id": spec.id,
        "kind": match spec.kind {
            Kind::Asr => "asr",
            Kind::Vad => "vad",
        },
        "bytes": spec.bytes,
        "size_on_disk": if installed { size } else { 0 },
        "installed": installed,
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
    })
}

pub fn status_value(req: u64, store: &Path) -> Value {
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
