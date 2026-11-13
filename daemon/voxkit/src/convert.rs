//! Optional Traditional Chinese conversion.
//!
//! Uses the system libopencc in-process (dlopen, `s2twp` config, opened once)
//! when available: ~1 ms per call instead of ~80 ms to spawn the `opencc`
//! binary. Falls back to the binary, then to the original text with a
//! warning. No dictionary is bundled; no link-time dependency on OpenCC.

use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub fn maybe_convert(text: &str, mode: &str) -> String {
    if mode != "s2twp" || text.is_empty() {
        return text.to_string();
    }
    if let Some(lib) = libopencc() {
        if let Some(converted) = lib.convert(text) {
            return converted;
        }
    }
    match convert_s2twp(text) {
        Ok(converted) => converted,
        Err(err) => {
            eprintln!("voxkit: OpenCC s2twp skipped ({err})");
            text.to_string()
        }
    }
}

pub fn convert_s2twp(text: &str) -> Result<String, String> {
    let bin = which("opencc").ok_or_else(|| "opencc is not on PATH".to_string())?;
    let config = opencc_config()
        .ok_or_else(|| "s2twp.json was not found (Debian/Ubuntu package: opencc)".to_string())?;
    let mut child = Command::new(bin)
        .arg("-c")
        .arg(&config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| err.to_string())?;
    {
        let mut stdin = child.stdin.take().ok_or("opencc stdin")?;
        stdin
            .write_all(text.as_bytes())
            .map_err(|err| err.to_string())?;
    }
    let output = child.wait_with_output().map_err(|err| err.to_string())?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(format!("opencc exited {}: {err}", output.status));
    }
    let mut converted = String::from_utf8_lossy(&output.stdout).into_owned();
    if converted.ends_with('\n') && !text.ends_with('\n') {
        converted.pop();
        if converted.ends_with('\r') {
            converted.pop();
        }
    }
    Ok(converted)
}

fn opencc_config() -> Option<PathBuf> {
    for candidate in [
        "/usr/share/opencc/s2twp.json",
        "/usr/local/share/opencc/s2twp.json",
    ] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn which(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_mode_is_identity() {
        assert_eq!(maybe_convert("软件", "none"), "软件");
    }

    #[test]
    fn s2twp_converts_when_opencc_is_installed() {
        if which("opencc").is_none() || opencc_config().is_none() {
            return;
        }
        let converted = convert_s2twp("软件").unwrap();
        assert_eq!(converted, "軟體");
    }
}

/// libopencc loaded at runtime with the s2twp converter opened once.
struct LibOpenCc {
    handle: *mut libc::c_void,
    convert: unsafe extern "C" fn(*mut libc::c_void, *const libc::c_char, libc::size_t) -> *mut libc::c_char,
    free: unsafe extern "C" fn(*mut libc::c_char),
    lock: std::sync::Mutex<()>,
}

// The OpenCC handle is only used under `lock`.
unsafe impl Send for LibOpenCc {}
unsafe impl Sync for LibOpenCc {}

impl LibOpenCc {
    fn load() -> Option<Self> {
        let config = opencc_config()?;
        let config = std::ffi::CString::new(config.to_str()?).ok()?;
        for name in [c"libopencc.so.1.4", c"libopencc.so.1", c"libopencc.so"] {
            let lib = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if lib.is_null() {
                continue;
            }
            let open = unsafe { libc::dlsym(lib, c"opencc_open".as_ptr()) };
            let convert = unsafe { libc::dlsym(lib, c"opencc_convert_utf8".as_ptr()) };
            let free = unsafe { libc::dlsym(lib, c"opencc_convert_utf8_free".as_ptr()) };
            if open.is_null() || convert.is_null() || free.is_null() {
                continue;
            }
            let open: unsafe extern "C" fn(*const libc::c_char) -> *mut libc::c_void =
                unsafe { std::mem::transmute(open) };
            let handle = unsafe { open(config.as_ptr()) };
            // opencc_open returns (opencc_t)-1 on failure.
            if handle.is_null() || handle as isize == -1 {
                return None;
            }
            return Some(LibOpenCc {
                handle,
                convert: unsafe { std::mem::transmute(convert) },
                free: unsafe { std::mem::transmute(free) },
                lock: std::sync::Mutex::new(()),
            });
        }
        None
    }

    fn convert(&self, text: &str) -> Option<String> {
        let _guard = self.lock.lock().ok()?;
        let out = unsafe {
            (self.convert)(self.handle, text.as_ptr() as *const libc::c_char, text.len())
        };
        if out.is_null() {
            return None;
        }
        let result = unsafe { std::ffi::CStr::from_ptr(out) }.to_string_lossy().into_owned();
        unsafe { (self.free)(out) };
        Some(result)
    }
}

fn libopencc() -> Option<&'static LibOpenCc> {
    static LIB: std::sync::OnceLock<Option<LibOpenCc>> = std::sync::OnceLock::new();
    LIB.get_or_init(LibOpenCc::load).as_ref()
}

#[cfg(test)]
mod lib_tests {
    #[test]
    fn libopencc_matches_the_binary_when_installed() {
        let Some(lib) = super::libopencc() else { return };
        let text = "我想测试，我想去台北吃火锅。软件";
        let via_lib = lib.convert(text).unwrap();
        assert_eq!(via_lib, "我想測試，我想去臺北吃火鍋。軟體");
        if let Ok(via_bin) = super::convert_s2twp(text) {
            assert_eq!(via_lib, via_bin);
        }
    }
}
