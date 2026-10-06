//! The settings page's Jev section (spec §5.9): `jev.get` / `jev.set` read
//! and write `[jev]`, and `jev.test` is "test connection" — three tiny
//! requests on one connection, the median latency, and a `timeout_ms`
//! suggestion.

use std::time::{Duration, Instant};

use super::client::{JevClient, JevError};
use super::wire::Question;
use crate::kb_wizard::{err_response, ok_response};
use crate::server::SharedAgentConfig;

const CMD: &str = "jev.test";

/// Decision points the page can switch (spec §5.9 item 4).
const POINTS: &[&str] = &["tools", "skills", "visibility", "rebuild", "permissions"];

/// A key as the page may see it: `abc...wxyz`, `****` when short, `""` when
/// none. The full key never leaves the daemon.
fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    match chars.len() {
        0 => String::new(),
        1..=4 => "****".into(),
        n => format!(
            "{}...{}",
            chars[..3].iter().collect::<String>(),
            chars[n - 4..].iter().collect::<String>()
        ),
    }
}

/// `https://`, or `http://` to this machine only (the client's own rule).
fn valid_endpoint(endpoint: &str) -> bool {
    let lower = endpoint.trim().to_ascii_lowercase();
    lower.starts_with("https://")
        || (lower.starts_with("http://") && crate::local::latch::is_loopback_url(endpoint.trim()))
}

/// Whether Jev's context work applies with the active provider (spec §5.9
/// item 6): `local` (on-device), `acp` (the provider runs its own loop), or
/// `applies`.
fn scope(cfg: &crate::config::AgentConfig) -> &'static str {
    let wire = cfg
        .llm
        .active_provider()
        .and_then(|p| cfg.llm.resolve_wire(p));
    if wire == Some(nevoflux_llm::ProviderType::Local) {
        "local"
    } else if cfg.llm.active_provider_is_acp() {
        "acp"
    } else {
        "applies"
    }
}

/// The `[jev]` section as the settings page sees it. `env_key` is
/// `NEVOFLUX_API_KEY_TYPESAFE`, reported only as present or not.
pub fn get_data(cfg: &crate::config::AgentConfig, env_key: Option<&str>) -> serde_json::Value {
    let j = &cfg.jev;
    serde_json::json!({
        "enabled": j.enabled,
        "endpoint": j.endpoint,
        "model": j.model,
        "timeout_ms": j.timeout_ms,
        "points": {
            "tools": j.points.tools,
            "skills": j.points.skills,
            "visibility": j.points.visibility,
            "rebuild": j.points.rebuild,
            "permissions": j.points.permissions,
        },
        "sensitive_domains": j.sensitive_domains,
        "has_api_key": !j.api_key.is_empty(),
        "api_key": mask(&j.api_key),
        "key_from_env": j.api_key.is_empty() && env_key.is_some_and(|k| !k.is_empty()),
        "builtin_sensitive_domains": super::privacy::BUILTIN_SENSITIVE_DOMAINS,
        "intranet_suffixes": super::privacy::INTRANET_SUFFIXES,
        "scope": scope(cfg),
    })
}

/// Apply `jev.set` params to `cfg` (all optional). An absent or empty
/// `api_key` keeps the stored key, `null` clears it; the env key is never
/// written. Errors are (code, message).
pub fn apply_set(
    cfg: &mut crate::config::AgentConfig,
    params: &serde_json::Value,
) -> Result<(), (&'static str, String)> {
    let j = &mut cfg.jev;
    if let Some(v) = params.get("enabled") {
        j.enabled = v
            .as_bool()
            .ok_or(("bad_enabled", "enabled must be true or false".to_string()))?;
    }
    if let Some(v) = params.get("endpoint") {
        let e = v.as_str().unwrap_or("").trim();
        if !valid_endpoint(e) {
            return Err((
                "bad_endpoint",
                "use https://, or http:// to this machine".into(),
            ));
        }
        j.endpoint = e.to_string();
    }
    match params.get("api_key") {
        None => {}
        Some(serde_json::Value::Null) => j.api_key.clear(),
        Some(serde_json::Value::String(k)) if k.trim().is_empty() => {}
        Some(serde_json::Value::String(k)) => j.api_key = k.trim().to_string(),
        Some(_) => return Err(("bad_key", "api_key must be a string or null".into())),
    }
    if let Some(v) = params.get("timeout_ms") {
        match v.as_u64() {
            Some(ms) if (100..=10_000).contains(&ms) => j.timeout_ms = ms,
            _ => return Err(("bad_timeout", "between 100 and 10000 ms".into())),
        }
    }
    if let Some(v) = params.get("points") {
        let obj = v
            .as_object()
            .ok_or(("bad_points", "points must be an object".to_string()))?;
        for (k, v) in obj {
            let on = v
                .as_bool()
                .ok_or(("bad_points", format!("{k} must be true or false")))?;
            match k.as_str() {
                "tools" => j.points.tools = on,
                "skills" => j.points.skills = on,
                "visibility" => j.points.visibility = on,
                "rebuild" => j.points.rebuild = on,
                "permissions" => j.points.permissions = on,
                _ => {
                    return Err((
                        "bad_points",
                        format!("unknown point {k}; expected one of {}", POINTS.join(", ")),
                    ))
                }
            }
        }
    }
    if let Some(v) = params.get("sensitive_domains") {
        let list = v.as_array().ok_or((
            "bad_domains",
            "sensitive_domains must be a list".to_string(),
        ))?;
        let mut out: Vec<String> = Vec::new();
        for d in list {
            let d = d
                .as_str()
                .ok_or(("bad_domains", "each domain must be a string".to_string()))?;
            let n = super::privacy::normalise_domain(d);
            if !n.is_empty() && !out.contains(&n) {
                out.push(n);
            }
        }
        j.sensitive_domains = out;
    }
    Ok(())
}

/// `jev.get`.
pub fn handle_get(
    params: &serde_json::Value,
    shared_config: &SharedAgentConfig,
) -> serde_json::Value {
    let id = crate::local::rpc::request_id(params);
    let cfg = shared_config
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let env = std::env::var(crate::config::JEV_KEY_ENV).ok();
    ok_response(&id, "jev.get", get_data(&cfg, env.as_deref()))
}

/// `jev.set`: validate, save `config.toml`, swap the running config.
pub fn handle_set(
    params: &serde_json::Value,
    shared_config: &SharedAgentConfig,
) -> serde_json::Value {
    let id = crate::local::rpc::request_id(params);
    let Ok(path) = crate::config::AgentConfig::default_config_path() else {
        return err_response(
            &id,
            "jev.set",
            "config_error",
            "could not resolve the config file path",
        );
    };
    set_with_path(params, shared_config, &path)
}

/// [`handle_set`] against the config file at `path`.
pub fn set_with_path(
    params: &serde_json::Value,
    shared_config: &SharedAgentConfig,
    path: &std::path::PathBuf,
) -> serde_json::Value {
    let id = crate::local::rpc::request_id(params);
    let cmd = "jev.set";
    // Load, change and save as one step: the settings page saves on every
    // change, and two overlapping saves would otherwise undo each other or
    // read the file half-written.
    static SET_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = SET_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut cfg = match crate::config::AgentConfig::load_from_path(path) {
        Ok(c) => c,
        Err(e) => {
            return err_response(
                &id,
                cmd,
                "config_error",
                format!("failed to load config: {e}"),
            )
        }
    };
    if let Err((code, message)) = apply_set(&mut cfg, params) {
        return err_response(&id, cmd, code, message);
    }
    if let Err(e) = cfg.save_to_path(path) {
        return err_response(
            &id,
            cmd,
            "config_error",
            format!("failed to save config: {e}"),
        );
    }
    *shared_config.write().unwrap_or_else(|e| e.into_inner()) = std::sync::Arc::new(cfg.clone());
    let env = std::env::var(crate::config::JEV_KEY_ENV).ok();
    ok_response(&id, cmd, get_data(&cfg, env.as_deref()))
}

/// Twice the median latency, rounded up to 100 ms, never below the spec's
/// default of 800 ms.
pub fn suggested_timeout_ms(p50_ms: u64) -> u64 {
    let doubled = p50_ms.saturating_mul(2);
    let rounded = doubled.div_ceil(100) * 100;
    rounded.max(800)
}

pub async fn handle_test(
    params: &serde_json::Value,
    shared_config: &SharedAgentConfig,
) -> serde_json::Value {
    let request_id = crate::local::rpc::request_id(params);
    let mut jev = shared_config
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .jev
        .clone();
    // The page tests what is typed, before saving or turning Jev on; the
    // test changes nothing saved.
    if let Some(e) = params
        .get("endpoint")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        jev.endpoint = e.to_string();
    }
    if let Some(k) = params
        .get("api_key")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        jev.api_key = k.to_string();
    }
    jev.enabled = true;
    let client = match JevClient::from_config(&jev) {
        Ok(c) => c,
        Err(_) => {
            return err_response(
                &request_id,
                CMD,
                "NOT_CONFIGURED",
                "Jev is disabled or has no endpoint/key",
            )
        }
    };
    let mut questions = std::collections::BTreeMap::new();
    questions.insert(
        "ping".to_string(),
        Question::Noul {
            instructions: "Is this a connection test?".into(),
            when_true: "yes".into(),
            when_false: "no".into(),
        },
    );
    let mut latencies = Vec::new();
    for _ in 0..3 {
        let t = Instant::now();
        match client
            .ask(
                serde_json::json!({"purpose": "connection test"}),
                questions.clone(),
                Duration::from_secs(10),
            )
            .await
        {
            Ok(_) => latencies.push(t.elapsed().as_millis() as u64),
            Err(JevError::Refused(m)) => return err_response(&request_id, CMD, "REFUSED", m),
            Err(e) => return err_response(&request_id, CMD, "FAILED", e.to_string()),
        }
    }
    let mut sorted = latencies.clone();
    sorted.sort_unstable();
    let p50 = sorted[1];
    ok_response(
        &request_id,
        CMD,
        serde_json::json!({
            "latency_ms": latencies,
            "p50_ms": p50,
            "suggested_timeout_ms": suggested_timeout_ms(p50),
            "model": jev.model,
        }),
    )
}

#[cfg(test)]
mod tests {
    use crate::config::AgentConfig;
    use serde_json::json;

    #[test]
    fn concurrent_saves_lose_no_change() {
        // The settings page saves on every change; two saves that overlap
        // must not undo each other (load-modify-save is one step).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        AgentConfig::default().save_to_path(&path).unwrap();
        let shared: crate::server::SharedAgentConfig = std::sync::Arc::new(std::sync::RwLock::new(
            std::sync::Arc::new(AgentConfig::default()),
        ));
        let writers: Vec<(&str, Box<dyn Fn(usize) -> serde_json::Value + Send + Sync>)> = vec![
            ("timeout", Box::new(|i| json!({"timeout_ms": 1000 + i}))),
            (
                "tools",
                Box::new(|i| json!({"points": {"tools": i % 2 == 0}})),
            ),
            (
                "skills",
                Box::new(|i| json!({"points": {"skills": i % 2 == 1}})),
            ),
            (
                "domains",
                Box::new(|i| json!({"sensitive_domains": [format!("d{i}.example")]})),
            ),
            (
                "key",
                Box::new(|i| json!({"api_key": format!("sk-{i:04}")})),
            ),
        ];
        const N: usize = 60;
        std::thread::scope(|scope| {
            for (_, f) in &writers {
                let (path, shared) = (&path, &shared);
                scope.spawn(move || {
                    for i in 0..N {
                        let r = set_with_path(&f(i), shared, path);
                        assert_eq!(r["payload"]["success"], true, "{r}");
                    }
                });
            }
        });
        let last = N - 1;
        let saved = AgentConfig::load_from_path(&path).unwrap().jev;
        assert_eq!(saved.timeout_ms, 1000 + last as u64);
        assert_eq!(saved.points.tools, last % 2 == 0);
        assert_eq!(saved.points.skills, last % 2 == 1);
        assert_eq!(saved.sensitive_domains, vec![format!("d{last}.example")]);
        assert_eq!(saved.api_key, format!("sk-{last:04}"));
    }

    fn cfg_with_key(key: &str) -> AgentConfig {
        let mut c = AgentConfig::default();
        c.jev.api_key = key.into();
        c
    }

    #[test]
    fn get_masks_the_key_and_lists_the_builtin_sites() {
        let d = get_data(&cfg_with_key("sk-1234567890abcd"), None);
        assert_eq!(d["has_api_key"], true);
        assert_eq!(d["api_key"], "sk-...abcd");
        assert!(!d.to_string().contains("1234567890"));
        assert!(d["builtin_sensitive_domains"].as_array().unwrap().len() > 10);
        assert!(!d["intranet_suffixes"].as_array().unwrap().is_empty());
        assert_eq!(d["points"]["permissions"], false);
        assert_eq!(d["enabled"], false);
    }

    #[test]
    fn get_reports_an_env_key_without_revealing_it() {
        let d = get_data(&cfg_with_key(""), Some("sk-from-env-9999"));
        assert_eq!(
            (d["has_api_key"].as_bool(), d["key_from_env"].as_bool()),
            (Some(false), Some(true))
        );
        assert!(!d.to_string().contains("9999"));
    }

    #[test]
    fn get_reports_the_provider_scope() {
        let mut c = AgentConfig::default();
        c.llm.provider = Some("anthropic".into());
        assert_eq!(get_data(&c, None)["scope"], "applies");
        c.llm.provider = Some("claude-code".into());
        assert_eq!(get_data(&c, None)["scope"], "acp");
        c.llm.provider = Some("local".into());
        assert_eq!(get_data(&c, None)["scope"], "local");
    }

    #[test]
    fn set_applies_and_validates() {
        let mut c = AgentConfig::default();
        apply_set(
            &mut c,
            &json!({"enabled": true, "timeout_ms": 1200,
                "points": {"permissions": true, "tools": false},
                "sensitive_domains": ["*.Bank.Example/", "", "bank.example"]}),
        )
        .unwrap();
        assert!(c.jev.enabled);
        assert_eq!(c.jev.timeout_ms, 1200);
        assert!(c.jev.points.permissions && !c.jev.points.tools && c.jev.points.skills);
        assert_eq!(c.jev.sensitive_domains, vec!["bank.example".to_string()]);
        assert_eq!(
            apply_set(&mut c, &json!({"timeout_ms": 50})).unwrap_err().0,
            "bad_timeout"
        );
        assert_eq!(
            apply_set(&mut c, &json!({"endpoint": "http://jev.example.com/v1"}))
                .unwrap_err()
                .0,
            "bad_endpoint"
        );
        assert_eq!(
            apply_set(&mut c, &json!({"points": {"nonsense": true}}))
                .unwrap_err()
                .0,
            "bad_points"
        );
        apply_set(&mut c, &json!({"endpoint": "http://127.0.0.1:9000/v1"})).unwrap();
        assert_eq!(c.jev.endpoint, "http://127.0.0.1:9000/v1");
    }

    #[test]
    fn an_absent_key_keeps_the_stored_one() {
        let mut c = cfg_with_key("sk-keep");
        apply_set(&mut c, &json!({"timeout_ms": 900})).unwrap();
        apply_set(&mut c, &json!({"api_key": ""})).unwrap();
        assert_eq!(c.jev.api_key, "sk-keep");
        apply_set(&mut c, &json!({"api_key": "sk-new"})).unwrap();
        assert_eq!(c.jev.api_key, "sk-new");
        apply_set(&mut c, &json!({"api_key": null})).unwrap();
        assert_eq!(c.jev.api_key, "");
    }

    #[test]
    fn set_never_writes_the_env_key() {
        let mut c = cfg_with_key("");
        apply_set(&mut c, &json!({"enabled": true})).unwrap();
        assert_eq!(c.jev.api_key, "");
    }

    #[tokio::test]
    async fn test_uses_unsaved_endpoint_and_key() {
        // Saved config: Jev disabled, no key. The test params carry both.
        let (url, bodies) = crate::jev::test_support::answering(
            json!({"answers": {"ping": {"noul": 0.9}}, "usage": {"input_tokens": 1, "output_tokens": 1}}),
            std::time::Duration::ZERO,
        )
        .await;
        let shared: crate::server::SharedAgentConfig = std::sync::Arc::new(std::sync::RwLock::new(
            std::sync::Arc::new(AgentConfig::default()),
        ));
        let r = handle_test(
            &json!({"request_id": "r1", "endpoint": url, "api_key": "k"}),
            &shared,
        )
        .await;
        assert_eq!(r["payload"]["success"], true, "{r}");
        assert_eq!(bodies.lock().unwrap().len(), 3);
        assert!(!shared.read().unwrap().jev.enabled, "a test saves nothing");
    }

    use super::*;

    /// Opt-in live check against the real System One endpoint. Needs
    /// `NEVOFLUX_API_KEY_TYPESAFE`; sends only public data; prints latency and
    /// the raw shape of one Score answer, never the key.
    ///
    /// `cargo test -j 3 -p nevoflux-daemon --lib jev_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn jev_live_test_connection() {
        let mut cfg = crate::config::AgentConfig::default();
        cfg.jev.enabled = true;
        if cfg.jev.resolved_api_key().is_none() {
            eprintln!("skipped: {} is not set", crate::config::JEV_KEY_ENV);
            return;
        }
        let jev = cfg.jev.clone();
        let shared = std::sync::Arc::new(std::sync::RwLock::new(std::sync::Arc::new(cfg)));
        let resp = handle_test(&serde_json::json!({"request_id": "live"}), &shared).await;
        let p = &resp["payload"];
        println!(
            "jev.test success={} latency_ms={} p50_ms={} suggested_timeout_ms={} error={}",
            p["success"],
            p["data"]["latency_ms"],
            p["data"]["p50_ms"],
            p["data"]["suggested_timeout_ms"],
            p["error"]["code"]
        );
        assert_eq!(p["success"], true);

        let client = JevClient::from_config(&jev).unwrap();
        let mut q = std::collections::BTreeMap::new();
        q.insert(
            "steps".to_string(),
            Question::Score {
                instructions: "How many browser steps will this task take?".into(),
                levels: vec![
                    "1-2 steps".into(),
                    "3-5 steps".into(),
                    "6-10 steps".into(),
                    "more than 10 steps".into(),
                ],
            },
        );
        let r = client
            .ask(
                serde_json::json!({"query": "read a Wikipedia article about Rust"}),
                q,
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        println!("score answer: {:?}", r.answer_for("steps"));
        println!(
            "usage: input={} output={}",
            r.usage.input_tokens, r.usage.output_tokens
        );
    }

    #[test]
    fn suggested_timeout_is_twice_the_median_and_never_below_the_default() {
        assert_eq!(suggested_timeout_ms(260), 800);
        assert_eq!(suggested_timeout_ms(400), 800);
        assert_eq!(suggested_timeout_ms(451), 1000);
        assert_eq!(suggested_timeout_ms(700), 1400);
    }

    #[tokio::test]
    async fn testing_with_jev_disabled_sends_nothing() {
        let cfg = crate::config::AgentConfig::default(); // jev disabled
        let shared = std::sync::Arc::new(std::sync::RwLock::new(std::sync::Arc::new(cfg)));
        let resp = handle_test(&serde_json::json!({"request_id": "r1"}), &shared).await;
        assert_eq!(resp["payload"]["success"], false);
        assert_eq!(resp["payload"]["error"]["code"], "NOT_CONFIGURED");
    }

    #[tokio::test]
    async fn testing_reports_latency_and_a_timeout_suggestion() {
        let (url, bodies) = crate::jev::test_support::answering(
            serde_json::json!({"answers": {"ping": {"noul": 0.5}}, "usage": {"input_tokens": 9, "output_tokens": 1}}),
            std::time::Duration::ZERO,
        )
        .await;
        let mut cfg = crate::config::AgentConfig::default();
        cfg.jev.enabled = true;
        cfg.jev.endpoint = url;
        cfg.jev.api_key = "k".into();
        let shared = std::sync::Arc::new(std::sync::RwLock::new(std::sync::Arc::new(cfg)));
        let resp = handle_test(&serde_json::json!({"request_id": "r2"}), &shared).await;
        assert_eq!(resp["payload"]["success"], true, "{resp}");
        assert_eq!(
            resp["payload"]["data"]["latency_ms"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert!(
            resp["payload"]["data"]["suggested_timeout_ms"]
                .as_u64()
                .unwrap()
                >= 800
        );
        assert_eq!(bodies.lock().unwrap().len(), 3);
    }
}
