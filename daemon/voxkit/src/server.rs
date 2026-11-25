//! Unix-socket server.
//!
//! The accept loop is non-blocking so a stop flag can end it. Each connection
//! has one worker thread: that thread owns the microphone (`cpal::Stream` is
//! not `Send`). The reader thread only parses lines and can cancel an
//! in-flight run.
//!
//! Models are shared by all connections and loaded lazily: the ASR model
//! (and, for `mode: "translate"`, the translation model) starts loading when
//! `start` arrives, overlapping the recording, and is unloaded after the idle
//! timeout (`lifecycle`). A request holds a busy guard from `start` until its
//! text is delivered or it is cancelled, so its model is never unloaded
//! under it.

use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::Value;
use transcribe_cpp::CancelToken;

use crate::audio::MicCapture;
use crate::convert::maybe_convert;
use crate::engine::{Outcome, Transcriber};
use crate::lifecycle::{self, Busy, Lifecycle};
use crate::models::{self, Kind};
use crate::models_ctl;
use crate::protocol::{self, Command, Mode};
use crate::translate::{self, Translator};
use crate::vad::rms_dbfs;

/// Audio kept after the key is released (the last syllable is often still
/// being spoken). 120 ms measured enough for push-to-talk; was 200 ms.
const TAIL: Duration = Duration::from_millis(120);

struct Writer {
    inner: Mutex<BufWriter<UnixStream>>,
}

impl Writer {
    fn send(&self, value: &Value) -> bool {
        let Ok(mut line) = serde_json::to_string(value) else {
            return false;
        };
        line.push('\n');
        let mut guard = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        guard.write_all(line.as_bytes()).is_ok() && guard.flush().is_ok()
    }
}

struct Control {
    /// Bumped on start and on cancel. Stop commits only when this still equals
    /// the epoch captured as recording began.
    epoch: AtomicU64,
    token: CancelToken,
    /// Non-zero asks the translation plugin to stop.
    mt_cancel: AtomicI32,
}

/// Models and their use tracking, shared by every connection.
#[derive(Clone)]
pub struct Models {
    pub store: PathBuf,
    pub asr: Arc<Mutex<Transcriber>>,
    pub asr_life: Arc<Lifecycle>,
    pub mt: Arc<Mutex<Translator>>,
    pub mt_life: Arc<Lifecycle>,
}

impl Models {
    pub fn new(store: &Path) -> Self {
        Models {
            store: store.to_path_buf(),
            asr: Arc::new(Mutex::new(Transcriber::new(store.to_path_buf()))),
            asr_life: Lifecycle::new("speech", lifecycle::asr_idle()),
            mt: Arc::new(Mutex::new(Translator::new(store.to_path_buf()))),
            mt_life: Lifecycle::new("translation", lifecycle::mt_idle()),
        }
    }

    /// Start loading the ASR model in the background (key down).
    fn preload_asr(&self, model: &str) {
        let asr = Arc::clone(&self.asr);
        let busy = self.asr_life.busy();
        let model = model.to_string();
        thread::spawn(move || {
            let _busy = busy;
            let mut engine = asr.lock().unwrap_or_else(|e| e.into_inner());
            if engine.loaded_model() == Some(model.as_str()) {
                return;
            }
            let started = std::time::Instant::now();
            match engine.preload(&model) {
                Ok(()) => eprintln!(
                    "voxkit: preloaded {model} in {} ms",
                    started.elapsed().as_millis()
                ),
                Err(err) => eprintln!("voxkit: preload {model} failed ({err:#})"),
            }
        });
    }

    /// Start loading (or downloading) the translation model (key down).
    fn preload_mt(&self) {
        let mt = Arc::clone(&self.mt);
        let busy = self.mt_life.busy();
        thread::spawn(move || {
            let _busy = busy;
            let mut translator = mt.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(err) = translator.ensure() {
                eprintln!("voxkit: translation preload: {err}");
            }
        });
    }
}

struct Active {
    req: u64,
    epoch: u64,
    mode: Mode,
    target: String,
    capture: MicCapture,
    level_stop: Arc<AtomicBool>,
    level_thread: Option<JoinHandle<()>>,
    asr_busy: Option<Busy>,
    mt_busy: Option<Busy>,
}

impl Drop for Active {
    fn drop(&mut self) {
        self.level_stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.level_thread.take() {
            let _ = thread.join();
        }
    }
}

enum Job {
    Start {
        req: u64,
        language: String,
        vocabulary: Vec<String>,
        convert: String,
        model: Option<String>,
        mode: Mode,
        target: String,
    },
    Stop {
        req: u64,
    },
    Cancel {
        req: u64,
    },
    SetVocabulary {
        vocabulary: Vec<String>,
    },
    Shutdown,
}

pub fn serve(
    socket: &Path,
    store: &Path,
    default_model: &str,
    shutdown: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    models_ctl::init_selected(store, default_model);
    let shared = Models::new(store);
    eprintln!(
        "voxkit: models load on first use; idle unload: speech {}, translation {}",
        describe_idle(shared.asr_life.idle()),
        describe_idle(shared.mt_life.idle())
    );
    match translate::plugin_path() {
        Some(path) => eprintln!("voxkit: translation plugin available: {}", path.display()),
        None => eprintln!(
            "voxkit: translation plugin {} not found; translate requests fall back to the transcript",
            translate::PLUGIN_FILE
        ),
    }
    // Old behaviour (load and warm at startup) on request.
    if std::env::var("VOXKIT_PREWARM").is_ok_and(|v| v == "1") {
        let asr = Arc::clone(&shared.asr);
        let model = models_ctl::selected();
        let model = if model.is_empty() {
            default_model.to_string()
        } else {
            model
        };
        thread::spawn(move || {
            let started = std::time::Instant::now();
            let mut engine = asr.lock().unwrap_or_else(|e| e.into_inner());
            match engine.prewarm(&model) {
                Ok(()) => eprintln!(
                    "voxkit: {model} ready in {} ms",
                    started.elapsed().as_millis()
                ),
                Err(err) => eprintln!("voxkit: prewarm skipped ({err:#})"),
            }
        });
    }
    lifecycle::spawn_reaper(
        (Arc::clone(&shared.asr_life), Arc::clone(&shared.asr)),
        (Arc::clone(&shared.mt_life), Arc::clone(&shared.mt)),
        Arc::clone(&shutdown),
    );
    if socket.exists() {
        match UnixStream::connect(socket) {
            Ok(_) => anyhow::bail!("voxkit is already listening on {}", socket.display()),
            Err(_) => fs::remove_file(socket)?,
        }
    }
    if let Some(parent) = socket.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    eprintln!("voxkit: listening on {}", socket.display());
    let _guard = SocketFile(socket.to_path_buf());
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let default_model = default_model.to_string();
                let shared = shared.clone();
                thread::spawn(move || {
                    if let Err(err) = handle_client(stream, &default_model, shared) {
                        eprintln!("voxkit: client ended: {err:#}");
                    }
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn describe_idle(idle: Option<Duration>) -> String {
    match idle {
        Some(d) => format!("after {} s", d.as_secs()),
        None => "never".into(),
    }
}

struct SocketFile(PathBuf);

impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn handle_client(stream: UnixStream, default_model: &str, shared: Models) -> anyhow::Result<()> {
    stream.set_nonblocking(false)?;
    let store = shared.store.clone();
    let store = store.as_path();
    let writer = Arc::new(Writer {
        inner: Mutex::new(BufWriter::new(stream.try_clone()?)),
    });
    let control = Arc::new(Control {
        epoch: AtomicU64::new(0),
        token: CancelToken::new(),
        mt_cancel: AtomicI32::new(0),
    });
    let (tx, rx) = mpsc::channel();
    let worker = {
        let writer = Arc::clone(&writer);
        let control = Arc::clone(&control);
        let default_model = default_model.to_string();
        let shared = shared.clone();
        thread::spawn(move || worker_loop(rx, writer, control, default_model, shared))
    };

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match protocol::parse_command(trimmed) {
            Ok(Command::Ping { req }) => {
                if !writer.send(&protocol::pong(req)) {
                    break;
                }
            }
            Ok(Command::Start {
                req,
                language,
                vocabulary,
                convert,
                model,
                mode,
                target,
            }) => {
                if tx
                    .send(Job::Start {
                        req,
                        language,
                        vocabulary,
                        convert,
                        model,
                        mode,
                        target,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(Command::Stop { req }) => {
                if tx.send(Job::Stop { req }).is_err() {
                    break;
                }
            }
            Ok(Command::Cancel { req }) => {
                control.epoch.fetch_add(1, Ordering::SeqCst);
                control.token.cancel();
                control.mt_cancel.store(1, Ordering::SeqCst);
                if tx.send(Job::Cancel { req }).is_err() {
                    break;
                }
            }
            Ok(Command::SetVocabulary { vocabulary, .. }) => {
                if tx.send(Job::SetVocabulary { vocabulary }).is_err() {
                    break;
                }
            }
            Ok(Command::ModelList { req }) => {
                writer.send(&models_ctl::list_value(req, store));
            }
            Ok(Command::ModelStatus { req }) => {
                writer.send(&models_ctl::status_value(req, store));
            }
            Ok(Command::ModelProgress { req }) => {
                writer.send(&models_ctl::progress_value(req));
            }
            Ok(Command::ModelCancel { req }) => {
                models_ctl::cancel();
                writer.send(&models_ctl::progress_value(req));
            }
            Ok(Command::ModelInstall { req, model }) => {
                match models_ctl::begin(store.to_path_buf(), model) {
                    Ok(()) => {
                        writer.send(&models_ctl::progress_value(req));
                    }
                    Err((code, message)) => {
                        writer.send(&protocol::error(req, &code, &message));
                    }
                }
            }
            Ok(Command::ModelRemove { req, model }) => match models_ctl::remove(store, &model) {
                Ok(()) => {
                    writer.send(&serde_json::json!({
                        "v": 1,
                        "type": "model.remove",
                        "req": req,
                        "model": model,
                        "installed": false
                    }));
                }
                Err((code, message)) => {
                    writer.send(&protocol::error(req, &code, &message));
                }
            },
            Ok(Command::ModelSelect { req, model }) => match models_ctl::select(store, &model) {
                Ok(()) => {
                    writer.send(&serde_json::json!({
                        "v": 1,
                        "type": "model.select",
                        "req": req,
                        "model": model
                    }));
                }
                Err((code, message)) => {
                    writer.send(&protocol::error(req, &code, &message));
                }
            },
            Err(message) => {
                writer.send(&protocol::error(0, "bad-request", &message));
            }
        }
    }
    let _ = tx.send(Job::Shutdown);
    let _ = worker.join();
    Ok(())
}

fn worker_loop(
    rx: Receiver<Job>,
    writer: Arc<Writer>,
    control: Arc<Control>,
    default_model: String,
    shared: Models,
) {
    let mut vocabulary = Vec::new();
    let mut active: Option<Active> = None;
    let mut language = "zh".to_string();
    let mut convert = "none".to_string();
    let mut model = default_model;
    while let Ok(job) = rx.recv() {
        match job {
            Job::Shutdown => break,
            Job::SetVocabulary { vocabulary: terms } => vocabulary = terms,
            Job::Start {
                req,
                language: lang,
                vocabulary: terms,
                convert: conversion,
                model: requested,
                mode,
                target,
            } => {
                active = None;
                if !terms.is_empty() {
                    vocabulary = terms;
                }
                language = lang;
                convert = conversion;
                if let Some(id) = requested {
                    model = id;
                } else {
                    let chosen = models_ctl::selected();
                    if !chosen.is_empty() {
                        model = chosen;
                    }
                }
                control.mt_cancel.store(0, Ordering::SeqCst);
                let asr_busy = shared.asr_life.busy();
                let mt_busy = (mode == Mode::Translate).then(|| shared.mt_life.busy());
                eprintln!(
                    "voxkit: req {req} start mode={} target={}",
                    if mode == Mode::Translate {
                        "translate"
                    } else {
                        "dictate"
                    },
                    if mode == Mode::Translate {
                        target.as_str()
                    } else {
                        "-"
                    }
                );
                match begin_capture(&writer, &control, &shared.store, req, &model) {
                    Ok((epoch, capture, level_stop, level_thread)) => {
                        shared.preload_asr(&model);
                        if mode == Mode::Translate {
                            shared.preload_mt();
                        }
                        active = Some(Active {
                            req,
                            epoch,
                            mode,
                            target,
                            capture,
                            level_stop,
                            level_thread: Some(level_thread),
                            asr_busy: Some(asr_busy),
                            mt_busy,
                        });
                    }
                    Err((code, message)) => {
                        writer.send(&protocol::error(req, code, message));
                    }
                }
            }
            Job::Stop { req } => {
                let session = match active.take() {
                    Some(session) if session.req == req => session,
                    other => {
                        active = other;
                        writer.send(&protocol::error(
                            req,
                            "not-recording",
                            "stop without an active start",
                        ));
                        continue;
                    }
                };
                finish(
                    session,
                    &writer,
                    &control,
                    &shared,
                    &vocabulary,
                    &language,
                    &convert,
                    &model,
                );
            }
            Job::Cancel { req } => {
                if active.as_ref().is_some_and(|session| session.req == req) {
                    active = None;
                }
            }
        }
    }
}

type Capture = (u64, MicCapture, Arc<AtomicBool>, JoinHandle<()>);

fn begin_capture(
    writer: &Arc<Writer>,
    control: &Control,
    store: &Path,
    req: u64,
    model: &str,
) -> Result<Capture, (&'static str, &'static str)> {
    let spec = models::get(model)
        .filter(|spec| spec.kind == Kind::Asr)
        .ok_or(("bad-model", "unknown model id"))?;
    if models::locate(store, spec).is_none() {
        return Err(("model-not-ready", "run: voxkit model install"));
    }
    let epoch = control.epoch.fetch_add(1, Ordering::SeqCst) + 1;
    control.token.reset();
    let capture = MicCapture::start().map_err(|err| {
        eprintln!("voxkit: microphone: {err:#}");
        ("no-input", "no microphone")
    })?;
    writer.send(&protocol::ready(req));
    writer.send(&protocol::partial(req, "", "聆聽中"));
    let level_stop = Arc::new(AtomicBool::new(false));
    let level_thread = spawn_levels(
        capture.samples_handle(),
        Arc::clone(&level_stop),
        Arc::clone(writer),
        req,
    );
    Ok((epoch, capture, level_stop, level_thread))
}

fn spawn_levels(
    samples: Arc<Mutex<Vec<f32>>>,
    stop: Arc<AtomicBool>,
    writer: Arc<Writer>,
    req: u64,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let guard = samples.lock().unwrap_or_else(|err| err.into_inner());
            let start = guard.len().saturating_sub(1600);
            let rms = rms_dbfs(&guard[start..]);
            drop(guard);
            if !writer.send(&protocol::level(req, rms)) {
                break;
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn finish(
    mut session: Active,
    writer: &Writer,
    control: &Control,
    shared: &Models,
    vocabulary: &[String],
    language: &str,
    convert: &str,
    model: &str,
) {
    // Keep the models pinned until the text is out.
    let _asr_busy = session.asr_busy.take();
    let _mt_busy = session.mt_busy.take();
    let epoch = session.epoch;
    let req = session.req;
    let mode = session.mode;
    let target = std::mem::take(&mut session.target);
    let steps = (TAIL.as_millis() / 20) as u32;
    for _ in 0..steps {
        if control.epoch.load(Ordering::SeqCst) != epoch {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    if control.epoch.load(Ordering::SeqCst) != epoch {
        return;
    }
    session.level_stop.store(true, Ordering::SeqCst);
    if let Some(thread) = session.level_thread.take() {
        let _ = thread.join();
    }
    let (samples, rate) = session.capture.finish();
    drop(session);
    if control.epoch.load(Ordering::SeqCst) != epoch {
        return;
    }
    writer.send(&protocol::partial(req, "", "辨識中"));
    // Translation needs the raw transcript; conversion happens afterwards.
    let asr_convert = if mode == Mode::Translate {
        "none"
    } else {
        convert
    };
    let outcome = {
        let mut engine = shared.asr.lock().unwrap_or_else(|e| e.into_inner());
        engine.transcribe(
            &samples,
            rate,
            model,
            language,
            vocabulary,
            asr_convert,
            &control.token,
        )
    };
    if control.epoch.load(Ordering::SeqCst) != epoch || control.token.is_cancelled() {
        return;
    }
    match outcome {
        Ok(Outcome::Text(text)) if mode == Mode::Translate => {
            writer.send(&protocol::partial(req, "", "翻譯中"));
            let result = {
                let mut translator = shared.mt.lock().unwrap_or_else(|e| e.into_inner());
                translate::run(
                    &mut translator,
                    &text,
                    language,
                    convert,
                    &target,
                    &control.mt_cancel,
                )
            };
            if control.epoch.load(Ordering::SeqCst) != epoch
                || control.mt_cancel.load(Ordering::SeqCst) != 0
            {
                return;
            }
            let code = translate::target(&target)
                .map(|t| t.code)
                .unwrap_or(target.as_str());
            let source = maybe_convert(&text, convert);
            let error = result.error.as_ref().map(|e| e.code());
            eprintln!(
                "voxkit: req {req} translate target={code} translated={} error={}",
                result.translated,
                result
                    .error
                    .as_ref()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "-".into())
            );
            writer.send(&protocol::final_translation(
                req,
                &result.text,
                code,
                result.translated,
                &source,
                error,
            ));
        }
        Ok(Outcome::Text(text)) => {
            writer.send(&protocol::final_text(req, &text));
        }
        Ok(Outcome::NoSpeech) => {
            writer.send(&protocol::discarded(req, "no_speech"));
        }
        Ok(Outcome::Cancelled) => {}
        Err(err) => {
            eprintln!("voxkit: {err:#}");
            let message = err.to_string();
            let code = if message.contains("not installed") {
                "model-not-ready"
            } else {
                "transcribe-failed"
            };
            writer.send(&protocol::error(req, code, &message));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_roundtrip() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket = std::env::temp_dir().join(format!("voxkit-ping-{stamp}.sock"));
        let store = std::env::temp_dir().join(format!("voxkit-store-{stamp}"));
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_flag = Arc::clone(&shutdown);
        let socket_for_server = socket.clone();
        let server =
            thread::spawn(move || serve(&socket_for_server, &store, "whisper-tiny", shutdown_flag));
        let mut connected = None;
        for _ in 0..100 {
            if let Ok(stream) = UnixStream::connect(&socket) {
                connected = Some(stream);
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let mut stream = connected.expect("server socket");
        stream
            .write_all(b"{\"v\":1,\"type\":\"ping\",\"req\":4}\n")
            .unwrap();
        stream.flush().unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        drop(reader);
        shutdown.store(true, Ordering::SeqCst);
        let value: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(value["type"], "pong");
        assert_eq!(value["req"], 4);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn model_status_advertises_translate() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket = std::env::temp_dir().join(format!("voxkit-caps-{stamp}.sock"));
        let store = std::env::temp_dir().join(format!("voxkit-caps-store-{stamp}"));
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_flag = Arc::clone(&shutdown);
        let socket_for_server = socket.clone();
        let server =
            thread::spawn(move || serve(&socket_for_server, &store, "whisper-tiny", shutdown_flag));
        let mut connected = None;
        for _ in 0..100 {
            if let Ok(stream) = UnixStream::connect(&socket) {
                connected = Some(stream);
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let mut stream = connected.expect("server socket");
        stream
            .write_all(b"{\"v\":1,\"type\":\"model.status\",\"req\":8}\n")
            .unwrap();
        stream.flush().unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        drop(reader);
        shutdown.store(true, Ordering::SeqCst);
        let value: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(value["type"], "model.status");
        assert_eq!(value["req"], 8);
        assert!(value["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "translate"));
        assert_eq!(value["translate"]["model"], "hy-mt1.5-1.8b");
        assert_eq!(value["translate"]["installed"], false);
        server.join().unwrap().unwrap();
    }
}
