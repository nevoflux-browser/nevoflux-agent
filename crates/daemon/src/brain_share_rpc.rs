//! Browser-facing `brain.share_*` RPC handlers (M5-B) — the online
//! `.nbrain` delivery surface. Mirrors [`crate::brain_rpc`]'s envelope
//! conventions and disabled-path behavior.

use std::sync::Arc;
use std::sync::OnceLock;

use nevoflux_brain::{BrainEngine, ImportTrust, Selection, StripRules};
use serde_json::Value;

use crate::brain_share::BrainShareService;
use crate::init_brain::CURRENT_BRAIN_SLOT;
use crate::kb_wizard::{err_response, ok_response};

/// Process-global handle to the live [`BrainShareService`], populated in
/// `server.rs` during boot. Empty until then (mirrors `CURRENT_BRAIN_SLOT`).
pub static CURRENT_BRAIN_SHARE_SLOT: OnceLock<Arc<BrainShareService>> = OnceLock::new();

async fn current_engine() -> Option<Arc<dyn BrainEngine>> {
    let slot = CURRENT_BRAIN_SLOT.get()?;
    let guard = slot.read().await;
    guard.as_ref().map(|boot| boot.engine.clone())
}

fn current_service() -> Option<Arc<BrainShareService>> {
    CURRENT_BRAIN_SHARE_SLOT.get().cloned()
}

fn disabled(request_id: &str, command: &str) -> Value {
    err_response(
        request_id,
        command,
        "BRAIN_DISABLED",
        "knowledge_base.brain is not enabled or the daemon hasn't \
         finished initializing the brain yet",
    )
}

fn req_id(params: &Value) -> &str {
    params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

/// Parse a `Selection` from `{ files?: [..], directory?: ".." }`.
fn parse_selection(params: &Value) -> Selection {
    let files: Vec<String> = params
        .get("files")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let dir = params
        .get("directory")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    match (files.is_empty(), dir) {
        (true, Some(d)) => Selection::Directory(d.to_string()),
        (false, Some(d)) => Selection::Mixed {
            files,
            directories: vec![d.to_string()],
        },
        (false, None) => Selection::Files(files),
        (true, None) => Selection::Directory(String::new()), // whole brain
    }
}

/// Parse `StripRules` overrides; defaults are privacy-safe (Spec A).
fn parse_rules(params: &Value) -> StripRules {
    let mut rules = StripRules::default();
    if let Some(v) = params.get("compiled_only").and_then(|v| v.as_bool()) {
        rules.compiled_only = v;
    }
    if let Some(arr) = params
        .get("frontmatter_whitelist")
        .and_then(|v| v.as_array())
    {
        rules.frontmatter_whitelist = arr
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
    }
    rules
}

/// `brain.share_create` — export selection, upload, return share URL (#key).
pub async fn handle_share_create(params: &Value) -> Value {
    let rid = req_id(params);
    let Some(engine) = current_engine().await else {
        return disabled(rid, "brain.share_create");
    };
    let Some(svc) = current_service() else {
        return disabled(rid, "brain.share_create");
    };
    let sel = parse_selection(params);
    let rules = parse_rules(params);
    let title = params.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let ttl = params.get("ttl_secs").and_then(|v| v.as_u64());

    match svc.create(&engine, sel, rules, title, ttl).await {
        Ok(r) => ok_response(
            rid,
            "brain.share_create",
            serde_json::json!({
                "share_id": r.share_id,
                "share_url": r.share_url,
                "expires_at": r.expires_at,
                "size_bytes": r.size_bytes,
            }),
        ),
        Err(e) => err_response(
            rid,
            "brain.share_create",
            "BRAIN_SHARE_ERROR",
            format!("create failed: {e}"),
        ),
    }
}

/// `brain.share_import_url` — fetch + import from a share URL (#key).
pub async fn handle_share_import_url(params: &Value) -> Value {
    let rid = req_id(params);
    let url = match params.get("url").and_then(|v| v.as_str()) {
        Some(u) if !u.is_empty() => u.to_string(),
        _ => {
            return err_response(
                rid,
                "brain.share_import_url",
                "BAD_REQUEST",
                "missing or empty `url`",
            )
        }
    };
    let source_name = params
        .get("source_name")
        .and_then(|v| v.as_str())
        .unwrap_or("shared");
    let trust = match params.get("trust").and_then(|v| v.as_str()) {
        Some("full_merge") => ImportTrust::FullMerge,
        _ => ImportTrust::ReadOnly,
    };
    let Some(engine) = current_engine().await else {
        return disabled(rid, "brain.share_import_url");
    };
    let Some(svc) = current_service() else {
        return disabled(rid, "brain.share_import_url");
    };
    match svc.import_url(&engine, &url, source_name, trust).await {
        Ok(report) => ok_response(
            rid,
            "brain.share_import_url",
            serde_json::json!({ "files_imported": report.files_imported, "conflicts": report.conflicts }),
        ),
        Err(e) => err_response(
            rid,
            "brain.share_import_url",
            "BRAIN_SHARE_ERROR",
            format!("import failed: {e}"),
        ),
    }
}

/// `brain.share_list` — list the user's locally-recorded brain shares.
pub async fn handle_share_list(params: &Value) -> Value {
    let rid = req_id(params);
    let Some(svc) = current_service() else {
        return disabled(rid, "brain.share_list");
    };
    match svc.list() {
        Ok(rows) => {
            let shares: Vec<Value> = rows
                .into_iter()
                .map(|r| {
                    serde_json::json!({
                        "share_id": r.share_id,
                        "share_url": r.share_url,
                        "title": r.title,
                        "expires_at": r.expires_at,
                        "size_bytes": r.size_bytes,
                        "created_at": r.created_at,
                    })
                })
                .collect();
            ok_response(
                rid,
                "brain.share_list",
                serde_json::json!({ "shares": shares }),
            )
        }
        Err(e) => err_response(
            rid,
            "brain.share_list",
            "BRAIN_SHARE_ERROR",
            format!("list failed: {e}"),
        ),
    }
}

/// `brain.share_renew` — extend a share's TTL.
pub async fn handle_share_renew(params: &Value) -> Value {
    let rid = req_id(params);
    let share_id = match params.get("share_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return err_response(
                rid,
                "brain.share_renew",
                "BAD_REQUEST",
                "missing or empty `share_id`",
            )
        }
    };
    let extend_secs = params
        .get("extend_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(30 * 24 * 3600);
    let Some(svc) = current_service() else {
        return disabled(rid, "brain.share_renew");
    };
    match svc.renew(&share_id, extend_secs).await {
        Ok(new_expires) => ok_response(
            rid,
            "brain.share_renew",
            serde_json::json!({ "expires_at": new_expires }),
        ),
        Err(e) => err_response(
            rid,
            "brain.share_renew",
            "BRAIN_SHARE_ERROR",
            format!("renew failed: {e}"),
        ),
    }
}

/// `brain.share_revoke` — revoke a share server-side + locally.
pub async fn handle_share_revoke(params: &Value) -> Value {
    let rid = req_id(params);
    let share_id = match params.get("share_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return err_response(
                rid,
                "brain.share_revoke",
                "BAD_REQUEST",
                "missing or empty `share_id`",
            )
        }
    };
    let Some(svc) = current_service() else {
        return disabled(rid, "brain.share_revoke");
    };
    match svc.revoke(&share_id).await {
        Ok(()) => ok_response(
            rid,
            "brain.share_revoke",
            serde_json::json!({ "revoked": true }),
        ),
        Err(e) => err_response(
            rid,
            "brain.share_revoke",
            "BRAIN_SHARE_ERROR",
            format!("revoke failed: {e}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // CURRENT_BRAIN_SLOT / CURRENT_BRAIN_SHARE_SLOT are never populated in
    // the test binary, so every handler takes the disabled / bad-request
    // branch. Happy paths require a live engine + Worker and are deferred
    // to the Task 7 live test.

    #[tokio::test]
    async fn share_create_disabled_when_brain_off() {
        let resp = handle_share_create(&serde_json::json!({ "request_id": "t1" })).await;
        assert_eq!(resp["payload"]["error"]["code"], "BRAIN_DISABLED");
    }

    #[tokio::test]
    async fn import_url_bad_request_when_url_missing() {
        let resp = handle_share_import_url(&serde_json::json!({ "request_id": "t2" })).await;
        assert_eq!(resp["payload"]["error"]["code"], "BAD_REQUEST");
    }

    #[tokio::test]
    async fn share_list_disabled_when_service_off() {
        let resp = handle_share_list(&serde_json::json!({ "request_id": "t3" })).await;
        assert_eq!(resp["payload"]["error"]["code"], "BRAIN_DISABLED");
    }

    #[tokio::test]
    async fn share_renew_bad_request_when_id_missing() {
        let resp = handle_share_renew(&serde_json::json!({ "request_id": "t4" })).await;
        assert_eq!(resp["payload"]["error"]["code"], "BAD_REQUEST");
    }

    #[tokio::test]
    async fn share_revoke_bad_request_when_id_missing() {
        let resp = handle_share_revoke(&serde_json::json!({ "request_id": "t5" })).await;
        assert_eq!(resp["payload"]["error"]["code"], "BAD_REQUEST");
    }

    #[test]
    fn parse_selection_whole_brain_default() {
        let sel = parse_selection(&serde_json::json!({}));
        assert!(matches!(sel, Selection::Directory(d) if d.is_empty()));
    }

    #[test]
    fn parse_selection_files_only() {
        let sel = parse_selection(&serde_json::json!({ "files": ["concepts/yc"] }));
        assert!(matches!(sel, Selection::Files(f) if f == vec!["concepts/yc".to_string()]));
    }
}
