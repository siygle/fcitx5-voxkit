//! Model catalog, checksums, and explicit downloads.
//!
//! Nothing is fetched unless `voxkit model install` is run. Checksums are
//! pinned. Silero and Qwen3-ASR match the Hex artifact list at commit
//! a7548271; Whisper tiny is the whisper.cpp `ggml-tiny.bin` file verified
//! against that URL (multilingual, small enough to run on CPU).

use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Asr,
    Vad,
    /// Translation model, run by the optional plugin (see `translate`).
    Mt,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Asr => "asr",
            Kind::Vad => "vad",
            Kind::Mt => "mt",
        }
    }
}

pub struct ModelSpec {
    pub id: &'static str,
    pub kind: Kind,
    pub filename: &'static str,
    pub url: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
    /// Context-prompt budget. Zero for the VAD model.
    pub prompt_chars: usize,
    pub license: &'static str,
    pub blurb: &'static str,
    /// Short user-facing name.
    pub title: &'static str,
    /// The license limits who may use the weights. Such a model is only
    /// downloaded after the user accepted it (`accept_license`).
    pub restricted: bool,
    /// What the restriction is, shown next to the consent checkbox.
    pub restriction: &'static str,
    /// The model this build suggests for its purpose.
    pub recommended: bool,
    /// Approximate resident memory once loaded, in bytes.
    pub runtime_bytes: u64,
}

const SPECS: &[ModelSpec] = &[
    ModelSpec {
        id: "whisper-tiny",
        kind: Kind::Asr,
        filename: "ggml-tiny.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin",
        bytes: 77_691_713,
        sha256: "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21",
        prompt_chars: crate::vocab::WHISPER_PROMPT_CHARS,
        license: "MIT (OpenAI Whisper weights)",
        title: "Whisper tiny（多語、輕量）",
        restricted: false,
        restriction: "",
        recommended: false,
        runtime_bytes: 150_000_000,
        blurb: "Whisper tiny, multilingual. Default zh model for this PoC: it loads through transcribe-cpp's whisper.cpp .bin path and fits a CPU box. Chinese quality is weak; prefer qwen3-asr-0.6b when you can download ~850 MB.",
    },
    ModelSpec {
        id: "qwen3-asr-0.6b",
        kind: Kind::Asr,
        filename: "Qwen3-ASR-0.6B-Q8_0.gguf",
        url: "https://huggingface.co/handy-computer/Qwen3-ASR-0.6B-gguf/resolve/e4e16599b900eb0cb36e524514756bb92eb092b7/Qwen3-ASR-0.6B-Q8_0.gguf",
        bytes: 850_423_456,
        sha256: "f081b2d5e23bd669d92cc331d722a8a0681943b8e6f34b48996fd5c319b5acd8",
        prompt_chars: crate::vocab::QWEN_PROMPT_CHARS,
        license: "see the Qwen3-ASR model card (not redistributed here)",
        title: "Qwen3-ASR 0.6B（中文辨識，建議）",
        restricted: false,
        restriction: "",
        recommended: true,
        runtime_bytes: 700_000_000,
        blurb: "Qwen3-ASR 0.6B Q8, the zh model Hex recommends. Checksum from anomalyco/hex a7548271. transcribe-cpp 0.4.1 knows the qwen3_asr architecture; this PoC has not been timed on CPU.",
    },
    ModelSpec {
        id: "silero-vad",
        kind: Kind::Vad,
        filename: "silero-vad-v6.2-F32.gguf",
        url: "https://huggingface.co/handy-computer/silero-vad-v6.2-gguf/resolve/a9b2d61c20ae4ddf3644edb34969e8185a485448/silero-vad-v6.2-F32.gguf",
        bytes: 1_241_056,
        sha256: "0d871a800c420fdd5d1b17a3aea4296d1c39608f0ff4a9295eb76457eb1c88e6",
        prompt_chars: 0,
        license: "MIT (Silero VAD)",
        title: "Silero VAD（語音偵測）",
        restricted: false,
        restriction: "",
        recommended: true,
        runtime_bytes: 10_000_000,
        blurb: "Silero VAD v6.2 F32 GGUF used by transcribe-cpp. Threshold 0.4 and min speech 160 ms, matching Hex.",
    },
    ModelSpec {
        id: "hy-mt1.5-1.8b",
        kind: Kind::Mt,
        filename: "HY-MT1.5-1.8B-Q4_K_M.gguf",
        url: "https://huggingface.co/tencent/HY-MT1.5-1.8B-GGUF/resolve/265b2e615a7dc9b06c435dc878829ad99a512ba2/HY-MT1.5-1.8B-Q4_K_M.gguf",
        bytes: 1_133_080_512,
        sha256: "4383ac0c3c8e476de98ff979c2a3f069f8c4fb385e7860cf2d28da896cc477c7",
        prompt_chars: 0,
        license: "Tencent HY Community License (not for use in the EU, UK or South Korea); see the model card",
        title: "HY-MT1.5 1.8B（翻譯）",
        restricted: true,
        restriction: "Tencent HY Community License：不得在歐盟、英國與南韓使用，並須遵守授權條款。",
        recommended: true,
        runtime_bytes: 1_300_000_000,
        blurb: "HY-MT1.5 1.8B Q4_K_M, the translation model for the translate hotkey. Downloaded on first use; needs the libvoxkit-mt plugin.",
    },
];

pub fn specs() -> &'static [ModelSpec] {
    SPECS
}

pub fn get(id: &str) -> Option<&'static ModelSpec> {
    SPECS.iter().find(|spec| spec.id == id)
}

pub fn default_asr_id() -> &'static str {
    "whisper-tiny"
}

/// The ASR model that will actually be used: `requested` if that file is
/// present, otherwise an installed ASR model (preferring `default_asr_id`).
/// Logs the fallback. If nothing is installed, returns `requested` unchanged
/// so the caller can report `model-not-ready` or start a download.
pub fn resolve_asr(store: &Path, requested: &str) -> String {
    if let Some(spec) = get(requested) {
        if spec.kind == Kind::Asr && locate(store, spec).is_some() {
            return requested.to_string();
        }
    }
    let mut fallback: Option<&str> = None;
    for spec in specs() {
        if spec.kind != Kind::Asr || locate(store, spec).is_none() {
            continue;
        }
        if spec.id == default_asr_id() {
            fallback = Some(spec.id);
            break;
        }
        if fallback.is_none() {
            fallback = Some(spec.id);
        }
    }
    match fallback {
        Some(id) => {
            if id != requested {
                eprintln!("voxkit: speech model {requested} is not installed; using {id}");
            }
            id.to_string()
        }
        None => requested.to_string(),
    }
}

/// True when a usable copy of this catalog id is in the store (or a
/// read-only fallback dir).
pub fn is_installed(store: &Path, id: &str) -> bool {
    get(id).is_some_and(|spec| locate(store, spec).is_some())
}

/// Where voxkit itself writes this model (download, delete). Never a
/// read-only fallback directory.
pub fn path_for(store: &Path, spec: &ModelSpec) -> PathBuf {
    store.join(spec.filename)
}

/// Read-only directories searched after the store, e.g. the Hex app's model
/// folder. `VOXKIT_READONLY_MODELS_DIRS` is a `:`-separated list. voxkit only
/// ever opens files here for reading: download, delete and select always use
/// the store, so another app's models cannot be changed or removed.
pub fn readonly_dirs() -> Vec<PathBuf> {
    env::var_os("VOXKIT_READONLY_MODELS_DIRS")
        .map(|v| {
            env::split_paths(&v)
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Where a usable copy of the model is: `Owned` in the store, or `Borrowed`
/// from a read-only directory. Borrowed copies are only reported when the
/// size matches (the checksum is verified when the model is loaded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    Owned(PathBuf),
    Borrowed(PathBuf),
}

impl Location {
    pub fn path(&self) -> &Path {
        match self {
            Location::Owned(p) | Location::Borrowed(p) => p,
        }
    }
}

pub fn locate(store: &Path, spec: &ModelSpec) -> Option<Location> {
    let own = path_for(store, spec);
    if own.is_file() {
        return Some(Location::Owned(own));
    }
    readonly_dirs().into_iter().find_map(|dir| {
        let candidate = dir.join(spec.filename);
        let size_ok = fs::metadata(&candidate)
            .map(|m| m.len() == spec.bytes)
            .unwrap_or(false);
        (candidate.is_file() && size_ok).then_some(Location::Borrowed(candidate))
    })
}

pub fn default_store() -> PathBuf {
    if let Some(dir) = env::var_os("VOXKIT_MODELS_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let base = env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/share"));
    base.join("voxkit").join("models")
}

pub fn default_socket() -> PathBuf {
    if let Some(path) = env::var_os("VOXKIT_SOCKET") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("voxkit.sock");
        }
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/voxkit-{uid}.sock"))
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn install(store: &Path, id: &str) -> anyhow::Result<PathBuf> {
    install_with(store, id, &mut |_, _, _| {})
}

/// `install` with a progress callback (id, received, total), for the
/// offline `--json` CLI.
pub fn install_with(
    store: &Path,
    id: &str,
    progress: &mut dyn FnMut(&str, u64, u64),
) -> anyhow::Result<PathBuf> {
    let spec = get(id).ok_or_else(|| anyhow!("unknown model '{id}'. try `voxkit model list`"))?;
    fs::create_dir_all(store)?;
    let _lock = DownloadLock::acquire(store)?;
    let dest = path_for(store, spec);
    if dest.is_file() && file_matches(&dest, spec)? {
        eprintln!("voxkit: {id} already installed ({})", dest.display());
        return Ok(dest);
    }
    if let Some(Location::Borrowed(path)) = locate(store, spec) {
        if file_matches(&path, spec)? {
            eprintln!(
                "voxkit: {id} already available read-only at {}",
                path.display()
            );
            return Ok(path);
        }
    }
    if !license_accepted(store, spec) {
        anyhow::bail!(
            "{id} has a restricted license; accept it first (--accept-license). {}",
            spec.restriction
        );
    }
    ensure_space(store, spec).map_err(|(_, message)| anyhow!(message))?;
    eprintln!("voxkit: downloading {id}");
    eprintln!("voxkit: {}", spec.blurb);
    eprintln!("voxkit: license: {}", spec.license);
    let partial = store.join(format!("{}.partial", spec.filename));
    let mut shown = 0u64;
    fetch(
        spec.url,
        &partial,
        spec.bytes,
        &mut |got| {
            progress(id, got, spec.bytes);
            if got >= shown + 8 * 1024 * 1024 || got == spec.bytes {
                eprint!("\rvoxkit: {got} / {} bytes", spec.bytes);
                let _ = std::io::stderr().flush();
                shown = got;
            }
        },
        &|| false,
    )
    .map_err(|err| anyhow!(err))?;
    eprintln!();
    let digest = sha256_file(&partial)?;
    let size = fs::metadata(&partial)?.len();
    if size != spec.bytes || !digest.eq_ignore_ascii_case(spec.sha256) {
        let _ = fs::remove_file(&partial);
        anyhow::bail!(
            "checksum mismatch for {id}: got {size} bytes {digest}, expected {} {}",
            spec.bytes,
            spec.sha256
        );
    }
    fs::rename(&partial, &dest).with_context(|| format!("rename {}", dest.display()))?;
    eprintln!("voxkit: installed {id} -> {}", dest.display());
    Ok(dest)
}

/// Install an ASR model and Silero, or Silero / a translation model alone.
pub fn install_requested(store: &Path, id: Option<&str>) -> anyhow::Result<()> {
    install_requested_with(store, id, &mut |_, _, _| {})
}

pub fn install_requested_with(
    store: &Path,
    id: Option<&str>,
    progress: &mut dyn FnMut(&str, u64, u64),
) -> anyhow::Result<()> {
    let id = id.unwrap_or_else(|| default_asr_id());
    let spec = get(id).ok_or_else(|| anyhow!("unknown model '{id}'"))?;
    install_with(store, spec.id, progress)?;
    if spec.kind == Kind::Asr {
        install_with(store, "silero-vad", progress)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// License consent

const ACCEPTED_FILE: &str = "licenses-accepted.txt";

/// Whether this model may be downloaded: unrestricted, or the user accepted
/// its license (recorded in the store, one id per line).
pub fn license_accepted(store: &Path, spec: &ModelSpec) -> bool {
    if !spec.restricted {
        return true;
    }
    fs::read_to_string(store.join(ACCEPTED_FILE))
        .map(|text| text.lines().any(|line| line.trim() == spec.id))
        .unwrap_or(false)
}

/// Record that the user accepted `id`'s license.
pub fn accept_license(store: &Path, id: &str) -> std::io::Result<()> {
    let Some(spec) = get(id) else {
        return Ok(());
    };
    if license_accepted(store, spec) {
        return Ok(());
    }
    fs::create_dir_all(store)?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(store.join(ACCEPTED_FILE))?;
    writeln!(file, "{}", spec.id)
}

// ---------------------------------------------------------------------------
// Disk space, verification

/// Free bytes for the current user on the file system holding `dir` (or its
/// nearest existing parent).
pub fn free_bytes(dir: &Path) -> Option<u64> {
    let mut probe = dir.to_path_buf();
    while !probe.exists() {
        if !probe.pop() {
            return None;
        }
    }
    let c = std::ffi::CString::new(probe.as_os_str().as_encoded_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

/// Head room kept free after a download.
pub const SPACE_MARGIN: u64 = 64 * 1024 * 1024;

/// Bytes still to fetch for `spec` (a partial download counts).
pub fn remaining_bytes(store: &Path, spec: &ModelSpec) -> u64 {
    let partial = store.join(format!("{}.partial", spec.filename));
    let have = fs::metadata(partial).map(|m| m.len()).unwrap_or(0);
    spec.bytes.saturating_sub(have.min(spec.bytes))
}

/// Refuse a download that would not fit (code `no-space`).
pub fn ensure_space(store: &Path, spec: &ModelSpec) -> Result<(), (String, String)> {
    let need = remaining_bytes(store, spec);
    if need == 0 {
        return Ok(());
    }
    if let Some(free) = free_bytes(store) {
        if free < need + SPACE_MARGIN {
            return Err((
                "no-space".into(),
                format!(
                    "{} needs {} MB but only {} MB are free in {}",
                    spec.id,
                    (need + SPACE_MARGIN) / 1_000_000,
                    free / 1_000_000,
                    store.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Outcome of `verify`.
#[derive(Debug, PartialEq, Eq)]
pub enum Verified {
    Ok(PathBuf),
    Missing,
    Corrupt(PathBuf),
}

/// Re-hash an installed model (owned or read-only) against the catalog.
pub fn verify(store: &Path, id: &str) -> anyhow::Result<Verified> {
    let spec = get(id).ok_or_else(|| anyhow!("unknown model '{id}'"))?;
    let Some(found) = locate(store, spec) else {
        return Ok(Verified::Missing);
    };
    let path = found.path().to_path_buf();
    if file_matches(&path, spec)? {
        Ok(Verified::Ok(path))
    } else {
        Ok(Verified::Corrupt(path))
    }
}

pub fn require_file(store: &Path, id: &str) -> anyhow::Result<PathBuf> {
    let spec = get(id).ok_or_else(|| anyhow!("unknown model '{id}'"))?;
    let Some(found) = locate(store, spec) else {
        anyhow::bail!("model {id} is not installed. run: voxkit model install {id}");
    };
    if !verified_before(found.path(), spec) && !file_matches(found.path(), spec)? {
        anyhow::bail!(
            "model {id} failed checksum ({}). run: voxkit model install {id}",
            found.path().display()
        );
    }
    Ok(found.path().to_path_buf())
}

fn file_matches(path: &Path, spec: &ModelSpec) -> anyhow::Result<bool> {
    let size = fs::metadata(path)?.len();
    if size != spec.bytes {
        return Ok(false);
    }
    let digest = sha256_file(path)?;
    let ok = digest.eq_ignore_ascii_case(spec.sha256);
    if ok {
        remember_verified(path);
    }
    Ok(ok)
}

/// Files whose checksum already matched in this process, keyed by path and
/// (size, mtime). Models are now unloaded when idle; reloading must not
/// hash 850 MB again unless the file changed.
fn verified(
) -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, (u64, std::time::SystemTime)>> {
    static SEEN: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, (u64, std::time::SystemTime)>>,
    > = std::sync::OnceLock::new();
    SEEN.get_or_init(Default::default)
}

fn stamp(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

fn remember_verified(path: &Path) {
    if let Some(stamp) = stamp(path) {
        if let Ok(mut seen) = verified().lock() {
            seen.insert(path.to_path_buf(), stamp);
        }
    }
}

fn verified_before(path: &Path, spec: &ModelSpec) -> bool {
    let Some(now) = stamp(path) else { return false };
    now.0 == spec.bytes
        && verified()
            .lock()
            .map(|seen| seen.get(path) == Some(&now))
            .unwrap_or(false)
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Download `url` into `partial`, resuming an earlier partial file with an
/// HTTP Range request. `progress` gets the byte count so far; `cancelled`
/// stops the transfer and keeps the partial file for a later resume.
/// Returns Err("cancelled") when stopped.
pub fn fetch(
    url: &str,
    partial: &Path,
    expected: u64,
    progress: &mut dyn FnMut(u64),
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    let mut have = fs::metadata(partial).map(|m| m.len()).unwrap_or(0);
    if expected > 0 && have > expected {
        let _ = fs::remove_file(partial);
        have = 0;
    }
    if expected > 0 && have == expected {
        progress(have);
        return Ok(());
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(120))
        .redirects(10)
        .build();
    let mut request = agent.get(url).set("User-Agent", "voxkit/0.1");
    if have > 0 {
        request = request.set("Range", &format!("bytes={have}-"));
    }
    let response = request
        .call()
        .map_err(|err| format!("download {url}: {err}"))?;
    let resumed = have > 0 && response.status() == 206;
    let mut file = if resumed {
        fs::OpenOptions::new()
            .append(true)
            .open(partial)
            .map_err(|err| err.to_string())?
    } else {
        // 200: the server sent the whole file; start over.
        have = 0;
        File::create(partial).map_err(|err| err.to_string())?
    };
    if resumed {
        eprintln!("voxkit: resuming at {have} bytes");
    }
    let mut reader = response.into_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut got = have;
    progress(got);
    loop {
        if cancelled() {
            let _ = file.flush();
            return Err("cancelled".into());
        }
        let n = reader.read(&mut buf).map_err(|err| err.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|err| err.to_string())?;
        got += n as u64;
        progress(got);
    }
    file.flush().map_err(|err| err.to_string())?;
    Ok(())
}

struct DownloadLock {
    file: File,
}

impl DownloadLock {
    fn acquire(dir: &Path) -> anyhow::Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(dir.join(".download.lock"))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            anyhow::bail!(
                "could not lock {}: {}",
                dir.display(),
                std::io::Error::last_os_error()
            );
        }
        Ok(DownloadLock { file })
    }
}

impl Drop for DownloadLock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

use std::os::unix::io::AsRawFd;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_checksums_are_pinned_hex() {
        assert!(get(default_asr_id()).is_some());
        assert!(get("silero-vad").unwrap().kind == Kind::Vad);
        for spec in specs() {
            assert_eq!(spec.sha256.len(), 64, "{}", spec.id);
            assert!(spec.sha256.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(spec.bytes > 0, "{}", spec.id);
            assert!(spec.url.starts_with("https://"));
        }
    }

    #[test]
    fn resolve_asr_falls_back_to_an_installed_model() {
        let dir = std::env::temp_dir().join(format!(
            "voxkit-resolve-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            resolve_asr(&dir, "qwen3-asr-0.6b"),
            "qwen3-asr-0.6b",
            "nothing installed: keep the requested id"
        );
        let tiny = path_for(&dir, get("whisper-tiny").unwrap());
        std::fs::write(&tiny, b"placeholder").unwrap();
        assert_eq!(
            resolve_asr(&dir, "qwen3-asr-0.6b"),
            "whisper-tiny",
            "configured model missing, an installed ASR is used"
        );
        assert_eq!(
            resolve_asr(&dir, "whisper-tiny"),
            "whisper-tiny",
            "the installed requested model is kept"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "voxkit-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn catalog_has_manager_metadata() {
        for spec in specs() {
            assert!(!spec.title.is_empty(), "{}", spec.id);
            assert!(spec.runtime_bytes > 0, "{}", spec.id);
            assert_eq!(spec.restricted, !spec.restriction.is_empty(), "{}", spec.id);
        }
        assert!(get("hy-mt1.5-1.8b").unwrap().restricted);
        assert!(!get("qwen3-asr-0.6b").unwrap().restricted);
    }

    #[test]
    fn restricted_license_must_be_accepted_once() {
        let store = temp("consent");
        let mt = get("hy-mt1.5-1.8b").unwrap();
        let asr = get("whisper-tiny").unwrap();
        assert!(license_accepted(&store, asr), "unrestricted needs nothing");
        assert!(!license_accepted(&store, mt));
        let err = install(&store, mt.id).unwrap_err().to_string();
        assert!(err.contains("restricted license"), "{err}");
        accept_license(&store, mt.id).unwrap();
        accept_license(&store, mt.id).unwrap();
        assert!(license_accepted(&store, mt));
        let text = std::fs::read_to_string(store.join(ACCEPTED_FILE)).unwrap();
        assert_eq!(text.lines().count(), 1, "recorded once");
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn space_and_partial_accounting() {
        let store = temp("space");
        assert!(free_bytes(&store).unwrap() > 0);
        assert!(
            free_bytes(&store.join("not/yet/there")).is_some(),
            "uses a parent"
        );
        let spec = get("silero-vad").unwrap();
        assert_eq!(remaining_bytes(&store, spec), spec.bytes);
        std::fs::write(
            store.join(format!("{}.partial", spec.filename)),
            vec![0u8; 1000],
        )
        .unwrap();
        assert_eq!(remaining_bytes(&store, spec), spec.bytes - 1000);
        assert!(ensure_space(&store, spec).is_ok());
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn verify_reports_missing_and_corrupt() {
        let store = temp("verify");
        assert_eq!(verify(&store, "silero-vad").unwrap(), Verified::Missing);
        let spec = get("silero-vad").unwrap();
        File::create(path_for(&store, spec))
            .unwrap()
            .set_len(spec.bytes)
            .unwrap();
        assert!(matches!(
            verify(&store, "silero-vad").unwrap(),
            Verified::Corrupt(_)
        ));
        let _ = std::fs::remove_dir_all(&store);
    }

    /// One-shot HTTP server: answers a Range request with 206 (or 200 when
    /// `honour_range` is false) and returns the Range header it saw.
    fn serve_once(body: Vec<u8>, honour_range: bool) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/model.bin", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut range = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                    range = v.trim().trim_end_matches('-').to_string();
                }
            }
            let from: usize = range.parse().unwrap_or(0);
            let (status, part) = if honour_range && from > 0 {
                ("206 Partial Content", &body[from..])
            } else {
                ("200 OK", &body[..])
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                part.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(part).unwrap();
            range
        });
        (url, handle)
    }

    #[test]
    fn fetch_resumes_a_partial_download() {
        let dir = temp("resume");
        let body: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let partial = dir.join("m.partial");
        std::fs::write(&partial, &body[..1200]).unwrap();
        let (url, server) = serve_once(body.clone(), true);
        let mut seen = Vec::new();
        fetch(
            &url,
            &partial,
            body.len() as u64,
            &mut |n| seen.push(n),
            &|| false,
        )
        .unwrap();
        assert_eq!(server.join().unwrap(), "1200", "asked for the rest only");
        assert_eq!(seen.first(), Some(&1200));
        assert_eq!(std::fs::read(&partial).unwrap(), body);

        // A server that ignores Range sends everything: start over.
        std::fs::write(&partial, &body[..700]).unwrap();
        let (url, server) = serve_once(body.clone(), false);
        fetch(&url, &partial, body.len() as u64, &mut |_| {}, &|| false).unwrap();
        server.join().unwrap();
        assert_eq!(std::fs::read(&partial).unwrap(), body);

        // Cancel keeps what was fetched.
        std::fs::remove_file(&partial).unwrap();
        let (url, server) = serve_once(body.clone(), true);
        let err = fetch(&url, &partial, body.len() as u64, &mut |_| {}, &|| true).unwrap_err();
        assert_eq!(err, "cancelled");
        assert!(partial.exists(), "partial file kept for a resume");
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
