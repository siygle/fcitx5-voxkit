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
    match sub {
        "list" => {
            print_models(&flags.store);
            Ok(ExitCode::SUCCESS)
        }
        "install" => {
            let id = flags.positionals.get(1).map(String::as_str);
            models::install_requested(&flags.store, id)?;
            Ok(ExitCode::SUCCESS)
        }
        other => Err(anyhow!("unknown model command '{other}'")),
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
    if flags.wait {
        loop {
            match translator.ensure() {
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
  voxkit model install [ID] [--store DIR]
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
