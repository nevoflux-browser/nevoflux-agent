//! `meeting.*` system commands for `nevoflux://meeting` and the settings page.
//!
//! - `meeting.list` → `{meetings:[Listing], live:[id]}`
//! - `meeting.get {id}` → `{meta, transcript, summary, live}`
//! - `meeting.audio {id, seg}` → `{mime, base64}`
//! - `meeting.delete {id}`
//! - `meeting.set_speaker {id, seg, who}`
//! - `meeting.settings.get` → `{settings, clean_model, summary_model}`, each
//!   model as `{name, cloud}` or `{error}`
//! - `meeting.settings.set {settings}`

use base64::Engine as _;
use serde_json::{json, Value};

use crate::kb_wizard::{err_response, ok_response};

fn request_id(params: &Value) -> &str {
    params.get("request_id").and_then(Value::as_str).unwrap_or("")
}

fn str_arg<'a>(params: &'a Value, k: &str) -> Option<&'a str> {
    params.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

pub async fn handle(cmd: &str, params: &Value) -> Value {
    let rid = request_id(params);
    let m = super::global();
    match cmd {
        "meeting.list" => {
            let store = m.store().clone();
            let list = tokio::task::spawn_blocking(move || store.list()).await.unwrap_or_default();
            ok_response(rid, cmd, json!({ "meetings": list, "live": m.live_ids().await }))
        }
        "meeting.get" => {
            let Some(id) = str_arg(params, "id") else {
                return err_response(rid, cmd, "BAD_REQUEST", "id is required");
            };
            let Some(meta) = m.store().meta(id) else {
                return err_response(rid, cmd, "NOT_FOUND", "no such meeting");
            };
            let live = m.live_ids().await.iter().any(|l| l == id);
            ok_response(
                rid,
                cmd,
                json!({
                    "meta": meta,
                    "transcript": m.store().transcript(id),
                    "summary": m.store().summary(id),
                    "live": live,
                }),
            )
        }
        "meeting.audio" => {
            let (Some(id), Some(seg)) = (str_arg(params, "id"), params.get("seg").and_then(Value::as_u64)) else {
                return err_response(rid, cmd, "BAD_REQUEST", "id and seg are required");
            };
            match m.store().audio_path(id, seg as u32).and_then(|p| std::fs::read(p).ok()) {
                Some(bytes) => ok_response(
                    rid,
                    cmd,
                    json!({ "mime": "audio/ogg; codecs=opus", "base64": base64::engine::general_purpose::STANDARD.encode(bytes) }),
                ),
                None => err_response(rid, cmd, "NOT_FOUND", "this sentence's audio is gone"),
            }
        }
        "meeting.delete" => {
            let Some(id) = str_arg(params, "id") else {
                return err_response(rid, cmd, "BAD_REQUEST", "id is required");
            };
            match m.delete(id).await {
                Ok(found) => ok_response(rid, cmd, json!({ "deleted": found })),
                Err(e) => err_response(rid, cmd, "IO_ERROR", e.to_string()),
            }
        }
        "meeting.set_speaker" => {
            let (Some(id), Some(seg), Some(who)) =
                (str_arg(params, "id"), params.get("seg").and_then(Value::as_u64), str_arg(params, "who"))
            else {
                return err_response(rid, cmd, "BAD_REQUEST", "id, seg and who are required");
            };
            if !who.chars().all(|c| c.is_alphanumeric() || c == '_') || who.chars().count() > 32 {
                return err_response(rid, cmd, "BAD_REQUEST", "who is me, other or a speaker label");
            }
            if m.set_speaker(id, seg as u32, who).await {
                ok_response(rid, cmd, json!({}))
            } else {
                err_response(rid, cmd, "NOT_FOUND", "no such sentence")
            }
        }
        "meeting.settings.get" => {
            let s = m.settings();
            let config = crate::config::AgentConfig::load().unwrap_or_default();
            let describe = |p: &Option<String>, mo: &Option<String>| match super::llm::resolve(&config, p.as_deref(), mo.as_deref()) {
                Ok(c) => json!({ "name": format!("{}/{}", c.provider, c.model), "cloud": super::llm::is_cloud(&c) }),
                Err(e) => json!({ "error": e }),
            };
            ok_response(
                rid,
                cmd,
                json!({
                    "settings": s,
                    "clean_model": describe(&s.clean_provider, &s.clean_model),
                    "summary_model": describe(&s.summary_provider, &s.summary_model),
                }),
            )
        }
        "meeting.settings.set" => {
            let Some(v) = params.get("settings") else {
                return err_response(rid, cmd, "BAD_REQUEST", "settings is required");
            };
            let s: super::Settings = match serde_json::from_value(v.clone()) {
                Ok(s) => s,
                Err(e) => return err_response(rid, cmd, "BAD_REQUEST", e.to_string()),
            };
            if s.audio_days == Some(0) {
                return err_response(rid, cmd, "BAD_REQUEST", "keep audio at least a day, or forever");
            }
            match m.put_settings(&s) {
                Ok(()) => ok_response(rid, cmd, json!({ "settings": s })),
                Err(e) => err_response(rid, cmd, "IO_ERROR", e.to_string()),
            }
        }
        _ => err_response(rid, cmd, "UNKNOWN_COMMAND", format!("unknown command {cmd}")),
    }
}
