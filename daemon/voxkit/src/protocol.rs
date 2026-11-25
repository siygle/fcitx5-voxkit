//! Line-delimited JSON between the input method and the daemon.
//!
//! One UTF-8 object per line. `v` is 1. Unknown fields are ignored.

use serde::Deserialize;
use serde_json::{json, Value};

/// What `start` asks for. Absent means `Dictate` (every older client).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dictate,
    /// Recognize, then translate into `target`. Advertised in
    /// `model.status.capabilities`; an older daemon ignores the field.
    Translate,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Start {
        req: u64,
        language: String,
        vocabulary: Vec<String>,
        convert: String,
        model: Option<String>,
        mode: Mode,
        /// Target language for `Translate` (e.g. "en", "ja", "zh-Hant").
        target: String,
    },
    Stop {
        req: u64,
    },
    Cancel {
        req: u64,
    },
    SetVocabulary {
        req: u64,
        vocabulary: Vec<String>,
    },
    Ping {
        req: u64,
    },
    ModelList {
        req: u64,
    },
    ModelInstall {
        req: u64,
        model: String,
    },
    ModelProgress {
        req: u64,
    },
    ModelCancel {
        req: u64,
    },
    ModelRemove {
        req: u64,
        model: String,
    },
    ModelSelect {
        req: u64,
        model: String,
    },
    ModelStatus {
        req: u64,
    },
}

#[derive(Debug, Deserialize)]
struct Incoming {
    #[serde(default)]
    v: u32,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    req: u64,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    vocabulary: Vec<String>,
    #[serde(default)]
    convert: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    target: Option<String>,
}

pub fn parse_command(line: &str) -> Result<Command, String> {
    let incoming: Incoming = serde_json::from_str(line).map_err(|err| err.to_string())?;
    if incoming.v > 1 {
        return Err(format!("unsupported protocol version {}", incoming.v));
    }
    let req = incoming.req;
    match incoming.kind.as_str() {
        "start" => {
            let mode = match incoming.mode.as_deref().unwrap_or("") {
                "" | "dictate" => Mode::Dictate,
                "translate" => Mode::Translate,
                other => return Err(format!("unknown mode '{other}'")),
            };
            Ok(Command::Start {
                req,
                language: incoming
                    .language
                    .filter(|lang| !lang.is_empty())
                    .unwrap_or_else(|| "zh".into()),
                vocabulary: incoming.vocabulary,
                convert: incoming.convert.unwrap_or_else(|| "none".into()),
                model: incoming.model.filter(|model| !model.is_empty()),
                mode,
                target: incoming
                    .target
                    .filter(|target| !target.is_empty())
                    .unwrap_or_else(|| "en".into()),
            })
        }
        "stop" => Ok(Command::Stop { req }),
        "cancel" => Ok(Command::Cancel { req }),
        "set_vocabulary" => Ok(Command::SetVocabulary {
            req,
            vocabulary: incoming.vocabulary,
        }),
        "ping" => Ok(Command::Ping { req }),
        "model.list" => Ok(Command::ModelList { req }),
        "model.install" => Ok(Command::ModelInstall {
            req,
            model: incoming.model.unwrap_or_default(),
        }),
        "model.progress" => Ok(Command::ModelProgress { req }),
        "model.cancel" => Ok(Command::ModelCancel { req }),
        "model.remove" => Ok(Command::ModelRemove {
            req,
            model: incoming.model.unwrap_or_default(),
        }),
        "model.select" => Ok(Command::ModelSelect {
            req,
            model: incoming.model.unwrap_or_default(),
        }),
        "model.status" => Ok(Command::ModelStatus { req }),
        other => Err(format!("unknown message type '{other}'")),
    }
}

pub fn ready(req: u64) -> Value {
    json!({"v": 1, "type": "ready", "req": req})
}

pub fn partial(req: u64, stable: &str, tentative: &str) -> Value {
    json!({"v": 1, "type": "partial", "req": req, "stable": stable, "tentative": tentative})
}

pub fn final_text(req: u64, text: &str) -> Value {
    json!({"v": 1, "type": "final", "req": req, "text": text})
}

/// `final` for a translate request. `text` is what to commit: the
/// translation, or the original transcript when `translated` is false
/// (`translate_error` then says why). `source` is always the transcript.
pub fn final_translation(
    req: u64,
    text: &str,
    target: &str,
    translated: bool,
    source: &str,
    error: Option<&str>,
) -> Value {
    let mut value = json!({
        "v": 1,
        "type": "final",
        "req": req,
        "text": text,
        "mode": "translate",
        "target": target,
        "translated": translated,
        "source": source,
    });
    if let Some(code) = error {
        value["translate_error"] = json!(code);
    }
    value
}

pub fn discarded(req: u64, reason: &str) -> Value {
    json!({"v": 1, "type": "discarded", "req": req, "reason": reason})
}

pub fn error(req: u64, code: &str, message: &str) -> Value {
    json!({"v": 1, "type": "error", "req": req, "code": code, "message": message})
}

pub fn level(req: u64, rms_dbfs: f32) -> Value {
    json!({"v": 1, "type": "level", "req": req, "rms_dbfs": rms_dbfs})
}

pub fn pong(req: u64) -> Value {
    json!({"v": 1, "type": "pong", "req": req})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_fills_defaults_and_keeps_vocabulary() {
        let command = parse_command(
            r#"{"v":1,"type":"start","req":7,"vocabulary":["小麥注音","OpenCode"],"convert":"s2twp"}"#,
        )
        .unwrap();
        match command {
            Command::Start {
                req,
                language,
                vocabulary,
                convert,
                model,
                mode,
                target,
            } => {
                assert_eq!(req, 7);
                assert_eq!(language, "zh");
                assert_eq!(vocabulary, vec!["小麥注音", "OpenCode"]);
                assert_eq!(convert, "s2twp");
                assert!(model.is_none());
                assert_eq!(mode, Mode::Dictate, "old clients dictate");
                assert_eq!(target, "en");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn events_are_one_object() {
        let line = serde_json::to_string(&final_text(3, "你好")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "final");
        assert_eq!(value["text"], "你好");
        assert_eq!(value["req"], 3);
    }

    #[test]
    fn model_list_is_version_1() {
        let command = parse_command(r#"{"v":1,"type":"model.list","req":3}"#).unwrap();
        assert!(matches!(command, Command::ModelList { req: 3 }));
        let command =
            parse_command(r#"{"v":1,"type":"model.select","req":4,"model":"whisper-tiny"}"#)
                .unwrap();
        match command {
            Command::ModelSelect { req, model } => {
                assert_eq!(req, 4);
                assert_eq!(model, "whisper-tiny");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn start_translate_mode_and_target() {
        let command = parse_command(
            r#"{"v":1,"type":"start","req":9,"convert":"s2twp","mode":"translate","target":"ja"}"#,
        )
        .unwrap();
        match command {
            Command::Start { mode, target, .. } => {
                assert_eq!(mode, Mode::Translate);
                assert_eq!(target, "ja");
            }
            other => panic!("unexpected {other:?}"),
        }
        let command = parse_command(r#"{"v":1,"type":"start","req":1,"mode":"dictate"}"#).unwrap();
        assert!(matches!(
            command,
            Command::Start {
                mode: Mode::Dictate,
                ..
            }
        ));
        assert!(parse_command(r#"{"v":1,"type":"start","req":1,"mode":"sing"}"#).is_err());
    }

    #[test]
    fn translated_final_carries_source_and_error() {
        let ok = final_translation(4, "Hello", "en", true, "你好", None);
        assert_eq!(ok["type"], "final");
        assert_eq!(ok["text"], "Hello");
        assert_eq!(ok["mode"], "translate");
        assert_eq!(ok["translated"], true);
        assert_eq!(ok["source"], "你好");
        assert!(ok.get("translate_error").is_none());
        let failed = final_translation(5, "你好", "en", false, "你好", Some("plugin-missing"));
        assert_eq!(failed["translated"], false);
        assert_eq!(failed["text"], "你好");
        assert_eq!(failed["translate_error"], "plugin-missing");
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(parse_command("not json").is_err());
        assert!(parse_command(r#"{"v":9,"type":"ping"}"#).is_err());
        assert!(parse_command(r#"{"v":1,"type":"nope"}"#).is_err());
    }
}
