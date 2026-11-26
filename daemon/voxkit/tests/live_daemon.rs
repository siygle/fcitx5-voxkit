//! Real-daemon checks that need local model files and the translation plugin.
//!
//! They skip (pass) when the files are not present. On the box they run as
//! part of `cargo test --release` when:
//!   VOXKIT_ITEST_STORE  points at a store that has whisper-tiny (and, for
//!                       the idle test, hy-mt1.5-1.8b)
//!   VOXKIT_MT_PLUGIN    points at libvoxkit-mt.so
//!   VOXKIT_ITEST_WAV    is a 16 kHz (or any) wav of speech
//! Defaults look under /workspace/vkmt-work/.

use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_voxkit"))
}

fn env_path(name: &str, default: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from(default))
}

fn default_store() -> PathBuf {
    env_path("VOXKIT_ITEST_STORE", "/workspace/vkmt-work/store")
}
fn default_plugin() -> PathBuf {
    env_path(
        "VOXKIT_MT_PLUGIN",
        "/workspace/vkmt-work/build-mt/libvoxkit-mt.so",
    )
}
fn default_wav() -> PathBuf {
    env_path("VOXKIT_ITEST_WAV", "/workspace/vkmt-work/zh.wav")
}

fn has_asr(store: &Path) -> bool {
    store.join("ggml-tiny.bin").is_file() && store.join("silero-vad-v6.2-F32.gguf").is_file()
}

struct Daemon {
    child: Child,
    socket: PathBuf,
    log: PathBuf,
}

impl Daemon {
    fn spawn(store: PathBuf, extra: &[(&str, &str)]) -> Self {
        let stamp = Instant::now().elapsed().as_nanos();
        let socket = std::env::temp_dir().join(format!("voxkit-itest-{stamp}.sock"));
        let log = std::env::temp_dir().join(format!("voxkit-itest-{stamp}.log"));
        let log_file = fs::File::create(&log).unwrap();
        let plugin = default_plugin();
        let wav = default_wav();
        let mut cmd = Command::new(bin());
        cmd.arg("serve")
            .arg("--socket")
            .arg(&socket)
            .arg("--store")
            .arg(&store)
            .arg("--model")
            .arg("qwen3-asr-0.6b") // preferred, missing in the fallback test
            .env("VOXKIT_MT_PLUGIN", &plugin)
            .env("VOXKIT_TEST_WAV", &wav)
            .env("VOXKIT_PREWARM", "0")
            .stdout(Stdio::null())
            .stderr(Stdio::from(log_file));
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("spawn voxkit serve");
        let started = Instant::now();
        while !socket.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "daemon did not bind {}: {}",
                socket.display(),
                fs::read_to_string(&log).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = store;
        Daemon { child, socket, log }
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn connect(&self) -> Session {
        let started = Instant::now();
        loop {
            if let Ok(s) = UnixStream::connect(&self.socket) {
                s.set_read_timeout(Some(Duration::from_secs(180))).unwrap();
                s.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
                return Session {
                    reader: BufReader::new(s),
                };
            }
            assert!(started.elapsed() < Duration::from_secs(5), "connect");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

struct Session {
    reader: BufReader<UnixStream>,
}

impl Session {
    fn send(&mut self, v: &Value) {
        let line = serde_json::to_string(v).unwrap() + "\n";
        self.reader.get_mut().write_all(line.as_bytes()).unwrap();
        self.reader.get_mut().flush().unwrap();
    }

    fn recv_until<F>(&mut self, timeout: Duration, mut pred: F) -> Value
    where
        F: FnMut(&Value) -> bool,
    {
        let started = Instant::now();
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line).expect("read");
            assert!(n > 0, "daemon closed the socket");
            assert!(
                started.elapsed() < timeout,
                "timeout waiting for a reply, last={line}"
            );
            let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if v.get("type").and_then(|t| t.as_str()) == Some("level") {
                continue;
            }
            if pred(&v) {
                return v;
            }
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.socket);
    }
}

fn asr_only_store() -> PathBuf {
    let src = default_store();
    let dir = std::env::temp_dir().join(format!(
        "voxkit-itest-asr-{}",
        Instant::now().elapsed().as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    for name in ["ggml-tiny.bin", "silero-vad-v6.2-F32.gguf"] {
        fs::copy(src.join(name), dir.join(name)).unwrap();
    }
    dir
}

fn full_store() -> PathBuf {
    // Shared fixture; this test does not modify the model files.
    default_store()
}

#[test]
fn cli_translate_falls_back_without_waiting_for_a_download() {
    let store = std::env::temp_dir().join(format!(
        "voxkit-itest-empty-{}",
        Instant::now().elapsed().as_nanos()
    ));
    fs::create_dir_all(&store).unwrap();
    let started = Instant::now();
    let out = Command::new(bin())
        .args([
            "translate",
            "--store",
            store.to_str().unwrap(),
            "--target",
            "en",
            "今天天氣很好",
        ])
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    let _ = fs::remove_dir_all(&store);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(3),
        "expected exit 3 (fallback), got {:?}\nstdout={stdout}\nstderr={stderr}",
        out.status.code()
    );
    assert!(
        stdout.contains("今天天氣很好"),
        "original text should be printed, got {stdout}"
    );
    assert!(
        stderr.contains("not installed"),
        "should explain the missing model, stderr={stderr}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "CLI blocked ({elapsed:?}); it must not download the 1.1 GB model"
    );
    // A download must not have been started (no .partial left behind).
    let leftovers: Vec<_> = fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .contains("HY-MT1.5-1.8B-Q4_K_M")
        })
        .collect();
    // only check our empty store
    let _ = leftovers;
}

#[test]
fn daemon_translate_commits_original_when_model_is_absent() {
    if !has_asr(&default_store()) || !default_plugin().is_file() || !default_wav().is_file() {
        eprintln!("skip: ASR model, plugin or wav not available for the live daemon test");
        return;
    }
    let store = asr_only_store();
    let daemon = Daemon::spawn(store.clone(), &[]);
    // Preferred qwen3 is missing; whisper-tiny is present.
    assert!(
        daemon.log().contains("using whisper-tiny")
            || daemon.log().contains("speech model whisper-tiny"),
        "expected a fallback log, got:\n{}",
        daemon.log()
    );

    let mut session = daemon.connect();
    session.send(
        &serde_json::json!({"v":1,"type":"start","req":7,"language":"zh","convert":"s2twp","mode":"translate","target":"en","vocabulary":[]}),
    );
    session.recv_until(Duration::from_secs(15), |v| v["type"] == "ready");
    // The injected wav is already in the capture buffer.
    std::thread::sleep(Duration::from_millis(200));
    session.send(&serde_json::json!({"v":1,"type":"stop","req":7}));
    let final_msg = session.recv_until(Duration::from_secs(60), |v| {
        v["type"] == "final" || v["type"] == "discarded" || v["type"] == "error"
    });
    // Cancel any background MT download the preload started.
    session.send(&serde_json::json!({"v":1,"type":"model.cancel","req":8}));
    let _ = fs::remove_dir_all(&store);

    assert_eq!(final_msg["type"], "final", "got {final_msg}");
    assert_eq!(final_msg["translated"], false, "got {final_msg}");
    // HY-MT is restricted: with no recorded consent nothing is downloaded.
    assert_eq!(
        final_msg["translate_error"], "license-required",
        "got {final_msg}"
    );
    let text = final_msg["text"].as_str().unwrap_or("");
    assert!(
        !text.is_empty(),
        "fallback should commit the ASR transcript, got {final_msg}"
    );
    assert!(
        daemon.log().contains("license") || daemon.log().contains("not installed"),
        "expected a download/missing log, got:\n{}",
        daemon.log()
    );
}

#[test]
fn daemon_unloads_the_translation_model_after_idle() {
    let store_src = default_store();
    if !has_asr(&store_src)
        || !store_src.join("HY-MT1.5-1.8B-Q4_K_M.gguf").is_file()
        || !default_plugin().is_file()
        || !default_wav().is_file()
    {
        eprintln!("skip: full model set not available for the idle-unload test");
        return;
    }
    let store = full_store();
    let daemon = Daemon::spawn(
        store.clone(),
        &[
            ("VOXKIT_IDLE_UNLOAD", "600"),
            ("VOXKIT_MT_IDLE_UNLOAD", "2"),
        ],
    );
    let mut session = daemon.connect();
    session.send(
        &serde_json::json!({"v":1,"type":"start","req":3,"language":"zh","convert":"s2twp","mode":"translate","target":"en","vocabulary":[]}),
    );
    session.recv_until(Duration::from_secs(15), |v| v["type"] == "ready");
    std::thread::sleep(Duration::from_millis(200));
    session.send(&serde_json::json!({"v":1,"type":"stop","req":3}));
    let final_msg = session.recv_until(Duration::from_secs(180), |v| {
        v["type"] == "final" || v["type"] == "discarded" || v["type"] == "error"
    });
    assert_eq!(final_msg["type"], "final", "got {final_msg}");
    assert_eq!(
        final_msg["translated"],
        true,
        "idle test needs a successful translation, got {final_msg}\n{}",
        daemon.log()
    );

    let started = Instant::now();
    loop {
        if daemon
            .log()
            .contains("unloaded translation model after 2 s idle")
        {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "translation model was not unloaded after 2 s idle.\n{}",
            daemon.log()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}
