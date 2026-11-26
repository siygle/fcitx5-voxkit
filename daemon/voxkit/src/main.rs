use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Context};

use voxkit::audio::load_wav_16k;
use voxkit::engine::{Outcome, Transcriber};
use voxkit::models;
use voxkit::models_ctl;
use voxkit::server;
use voxkit::vocab;
use voxkit::{default_socket, default_store};

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("voxkit: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" || args[0] == "help" {
        print_help();
        return Ok(ExitCode::SUCCESS);
    }
    let cmd = args.remove(0);
    let flags = Flags::parse(&args)?;
    if cmd != "serve" {
        // The CLI honours the same selected speech / translation model.
        models_ctl::init_selected(&flags.store, &flags.model);
    }
    match cmd.as_str() {
        "serve" => {
            let shutdown = Arc::new(AtomicBool::new(false));
            server::serve(&flags.socket, &flags.store, &flags.model, shutdown)?;
            Ok(ExitCode::SUCCESS)
        }
        "model" => model_command(&flags),
        "transcribe" => transcribe_command(&flags),
        "translate" => translate_command(&flags),
        other => Err(anyhow!("unknown command '{other}'. {HELP}")),
    }
}

fn model_command(flags: &Flags) -> anyhow::Result<ExitCode> {
    let sub = flags
        .positionals
        .first()
        .map(String::as_str)
        .unwrap_or("list");
    let id = flags.positionals.get(1).map(String::as_str);
    let store = &flags.store;
    if flags.json {
        return Ok(model_json(flags, sub, id));
    }
    match sub {
        "list" => {
            print_models(store);
            Ok(ExitCode::SUCCESS)
        }
        "install" => {
            if flags.accept_license {
                if let Some(id) = id {
                    models::accept_license(store, id)?;
                }
            }
            models::install_requested(store, id)?;
            Ok(ExitCode::SUCCESS)
        }
        "remove" => {
            let id = id.ok_or_else(|| anyhow!("usage: voxkit model remove ID"))?;
            refuse_while_serving(flags).map_err(|(_, m)| anyhow!(m))?;
            models_ctl::remove(store, id).map_err(|(_, m)| anyhow!(m))?;
            eprintln!("voxkit: removed {id}");
            Ok(ExitCode::SUCCESS)
        }
        "verify" => {
            let id = id.ok_or_else(|| anyhow!("usage: voxkit model verify ID"))?;
            let result = models::verify(store, id)?;
            println!("{id}: {}", models_ctl::verify_word(&result));
            Ok(if matches!(result, models::Verified::Ok(_)) {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        "select" => {
            let id = id.ok_or_else(|| anyhow!("usage: voxkit model select ID"))?;
            models_ctl::select(store, id).map_err(|(_, m)| anyhow!(m))?;
            eprintln!("voxkit: selected {id} (a running daemon picks it up after a restart)");
            Ok(ExitCode::SUCCESS)
        }
        other => Err(anyhow!("unknown model command '{other}'")),
    }
}

/// Deleting a file a running daemon may have loaded is the daemon's job
/// (`model.remove` unloads it first).
fn refuse_while_serving(flags: &Flags) -> Result<(), (String, String)> {
    if std::os::unix::net::UnixStream::connect(&flags.socket).is_ok() {
        return Err((
            "daemon-running".into(),
            format!(
                "voxkit is running on {}; remove the model through it (model.remove)",
                flags.socket.display()
            ),
        ));
    }
    Ok(())
}

fn print_json(value: &serde_json::Value) {
    println!("{value}");
}

fn json_error(code: &str, message: &str) -> ExitCode {
    print_json(&serde_json::json!({
        "v": 1, "type": "error", "req": 0, "daemon": "offline",
        "code": code, "message": message,
    }));
    ExitCode::from(1)
}

/// Offline `--json` model commands: the same shapes as the socket replies
/// (`daemon: "offline"`), one JSON object per line on stdout. For the
/// settings windows when the daemon is not running.
fn model_json(flags: &Flags, sub: &str, id: Option<&str>) -> ExitCode {
    let store = &flags.store;
    let need_id = |id: Option<&str>| -> Result<&'static models::ModelSpec, ExitCode> {
        let Some(id) = id else {
            return Err(json_error("bad-request", "model id missing"));
        };
        models::get(id).ok_or_else(|| json_error("bad-model", &format!("unknown model '{id}'")))
    };
    match sub {
        "list" => {
            print_json(&models_ctl::list_value_for(0, store, "offline"));
            ExitCode::SUCCESS
        }
        "install" => {
            let spec = match need_id(id) {
                Ok(spec) => spec,
                Err(code) => return code,
            };
            if flags.accept_license {
                if let Err(err) = models::accept_license(store, spec.id) {
                    return json_error("io", &err.to_string());
                }
            }
            if !models::license_accepted(store, spec) {
                return json_error(
                    "license-required",
                    &format!(
                        "{}: accept the license first. {}",
                        spec.id, spec.restriction
                    ),
                );
            }
            if models::locate(store, spec).is_none() {
                if let Err((code, message)) = models::ensure_space(store, spec) {
                    return json_error(&code, &message);
                }
            }
            let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
            let mut report = |model: &str, received: u64, bytes: u64| {
                if last.elapsed() >= std::time::Duration::from_millis(250) || received == bytes {
                    last = std::time::Instant::now();
                    print_json(&serde_json::json!({
                        "v": 1, "type": "model.progress", "req": 0, "daemon": "offline",
                        "state": "downloading", "model": model,
                        "received": received, "bytes": bytes, "message": "",
                    }));
                }
            };
            match models::install_requested_with(store, Some(spec.id), &mut report) {
                Ok(()) => {
                    print_json(&serde_json::json!({
                        "v": 1, "type": "model.progress", "req": 0, "daemon": "offline",
                        "state": "done", "model": spec.id,
                        "received": spec.bytes, "bytes": spec.bytes, "message": "",
                    }));
                    ExitCode::SUCCESS
                }
                Err(err) => json_error("download-failed", &format!("{err:#}")),
            }
        }
        "remove" => {
            let spec = match need_id(id) {
                Ok(spec) => spec,
                Err(code) => return code,
            };
            if let Err((code, message)) = refuse_while_serving(flags) {
                return json_error(&code, &message);
            }
            match models_ctl::remove(store, spec.id) {
                Ok(()) => {
                    print_json(&serde_json::json!({
                        "v": 1, "type": "model.remove", "req": 0, "daemon": "offline",
                        "model": spec.id, "installed": false,
                    }));
                    ExitCode::SUCCESS
                }
                Err((code, message)) => json_error(&code, &message),
            }
        }
        "verify" => {
            let spec = match need_id(id) {
                Ok(spec) => spec,
                Err(code) => return code,
            };
            match models::verify(store, spec.id) {
                Ok(result) => {
                    let word = models_ctl::verify_word(&result);
                    print_json(&serde_json::json!({
                        "v": 1, "type": "model.verify", "req": 0, "daemon": "offline",
                        "model": spec.id, "result": word,
                    }));
                    if word == "ok" {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::from(1)
                    }
                }
                Err(err) => json_error("io", &format!("{err:#}")),
            }
        }
        "select" => {
            let spec = match need_id(id) {
                Ok(spec) => spec,
                Err(code) => return code,
            };
            match models_ctl::select(store, spec.id) {
                Ok(()) => {
                    print_json(&serde_json::json!({
                        "v": 1, "type": "model.select", "req": 0, "daemon": "offline",
                        "model": spec.id,
                    }));
                    ExitCode::SUCCESS
                }
                Err((code, message)) => json_error(&code, &message),
            }
        }
        other => json_error("bad-request", &format!("unknown model command '{other}'")),
    }
}

fn print_models(store: &std::path::Path) {
    println!("store: {}", store.display());
    for dir in models::readonly_dirs() {
        println!("read-only: {}", dir.display());
    }
    for spec in models::specs() {
        let mark = match models::locate(store, spec) {
            Some(models::Location::Owned(_)) => "installed",
            Some(models::Location::Borrowed(_)) => "read-only",
            None => "not installed",
        };
        let kind = spec.kind.as_str();
        println!("{:<16} {:<13} {:<12} {}", spec.id, kind, mark, spec.blurb);
        println!("  {}", spec.license);
    }
}

fn transcribe_command(flags: &Flags) -> anyhow::Result<ExitCode> {
    let path = flags
        .positionals
        .first()
        .ok_or_else(|| anyhow!("usage: voxkit transcribe FILE.wav"))?;
    let wav = load_wav_16k(&PathBuf::from(path)).with_context(|| format!("read {path}"))?;
    let terms = match &flags.vocab_dir {
        Some(dir) => vocab::load_user_dir(dir),
        None => Vec::new(),
    };
    let started = Instant::now();
    let mut engine = Transcriber::new(&flags.store);
    let model = voxkit::models::resolve_asr(&flags.store, &flags.model);
    let outcome = engine.transcribe(
        &wav,
        voxkit::audio::TARGET_RATE,
        &model,
        &flags.language,
        &terms,
        if flags.translate.is_some() {
            "none"
        } else {
            &flags.convert
        },
        &transcribe_cpp::CancelToken::new(),
    )?;
    match outcome {
        Outcome::Text(text) if flags.translate.is_some() => {
            eprintln!(
                "voxkit: {} in {:.1}s: {text}",
                flags.model,
                started.elapsed().as_secs_f32()
            );
            let target = flags.translate.as_deref().unwrap_or("en");
            Ok(print_translation(flags, &text, target))
        }
        Outcome::Text(text) => {
            eprintln!(
                "voxkit: {} in {:.1}s, {} chars",
                flags.model,
                started.elapsed().as_secs_f32(),
                text.chars().count()
            );
            println!("{text}");
            Ok(ExitCode::SUCCESS)
        }
        Outcome::NoSpeech => {
            eprintln!("voxkit: no speech");
            Ok(ExitCode::from(2))
        }
        Outcome::Cancelled => {
            eprintln!("voxkit: cancelled");
            Ok(ExitCode::from(2))
        }
    }
}

/// `voxkit translate TEXT`: run the translation stage alone (no microphone).
/// Exit 0 when translated, 3 when it fell back to the original text.
fn translate_command(flags: &Flags) -> anyhow::Result<ExitCode> {
    let text = flags.positionals.join(" ");
    if text.trim().is_empty() {
        return Err(anyhow!("usage: voxkit translate [--target en] TEXT"));
    }
    let target = flags.translate.as_deref().unwrap_or("en");
    Ok(print_translation(flags, &text, target))
}

fn print_translation(flags: &Flags, text: &str, target: &str) -> ExitCode {
    let mut translator = voxkit::translate::Translator::new(&flags.store);
    let cancel = std::sync::atomic::AtomicI32::new(0);
    if !flags.wait && !voxkit::models::is_installed(&flags.store, translator.model_id()) {
        let id = translator.model_id();
        eprintln!("voxkit: translation model {id} is not installed; leaving the text unchanged");
        eprintln!("voxkit: run `voxkit model install {id}` or pass --wait to download first");
        println!("{}", voxkit::convert::maybe_convert(text, &flags.convert));
        return ExitCode::from(3);
    }
    if flags.accept_license {
        if let Err(err) = voxkit::models::accept_license(&flags.store, translator.model_id()) {
            eprintln!("voxkit: could not record the license acceptance: {err}");
        }
    }
    if flags.wait {
        loop {
            match translator.ensure() {
                Err(voxkit::translate::Failure::LicenseRequired) => {
                    eprintln!(
                        "voxkit: {} has a restricted license; pass --accept-license to download it",
                        translator.model_id()
                    );
                    break;
                }
                Err(voxkit::translate::Failure::ModelDownloading) => {
                    eprintln!(
                        "voxkit: waiting for the translation model download (or run `voxkit model install hy-mt1.5-1.8b`)"
                    );
                    if !voxkit::models_ctl::downloading(translator.model_id()) {
                        break;
                    }
                    while voxkit::models_ctl::downloading(translator.model_id()) {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                }
                _ => break,
            }
        }
    }
    let started = Instant::now();
    let out = voxkit::translate::run(
        &mut translator,
        text,
        &flags.language,
        &flags.convert,
        target,
        &cancel,
    );
    eprintln!(
        "voxkit: translate -> {target} in {:.1}s, translated={} error={}",
        started.elapsed().as_secs_f32(),
        out.translated,
        out.error
            .as_ref()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "-".into())
    );
    println!("{}", out.text);
    if out.translated {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(3)
    }
}

struct Flags {
    store: PathBuf,
    socket: PathBuf,
    model: String,
    language: String,
    convert: String,
    vocab_dir: Option<PathBuf>,
    translate: Option<String>,
    /// For `translate` / `transcribe --translate`: wait for a missing
    /// model to download before running. Default is to fall back (exit 3)
    /// like the IME, and tell the user to run `voxkit model install`.
    wait: bool,
    /// Accept a restricted model's license (install, translate --wait).
    accept_license: bool,
    /// Machine-readable output for the model commands.
    json: bool,
    positionals: Vec<String>,
}

impl Flags {
    fn parse(args: &[String]) -> anyhow::Result<Self> {
        let mut store = default_store();
        let mut socket = default_socket();
        let mut model =
            env::var("VOXKIT_MODEL").unwrap_or_else(|_| models::default_asr_id().to_string());
        let mut language = "zh".to_string();
        let mut convert = "none".to_string();
        let mut vocab_dir = None;
        let mut translate = None;
        let mut wait = false;
        let mut accept_license = false;
        let mut json = false;
        let mut positionals = Vec::new();
        let mut index = 0;
        while index < args.len() {
            let arg = &args[index];
            let value = |index: &mut usize| -> anyhow::Result<String> {
                *index += 1;
                args.get(*index)
                    .cloned()
                    .ok_or_else(|| anyhow!("{arg} needs a value"))
            };
            match arg.as_str() {
                "--store" => store = PathBuf::from(value(&mut index)?),
                "--socket" => socket = PathBuf::from(value(&mut index)?),
                "--model" => model = value(&mut index)?,
                "--language" => language = value(&mut index)?,
                "--convert" => convert = value(&mut index)?,
                "--vocab-dir" => vocab_dir = Some(PathBuf::from(value(&mut index)?)),
                "--translate" | "--target" => translate = Some(value(&mut index)?),
                "--wait" => wait = true,
                "--accept-license" => accept_license = true,
                "--json" => json = true,
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other if other.starts_with('-') => return Err(anyhow!("unknown flag {other}")),
                other => positionals.push(other.to_string()),
            }
            index += 1;
        }
        if convert != "none" && convert != "s2twp" {
            return Err(anyhow!("--convert must be none or s2twp"));
        }
        Ok(Flags {
            store,
            socket,
            model,
            language,
            convert,
            vocab_dir,
            translate,
            wait,
            accept_license,
            json,
            positionals,
        })
    }
}

const HELP: &str = "see `voxkit help`";

fn print_help() {
    eprintln!(
        r#"voxkit — local voice daemon for the fcitx5-mcbopomofo proof of concept

Usage:
  voxkit serve [--socket PATH] [--store DIR] [--model ID]
  voxkit model list [--store DIR]
  voxkit model install [ID] [--accept-license] [--store DIR] [--json]
  voxkit model remove ID | verify ID | select ID [--store DIR] [--json]
  voxkit model list --json           (offline; same shape as the socket's model.list)
  voxkit transcribe FILE.wav [--model ID] [--language zh] [--convert none|s2twp]
                             [--vocab-dir DIR] [--store DIR] [--translate LANG]
                             [--wait]
  voxkit translate TEXT [--target en|ja|ko|zh-Hant|...] [--language zh]
                        [--convert none|s2twp] [--store DIR] [--wait]

The default speech model is whisper-tiny. `VOXKIT_MODEL` (the systemd unit
prefers qwen3-asr-0.6b) is used when that file is installed; otherwise the
daemon falls back to an installed ASR model and logs it. `model install`
with no id installs whisper-tiny and Silero VAD. Speech models are
downloaded by `model install`, or (if none is installed at all) on the first
dictation — a small default, not the 850 MB preferred model.

The translation model (hy-mt1.5-1.8b, ~1.1 GB) is downloaded in the
background on the first IME translate request; that request commits the
original transcript and shows 「翻譯模型下載中」. The CLI does the same
(exit 3) unless you pass `--wait` or run `voxkit model install hy-mt1.5-1.8b`
first. Translation needs
the optional plugin libvoxkit-mt (searched in $VOXKIT_MT_PLUGIN,
<exe>/../lib/voxkit/, /usr/lib/voxkit/).

Models load on first use and unload after VOXKIT_IDLE_UNLOAD seconds idle
(default 600; `never` keeps them; VOXKIT_MT_IDLE_UNLOAD for translation only).

Paths:
  models   $VOXKIT_MODELS_DIR or $XDG_DATA_HOME/voxkit/models
  socket   $VOXKIT_SOCKET or $XDG_RUNTIME_DIR/voxkit.sock
"#
    );
}
