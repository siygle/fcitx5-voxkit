//! Translation after recognition (the `translate` mode of `start`).
//!
//! The transcript goes through a local LLM translation model (default
//! HY-MT1.5-1.8B, GGUF) run by the optional plugin `libvoxkit-mt`: llama.cpp
//! with its own ggml, loaded with dlopen only when a translation is asked
//! for and closed again after the idle timeout (see `lifecycle`). No extra
//! process and no server: the daemon calls the plugin's C functions.
//!
//! Rules:
//! - Traditional Chinese targets get OpenCC `s2twp` after translation.
//! - Any failure (no plugin, model still downloading, load or decode error,
//!   empty output) delivers the original transcript instead, converted as
//!   in dictation, and says why in `translate_error`.
//! - The user's dictionary is used only as a recognition hint for the ASR
//!   step. It is never applied to the translated text.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};

use crate::convert::maybe_convert;
use crate::models::{self, Kind};
use crate::models_ctl;

pub const DEFAULT_MT_MODEL: &str = "hy-mt1.5-1.8b";
pub const PLUGIN_ABI: c_int = 1;

#[cfg(target_os = "macos")]
pub const PLUGIN_FILE: &str = "libvoxkit-mt.dylib";
#[cfg(not(target_os = "macos"))]
pub const PLUGIN_FILE: &str = "libvoxkit-mt.so";

/// A language the frontends may ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    /// Canonical code sent back to the frontend.
    pub code: &'static str,
    /// Name used in HY-MT's Chinese prompt template.
    pub zh_name: &'static str,
    /// Name used in HY-MT's English prompt template.
    pub en_name: &'static str,
    /// Run OpenCC s2twp on the translation.
    pub traditional: bool,
}

const TARGETS: &[Target] = &[
    Target {
        code: "en",
        zh_name: "英语",
        en_name: "English",
        traditional: false,
    },
    Target {
        code: "ja",
        zh_name: "日语",
        en_name: "Japanese",
        traditional: false,
    },
    Target {
        code: "ko",
        zh_name: "韩语",
        en_name: "Korean",
        traditional: false,
    },
    Target {
        code: "zh-Hant",
        zh_name: "繁体中文",
        en_name: "Traditional Chinese",
        traditional: true,
    },
    Target {
        code: "zh-Hans",
        zh_name: "中文",
        en_name: "Chinese",
        traditional: false,
    },
    Target {
        code: "fr",
        zh_name: "法语",
        en_name: "French",
        traditional: false,
    },
    Target {
        code: "de",
        zh_name: "德语",
        en_name: "German",
        traditional: false,
    },
    Target {
        code: "es",
        zh_name: "西班牙语",
        en_name: "Spanish",
        traditional: false,
    },
    Target {
        code: "pt",
        zh_name: "葡萄牙语",
        en_name: "Portuguese",
        traditional: false,
    },
    Target {
        code: "it",
        zh_name: "意大利语",
        en_name: "Italian",
        traditional: false,
    },
    Target {
        code: "ru",
        zh_name: "俄语",
        en_name: "Russian",
        traditional: false,
    },
    Target {
        code: "vi",
        zh_name: "越南语",
        en_name: "Vietnamese",
        traditional: false,
    },
    Target {
        code: "th",
        zh_name: "泰语",
        en_name: "Thai",
        traditional: false,
    },
    Target {
        code: "id",
        zh_name: "印尼语",
        en_name: "Indonesian",
        traditional: false,
    },
    Target {
        code: "ms",
        zh_name: "马来语",
        en_name: "Malay",
        traditional: false,
    },
    Target {
        code: "tr",
        zh_name: "土耳其语",
        en_name: "Turkish",
        traditional: false,
    },
    Target {
        code: "ar",
        zh_name: "阿拉伯语",
        en_name: "Arabic",
        traditional: false,
    },
    Target {
        code: "nl",
        zh_name: "荷兰语",
        en_name: "Dutch",
        traditional: false,
    },
    Target {
        code: "pl",
        zh_name: "波兰语",
        en_name: "Polish",
        traditional: false,
    },
];

/// `en`, `EN`, `en-US` → English; `zh-TW`, `zh_TW`, `zh-HK`, `zh-Hant` →
/// Traditional Chinese; `zh`, `zh-CN`, `zh-Hans` → Simplified.
pub fn target(code: &str) -> Option<Target> {
    let lower = code.trim().to_ascii_lowercase().replace('_', "-");
    let canonical = match lower.as_str() {
        "zh-hant" | "zh-tw" | "zh-hk" | "zh-mo" | "zht" => "zh-Hant",
        "zh" | "zh-hans" | "zh-cn" | "zh-sg" => "zh-Hans",
        other => other.split('-').next().unwrap_or(other),
    };
    TARGETS
        .iter()
        .copied()
        .find(|t| t.code.eq_ignore_ascii_case(canonical))
}

pub fn target_codes() -> Vec<&'static str> {
    TARGETS.iter().map(|t| t.code).collect()
}

/// HY-MT1.5 prompt (model card): the Chinese template when Chinese is on
/// either side, otherwise the English one.
pub fn prompt(source_text: &str, source_language: &str, target: &Target) -> String {
    let chinese_side = source_language
        .trim()
        .to_ascii_lowercase()
        .starts_with("zh")
        || target.code.starts_with("zh");
    if chinese_side {
        format!(
            "将以下文本翻译为{}，注意只需要输出翻译后的结果，不要额外解释：\n\n{}",
            target.zh_name, source_text
        )
    } else {
        format!(
            "Translate the following segment into {}, without additional explanation.\n\n{}",
            target.en_name, source_text
        )
    }
}

/// Why a translation did not happen. `code()` goes into `translate_error`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    BadTarget(String),
    PluginMissing,
    Plugin(String),
    ModelDownloading,
    Load(String),
    Translate(String),
    Empty,
    Cancelled,
}

impl Failure {
    pub fn code(&self) -> &'static str {
        match self {
            Failure::BadTarget(_) => "bad-target",
            Failure::PluginMissing => "plugin-missing",
            Failure::Plugin(_) => "plugin-error",
            Failure::ModelDownloading => "model-downloading",
            Failure::Load(_) => "load-failed",
            Failure::Translate(_) => "translate-failed",
            Failure::Empty => "empty",
            Failure::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::BadTarget(t) => write!(f, "unsupported target language '{t}'"),
            Failure::PluginMissing => write!(f, "translation plugin {PLUGIN_FILE} not found"),
            Failure::Plugin(m) => write!(f, "translation plugin: {m}"),
            Failure::ModelDownloading => write!(f, "translation model is downloading"),
            Failure::Load(m) => write!(f, "translation model failed to load: {m}"),
            Failure::Translate(m) => write!(f, "translation failed: {m}"),
            Failure::Empty => write!(f, "translation was empty"),
            Failure::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// What gets committed for a translate request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub text: String,
    pub translated: bool,
    pub error: Option<Failure>,
}

/// Turn the model's answer (or failure) into the text to commit.
/// `convert(text, mode)` is OpenCC (`maybe_convert` in the daemon).
pub fn finish(
    source: &str,
    dictation_convert: &str,
    target: Option<&Target>,
    result: Result<String, Failure>,
    convert: &dyn Fn(&str, &str) -> String,
) -> Outcome {
    let fallback = |error: Failure| Outcome {
        text: convert(source, dictation_convert),
        translated: false,
        error: Some(error),
    };
    match result {
        Ok(raw) => {
            let text = clean(&raw);
            if text.is_empty() {
                return fallback(Failure::Empty);
            }
            let text = match target {
                Some(t) if t.traditional => convert(&text, "s2twp"),
                _ => text,
            };
            Outcome {
                text,
                translated: true,
                error: None,
            }
        }
        Err(error) => fallback(error),
    }
}

/// Trim the model output. HY-MT answers with the translation only; drop
/// surrounding whitespace and a stray end-of-turn marker if one leaks.
fn clean(raw: &str) -> String {
    let mut text = raw.trim().to_string();
    for marker in ["<｜hy_place▁holder▁no▁2｜>", "</s>", "<|eos|>"] {
        if let Some(stripped) = text.strip_suffix(marker) {
            text = stripped.trim_end().to_string();
        }
    }
    text
}

/// A loaded translation model.
pub trait Engine: Send {
    fn generate(
        &mut self,
        prompt: &str,
        max_tokens: i32,
        cancel: &AtomicI32,
    ) -> Result<String, Failure>;
}

type Loader = dyn FnMut(&Path) -> Result<Box<dyn Engine>, Failure> + Send;

/// Owns the (lazily loaded) translation model.
pub struct Translator {
    store: PathBuf,
    model_id: String,
    engine: Option<Box<dyn Engine>>,
    loader: Box<Loader>,
}

impl Translator {
    /// Real translator: model from the store, engine from the plugin.
    pub fn new(store: impl Into<PathBuf>) -> Self {
        let model_id = std::env::var("VOXKIT_MT_MODEL")
            .ok()
            .filter(|id| models::get(id).is_some_and(|s| s.kind == Kind::Mt))
            .unwrap_or_else(|| DEFAULT_MT_MODEL.to_string());
        Self::with_loader(store, model_id, Box::new(|path| plugin_engine(path)))
    }

    pub fn with_loader(store: impl Into<PathBuf>, model_id: String, loader: Box<Loader>) -> Self {
        Translator {
            store: store.into(),
            model_id,
            engine: None,
            loader,
        }
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Load the model if it is installed. When it is not, start the
    /// download (first use) and report `ModelDownloading`.
    pub fn ensure(&mut self) -> Result<(), Failure> {
        if self.engine.is_some() {
            return Ok(());
        }
        let spec = models::get(&self.model_id)
            .ok_or_else(|| Failure::Load(format!("unknown model {}", self.model_id)))?;
        let Some(found) = models::locate(&self.store, spec) else {
            if !models_ctl::downloading(&self.model_id) {
                match models_ctl::begin(self.store.clone(), self.model_id.clone()) {
                    Ok(()) => eprintln!(
                        "voxkit: translation model {} not installed; downloading ({} MB)",
                        self.model_id,
                        spec.bytes / 1_000_000
                    ),
                    Err((code, message)) => {
                        eprintln!(
                            "voxkit: cannot start {} download: {code} {message}",
                            self.model_id
                        )
                    }
                }
            }
            return Err(Failure::ModelDownloading);
        };
        let size = std::fs::metadata(found.path())
            .map(|m| m.len())
            .unwrap_or(0);
        if size != spec.bytes {
            return Err(Failure::Load(format!(
                "{} has {size} bytes, expected {}",
                found.path().display(),
                spec.bytes
            )));
        }
        let started = std::time::Instant::now();
        let engine = (self.loader)(found.path())?;
        eprintln!(
            "voxkit: loaded translation model {} in {} ms",
            self.model_id,
            started.elapsed().as_millis()
        );
        self.engine = Some(engine);
        Ok(())
    }

    pub fn translate(
        &mut self,
        text: &str,
        source_language: &str,
        target: &Target,
        cancel: &AtomicI32,
    ) -> Result<String, Failure> {
        self.ensure()?;
        let engine = self.engine.as_mut().ok_or(Failure::PluginMissing)?;
        let chars = text.chars().count() as i32;
        let max_tokens = (chars * 4 + 64).min(1024);
        let started = std::time::Instant::now();
        let out = engine.generate(&prompt(text, source_language, target), max_tokens, cancel);
        eprintln!(
            "voxkit: translate {} -> {} ({} chars) in {} ms: {}",
            source_language,
            target.code,
            chars,
            started.elapsed().as_millis(),
            match &out {
                Ok(_) => "ok".to_string(),
                Err(e) => e.to_string(),
            }
        );
        out
    }
}

impl crate::lifecycle::Unload for Translator {
    fn loaded(&self) -> bool {
        self.engine.is_some()
    }
    fn unload(&mut self) {
        self.engine = None; // drops the model, then dlcloses the plugin
    }
}

// ---------------------------------------------------------------------------
// The dlopen plugin.

/// Where the plugin is looked for, first hit wins:
/// `$VOXKIT_MT_PLUGIN`; next to the executable (`../lib/voxkit/`, the
/// executable's own directory, `../Frameworks/` in a macOS bundle);
/// `/usr/lib/voxkit/`, `/usr/local/lib/voxkit/`.
pub fn plugin_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(path) = std::env::var_os("VOXKIT_MT_PLUGIN").filter(|p| !p.is_empty()) {
        out.push(PathBuf::from(path));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join("../lib/voxkit").join(PLUGIN_FILE));
            out.push(dir.join(PLUGIN_FILE));
            out.push(dir.join("../Frameworks").join(PLUGIN_FILE));
        }
    }
    out.push(PathBuf::from("/usr/lib/voxkit").join(PLUGIN_FILE));
    out.push(PathBuf::from("/usr/local/lib/voxkit").join(PLUGIN_FILE));
    out
}

pub fn plugin_path() -> Option<PathBuf> {
    plugin_candidates().into_iter().find(|p| p.is_file())
}

type AbiFn = unsafe extern "C" fn() -> c_int;
type InfoFn = unsafe extern "C" fn() -> *const c_char;
type LoadFn = unsafe extern "C" fn(*const c_char, c_int, *mut c_char, usize) -> *mut c_void;
type GenerateFn = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    c_int,
    *const c_int,
    *mut c_char,
    usize,
    *mut c_char,
    usize,
) -> c_int;
type UnloadFn = unsafe extern "C" fn(*mut c_void);

struct Library {
    handle: *mut c_void,
    load: LoadFn,
    generate: GenerateFn,
    unload: UnloadFn,
}

// The handle is only used behind the Translator mutex.
unsafe impl Send for Library {}

impl Drop for Library {
    fn drop(&mut self) {
        unsafe { libc::dlclose(self.handle) };
    }
}

fn dlerror() -> String {
    let err = unsafe { libc::dlerror() };
    if err.is_null() {
        "unknown dlopen error".into()
    } else {
        unsafe { CStr::from_ptr(err) }
            .to_string_lossy()
            .into_owned()
    }
}

impl Library {
    fn open(path: &Path) -> Result<Library, Failure> {
        let cpath = CString::new(path.as_os_str().to_string_lossy().as_bytes())
            .map_err(|_| Failure::Plugin("bad path".into()))?;
        let handle = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(Failure::Plugin(format!(
                "dlopen {}: {}",
                path.display(),
                dlerror()
            )));
        }
        let sym = |name: &CStr| -> Result<*mut c_void, Failure> {
            let p = unsafe { libc::dlsym(handle, name.as_ptr()) };
            if p.is_null() {
                Err(Failure::Plugin(format!(
                    "missing symbol {}",
                    name.to_string_lossy()
                )))
            } else {
                Ok(p)
            }
        };
        let resolved = (|| {
            let abi: AbiFn = unsafe { std::mem::transmute(sym(c"vk_mt_abi")?) };
            let info: InfoFn = unsafe { std::mem::transmute(sym(c"vk_mt_info")?) };
            let version = unsafe { abi() };
            if version != PLUGIN_ABI {
                return Err(Failure::Plugin(format!(
                    "plugin ABI {version}, daemon wants {PLUGIN_ABI}"
                )));
            }
            let info = unsafe { CStr::from_ptr(info()) }
                .to_string_lossy()
                .into_owned();
            eprintln!("voxkit: translation plugin {} ({info})", path.display());
            Ok((
                unsafe { std::mem::transmute::<*mut c_void, LoadFn>(sym(c"vk_mt_load")?) },
                unsafe { std::mem::transmute::<*mut c_void, GenerateFn>(sym(c"vk_mt_generate")?) },
                unsafe { std::mem::transmute::<*mut c_void, UnloadFn>(sym(c"vk_mt_unload")?) },
            ))
        })();
        match resolved {
            Ok((load, generate, unload)) => Ok(Library {
                handle,
                load,
                generate,
                unload,
            }),
            Err(err) => {
                unsafe { libc::dlclose(handle) };
                Err(err)
            }
        }
    }
}

struct PluginEngine {
    model: *mut c_void,
    // Declared after `model`: dropped (dlclosed) after the model is freed.
    lib: Library,
}

unsafe impl Send for PluginEngine {}

impl Drop for PluginEngine {
    fn drop(&mut self) {
        unsafe { (self.lib.unload)(self.model) };
    }
}

impl Engine for PluginEngine {
    fn generate(
        &mut self,
        prompt: &str,
        max_tokens: i32,
        cancel: &AtomicI32,
    ) -> Result<String, Failure> {
        let cprompt = CString::new(prompt).map_err(|_| Failure::Translate("NUL in text".into()))?;
        let mut out = vec![0u8; 32 * 1024];
        let mut err = vec![0u8; 512];
        let rc = unsafe {
            (self.lib.generate)(
                self.model,
                cprompt.as_ptr(),
                max_tokens,
                cancel.as_ptr() as *const c_int,
                out.as_mut_ptr() as *mut c_char,
                out.len(),
                err.as_mut_ptr() as *mut c_char,
                err.len(),
            )
        };
        if rc == -2 {
            return Err(Failure::Cancelled);
        }
        if rc < 0 {
            let message = CStr::from_bytes_until_nul(&err)
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Err(Failure::Translate(format!("code {rc}: {message}")));
        }
        out.truncate(rc as usize);
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

fn plugin_engine(model_path: &Path) -> Result<Box<dyn Engine>, Failure> {
    let plugin = plugin_path().ok_or(Failure::PluginMissing)?;
    let lib = Library::open(&plugin)?;
    let cpath = CString::new(model_path.as_os_str().to_string_lossy().as_bytes())
        .map_err(|_| Failure::Load("bad model path".into()))?;
    let mut err = vec![0u8; 512];
    let model = unsafe {
        (lib.load)(
            cpath.as_ptr(),
            crate::engine::asr_threads(),
            err.as_mut_ptr() as *mut c_char,
            err.len(),
        )
    };
    if model.is_null() {
        let message = CStr::from_bytes_until_nul(&err)
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();
        return Err(Failure::Load(message));
    }
    Ok(Box::new(PluginEngine { model, lib }))
}

/// Convenience for the daemon: translate and decide what to commit.
pub fn run(
    translator: &mut Translator,
    source: &str,
    source_language: &str,
    dictation_convert: &str,
    target_code: &str,
    cancel: &AtomicI32,
) -> Outcome {
    let target = target(target_code);
    let result = match &target {
        None => Err(Failure::BadTarget(target_code.to_string())),
        Some(t) => translator.translate(source, source_language, t, cancel),
    };
    if cancel.load(Ordering::SeqCst) != 0 && result.is_err() {
        return Outcome {
            text: String::new(),
            translated: false,
            error: Some(Failure::Cancelled),
        };
    }
    finish(
        source,
        dictation_convert,
        target.as_ref(),
        result,
        &maybe_convert,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn fake_convert(text: &str, mode: &str) -> String {
        if mode == "s2twp" {
            format!("[s2twp]{text}")
        } else {
            text.to_string()
        }
    }

    #[test]
    fn target_aliases() {
        assert_eq!(target("EN").unwrap().code, "en");
        assert_eq!(target("en-US").unwrap().code, "en");
        assert_eq!(target("zh_TW").unwrap().code, "zh-Hant");
        assert_eq!(target("zh-Hant").unwrap().code, "zh-Hant");
        assert!(target("zh-Hant").unwrap().traditional);
        assert_eq!(target("zh-CN").unwrap().code, "zh-Hans");
        assert!(target("klingon").is_none());
    }

    #[test]
    fn prompt_uses_the_chinese_template_for_chinese_speech() {
        let p = prompt("今天天氣很好", "zh", &target("en").unwrap());
        assert!(p.starts_with("将以下文本翻译为英语"));
        assert!(p.ends_with("\n\n今天天氣很好"));
        let p = prompt("Good morning", "en", &target("ja").unwrap());
        assert!(p.starts_with("Translate the following segment into Japanese"));
    }

    #[test]
    fn traditional_target_runs_s2twp_after_translation() {
        let out = finish(
            "hello",
            "s2twp",
            target("zh-TW").as_ref(),
            Ok("你好软件".into()),
            &fake_convert,
        );
        assert_eq!(out.text, "[s2twp]你好软件");
        assert!(out.translated);
        // Other targets are left alone.
        let out = finish(
            "你好",
            "s2twp",
            target("en").as_ref(),
            Ok(" Hello. \n".into()),
            &fake_convert,
        );
        assert_eq!(out.text, "Hello.");
        assert!(out.translated);
    }

    #[test]
    fn failure_commits_the_original_transcript() {
        let out = finish(
            "今天天气很好",
            "s2twp",
            target("en").as_ref(),
            Err(Failure::PluginMissing),
            &fake_convert,
        );
        assert_eq!(
            out.text, "[s2twp]今天天气很好",
            "dictation conversion still applies"
        );
        assert!(!out.translated);
        assert_eq!(out.error.as_ref().unwrap().code(), "plugin-missing");
        let out = finish(
            "原文",
            "none",
            target("en").as_ref(),
            Ok("   ".into()),
            &fake_convert,
        );
        assert_eq!(out.text, "原文");
        assert_eq!(out.error.unwrap().code(), "empty");
    }

    #[test]
    fn real_opencc_on_translation_when_installed() {
        if crate::convert::convert_s2twp("软件").is_err() {
            return; // OpenCC not installed here
        }
        let out = finish(
            "x",
            "none",
            target("zh-Hant").as_ref(),
            Ok("我想测试软件".into()),
            &maybe_convert,
        );
        assert_eq!(out.text, "我想測試軟體");
    }

    struct Echo {
        prompts: Arc<Mutex<Vec<String>>>,
        fail: bool,
    }

    impl Engine for Echo {
        fn generate(&mut self, prompt: &str, _: i32, _: &AtomicI32) -> Result<String, Failure> {
            self.prompts.lock().unwrap().push(prompt.to_string());
            if self.fail {
                Err(Failure::Translate("boom".into()))
            } else {
                Ok("Hello world".into())
            }
        }
    }

    fn store_with_model(tag: &str) -> PathBuf {
        let store = std::env::temp_dir().join(format!("voxkit-mt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        std::fs::create_dir_all(&store).unwrap();
        let spec = models::get(DEFAULT_MT_MODEL).unwrap();
        std::fs::File::create(store.join(spec.filename))
            .unwrap()
            .set_len(spec.bytes)
            .unwrap();
        store
    }

    #[test]
    fn translator_loads_lazily_and_unloads() {
        use crate::lifecycle::Unload;
        let store = store_with_model("lazy");
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let loads = Arc::new(Mutex::new(0));
        let (p, l) = (Arc::clone(&prompts), Arc::clone(&loads));
        let mut tr = Translator::with_loader(
            &store,
            DEFAULT_MT_MODEL.into(),
            Box::new(move |_| {
                *l.lock().unwrap() += 1;
                Ok(Box::new(Echo {
                    prompts: Arc::clone(&p),
                    fail: false,
                }) as Box<dyn Engine>)
            }),
        );
        assert!(!tr.loaded(), "nothing loaded before first use");
        let cancel = AtomicI32::new(0);
        let out = run(&mut tr, "你好世界", "zh", "s2twp", "en", &cancel);
        assert_eq!(out.text, "Hello world");
        assert!(out.translated);
        assert!(tr.loaded());
        run(&mut tr, "你好", "zh", "s2twp", "en", &cancel);
        assert_eq!(*loads.lock().unwrap(), 1, "loaded once");
        tr.unload();
        assert!(!tr.loaded());
        run(&mut tr, "你好", "zh", "s2twp", "en", &cancel);
        assert_eq!(*loads.lock().unwrap(), 2, "reloaded after unload");
        assert!(prompts.lock().unwrap()[0].contains("翻译为英语"));
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn engine_error_and_bad_target_fall_back() {
        let store = store_with_model("fail");
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let p = Arc::clone(&prompts);
        let mut tr = Translator::with_loader(
            &store,
            DEFAULT_MT_MODEL.into(),
            Box::new(move |_| {
                Ok(Box::new(Echo {
                    prompts: Arc::clone(&p),
                    fail: true,
                }) as Box<dyn Engine>)
            }),
        );
        let cancel = AtomicI32::new(0);
        let out = run(&mut tr, "原文", "zh", "none", "en", &cancel);
        assert_eq!(out.text, "原文");
        assert_eq!(out.error.unwrap().code(), "translate-failed");
        let out = run(&mut tr, "原文", "zh", "none", "xx-nope", &cancel);
        assert_eq!(out.text, "原文");
        assert_eq!(out.error.unwrap().code(), "bad-target");
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn missing_plugin_is_reported() {
        let store = store_with_model("noplugin");
        std::env::set_var("VOXKIT_MT_PLUGIN", "/nonexistent/libvoxkit-mt.so");
        // Only meaningful when no system copy exists.
        if plugin_candidates().iter().skip(1).any(|p| p.is_file()) {
            return;
        }
        let mut tr = Translator::new(&store);
        let cancel = AtomicI32::new(0);
        let out = run(&mut tr, "原文", "zh", "none", "en", &cancel);
        assert_eq!(out.text, "原文");
        assert_eq!(out.error.unwrap().code(), "plugin-missing");
        let _ = std::fs::remove_dir_all(&store);
    }
}
