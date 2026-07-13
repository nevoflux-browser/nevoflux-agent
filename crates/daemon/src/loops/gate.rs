//! Deterministic loop gate evaluator (W3 §gate).
//!
//! A gate suppresses a loop iteration unless an observed value changes
//! (`http`/`bash`) or an event payload matches a predicate (`event`).
//! `GateKind::None` always fires. All network/exec is done through the
//! injectable [`Fetcher`] trait so the decision logic is unit-testable
//! without a real network or shell — see the `default impl` `DefaultFetcher`
//! for the production path, wired up in the loop dispatcher (later W3 task).
//!
//! Every evaluation path is fail-open: a missing/malformed spec, a failed
//! fetch, or a non-zero bash exit all resolve to `run=true` with `error`
//! set, rather than silently dropping an iteration or panicking.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use super::types::{GateKind, GateSpec};

/// Cap on `gate_output`/`new_last_value` length, in **characters** (not
/// bytes) — a byte-slice truncation on a multibyte string panics on a
/// split code point. See `feedback_html_tag_regex_word_boundary` project
/// memory for a prior incident in this codebase.
const MAX_GATE_VALUE_CHARS: usize = 16 * 1024;

/// Timeout applied to each individual `http_get` / `run_bash` call in
/// [`DefaultFetcher`].
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Result of evaluating a loop's gate for one trigger fire.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GateDecision {
    /// Whether the iteration should run.
    pub run: bool,
    /// Value handed to the iteration when it runs (http/bash extracted
    /// value, or the event payload as compact JSON). `None` for
    /// `GateKind::None` and for skipped iterations.
    pub gate_output: Option<String>,
    /// New `gate_last_value` to persist — only set for http/bash gates that
    /// ran (value-diff state). `event`/`none` never persist a last value.
    pub new_last_value: Option<String>,
    /// Set when this decision is a fail-open `run=true` triggered by an
    /// error (malformed spec, fetch failure, non-zero exit, timeout).
    pub error: Option<String>,
}

impl GateDecision {
    fn run_no_output() -> Self {
        Self {
            run: true,
            ..Default::default()
        }
    }

    fn skip() -> Self {
        Self {
            run: false,
            ..Default::default()
        }
    }

    fn fail_open(err: impl Into<String>) -> Self {
        Self {
            run: true,
            error: Some(err.into()),
            ..Default::default()
        }
    }

    fn ran_with_value(value: String) -> Self {
        Self {
            run: true,
            gate_output: Some(value.clone()),
            new_last_value: Some(value),
            error: None,
        }
    }
}

/// Injectable network/exec boundary for gate evaluation. Production code
/// uses [`DefaultFetcher`]; tests inject a stub so decisions are
/// deterministic and offline.
#[async_trait]
pub trait Fetcher: Send + Sync {
    /// Fetch `url` and return the response body as text.
    async fn http_get(&self, url: &str) -> Result<String, String>;
    /// Run `cmd` in a shell and return `(exit_code, stdout_trimmed)`.
    async fn run_bash(&self, cmd: &str) -> Result<(i32, String), String>;
}

/// Real [`Fetcher`] impl: `reqwest` GET / `tokio::process::Command`, each
/// wrapped in a 5s timeout.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultFetcher;

#[async_trait]
impl Fetcher for DefaultFetcher {
    async fn http_get(&self, url: &str) -> Result<String, String> {
        let fut = async {
            let resp = reqwest::get(url).await.map_err(|e| e.to_string())?;
            resp.text().await.map_err(|e| e.to_string())
        };
        match tokio::time::timeout(FETCH_TIMEOUT, fut).await {
            Ok(result) => result,
            Err(_) => Err(format!("http_get timed out after {FETCH_TIMEOUT:?}")),
        }
    }

    async fn run_bash(&self, cmd: &str) -> Result<(i32, String), String> {
        let mut command = if cfg!(target_os = "windows") {
            let mut c = tokio::process::Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-Command", cmd]);
            c
        } else {
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", cmd]);
            c
        };

        let output = match tokio::time::timeout(FETCH_TIMEOUT, command.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => return Err(format!("failed to spawn gate command: {e}")),
            Err(_) => return Err(format!("bash gate timed out after {FETCH_TIMEOUT:?}")),
        };

        let code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((code, stdout))
    }
}

/// Evaluate a loop's gate. `event_payload` is `Some` only for
/// event-triggered fires. `last_value` is the loop's stored
/// `gate_last_value`. Never panics; every error path is fail-open (see
/// module docs).
pub async fn evaluate_gate(
    spec: &GateSpec,
    last_value: Option<&str>,
    event_payload: Option<&Value>,
    fetcher: &dyn Fetcher,
) -> GateDecision {
    match spec.kind {
        GateKind::None => GateDecision::run_no_output(),
        GateKind::Http => evaluate_http(spec, last_value, fetcher).await,
        GateKind::Bash => evaluate_bash(spec, last_value, fetcher).await,
        GateKind::Event => evaluate_event(spec, event_payload),
    }
}

async fn evaluate_http(
    spec: &GateSpec,
    last_value: Option<&str>,
    fetcher: &dyn Fetcher,
) -> GateDecision {
    let Some(url) = spec.spec_json.get("url").and_then(Value::as_str) else {
        return GateDecision::fail_open("http gate spec missing 'url'");
    };
    let extract = spec.spec_json.get("extract").and_then(Value::as_str);

    let body = match fetcher.http_get(url).await {
        Ok(body) => body,
        Err(e) => return GateDecision::fail_open(format!("http gate fetch failed: {e}")),
    };

    value_diff_decision(extract_value(&body, extract), last_value)
}

async fn evaluate_bash(
    spec: &GateSpec,
    last_value: Option<&str>,
    fetcher: &dyn Fetcher,
) -> GateDecision {
    let Some(command) = spec.spec_json.get("command").and_then(Value::as_str) else {
        return GateDecision::fail_open("bash gate spec missing 'command'");
    };

    match fetcher.run_bash(command).await {
        Ok((0, stdout)) => value_diff_decision(stdout.trim().to_string(), last_value),
        Ok((code, _stdout)) => {
            GateDecision::fail_open(format!("bash gate command exited with status {code}"))
        }
        Err(e) => GateDecision::fail_open(format!("bash gate command failed: {e}")),
    }
}

fn evaluate_event(spec: &GateSpec, event_payload: Option<&Value>) -> GateDecision {
    let Some(payload) = event_payload else {
        return GateDecision::fail_open("event gate fired without an event payload");
    };
    let Some(path) = spec.spec_json.get("path").and_then(Value::as_str) else {
        return GateDecision::fail_open("event gate spec missing 'path'");
    };
    let Some(equals) = spec.spec_json.get("equals") else {
        return GateDecision::fail_open("event gate spec missing 'equals'");
    };

    let observed = walk_json_path(payload, path);
    if observed == Some(equals) {
        let json = serde_json::to_string(payload).unwrap_or_default();
        GateDecision {
            run: true,
            gate_output: Some(truncate_chars(&json, MAX_GATE_VALUE_CHARS)),
            new_last_value: None,
            error: None,
        }
    } else {
        GateDecision::skip()
    }
}

/// Value-diff semantics shared by `http` and `bash`: empty extracted value
/// skips; non-empty value that differs from `last_value` runs and persists;
/// non-empty value equal to `last_value` skips.
fn value_diff_decision(value: String, last_value: Option<&str>) -> GateDecision {
    if value.is_empty() {
        return GateDecision::skip();
    }
    let truncated = truncate_chars(&value, MAX_GATE_VALUE_CHARS);
    if last_value == Some(truncated.as_str()) {
        GateDecision::skip()
    } else {
        GateDecision::ran_with_value(truncated)
    }
}

/// Extract a value from an HTTP response body using either a JSONPath-lite
/// dot path (`$.a.b`) or a `/regex/`-delimited pattern (first capture group,
/// falling back to the whole match). Unrecognized syntax or an extraction
/// that fails (parse error, missing key, no match) yields an empty string,
/// which `value_diff_decision` treats as "skip". A `None` extractor uses
/// the whole trimmed body as the value.
fn extract_value(body: &str, extract: Option<&str>) -> String {
    let Some(pattern) = extract else {
        return body.trim().to_string();
    };

    if let Some(regex_src) = strip_regex_delims(pattern) {
        return extract_regex(body, regex_src);
    }
    if let Some(path) = pattern.strip_prefix("$.") {
        return extract_jsonpath(body, path);
    }
    String::new()
}

fn strip_regex_delims(pattern: &str) -> Option<&str> {
    if pattern.len() >= 2 && pattern.starts_with('/') && pattern.ends_with('/') {
        Some(&pattern[1..pattern.len() - 1])
    } else {
        None
    }
}

fn extract_regex(body: &str, pattern: &str) -> String {
    let Ok(re) = regex::Regex::new(pattern) else {
        return String::new();
    };
    let Some(caps) = re.captures(body) else {
        return String::new();
    };
    caps.get(1)
        .or_else(|| caps.get(0))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default()
}

fn extract_jsonpath(body: &str, path: &str) -> String {
    let Ok(root) = serde_json::from_str::<Value>(body) else {
        return String::new();
    };
    match walk_json_path(&root, path) {
        Some(v) => value_to_string(v),
        None => String::new(),
    }
}

/// Walk `root` by a dot-separated path (`a.b.c`); an empty path returns
/// `root` itself. Returns `None` if any segment is missing.
fn walk_json_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    if path.is_empty() {
        return Some(cur);
    }
    for key in path.split('.') {
        cur = cur.get(key)?;
    }
    Some(cur)
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Char-safe truncation (never splits a multibyte code point).
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        s.chars().take(max_chars).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Deterministic offline stub `Fetcher`. Construct with `http`/`bash` to
    /// canned exactly one response; evaluate_gate only ever calls one of
    /// the two methods per test.
    struct StubFetcher {
        http_result: Mutex<Option<Result<String, String>>>,
        bash_result: Mutex<Option<Result<(i32, String), String>>>,
    }

    impl StubFetcher {
        fn http(result: Result<String, String>) -> Self {
            Self {
                http_result: Mutex::new(Some(result)),
                bash_result: Mutex::new(None),
            }
        }

        fn bash(result: Result<(i32, String), String>) -> Self {
            Self {
                http_result: Mutex::new(None),
                bash_result: Mutex::new(Some(result)),
            }
        }
    }

    #[async_trait]
    impl Fetcher for StubFetcher {
        async fn http_get(&self, _url: &str) -> Result<String, String> {
            self.http_result
                .lock()
                .unwrap()
                .clone()
                .expect("StubFetcher: http_get called but not stubbed")
        }

        async fn run_bash(&self, _cmd: &str) -> Result<(i32, String), String> {
            self.bash_result
                .lock()
                .unwrap()
                .clone()
                .expect("StubFetcher: run_bash called but not stubbed")
        }
    }

    fn http_spec(url: &str, extract: Option<&str>) -> GateSpec {
        let mut json = serde_json::json!({ "url": url });
        if let Some(e) = extract {
            json["extract"] = Value::String(e.to_string());
        }
        GateSpec {
            kind: GateKind::Http,
            spec_json: json,
        }
    }

    fn bash_spec(command: &str) -> GateSpec {
        GateSpec {
            kind: GateKind::Bash,
            spec_json: serde_json::json!({ "command": command }),
        }
    }

    fn event_spec(path: &str, equals: Value) -> GateSpec {
        GateSpec {
            kind: GateKind::Event,
            spec_json: serde_json::json!({ "path": path, "equals": equals }),
        }
    }

    // -- none --------------------------------------------------------

    #[tokio::test]
    async fn none_kind_always_runs() {
        let spec = GateSpec {
            kind: GateKind::None,
            spec_json: Value::Null,
        };
        let fetcher = StubFetcher::http(Ok(String::new()));
        let decision = evaluate_gate(&spec, None, None, &fetcher).await;
        assert_eq!(
            decision,
            GateDecision {
                run: true,
                gate_output: None,
                new_last_value: None,
                error: None
            }
        );
    }

    // -- http --------------------------------------------------------

    #[tokio::test]
    async fn http_jsonpath_value_differs_runs_and_persists() {
        let spec = http_spec("https://x", Some("$.a.b"));
        let fetcher = StubFetcher::http(Ok(r#"{"a":{"b":"5"}}"#.to_string()));
        let decision = evaluate_gate(&spec, Some("4"), None, &fetcher).await;
        assert_eq!(
            decision,
            GateDecision {
                run: true,
                gate_output: Some("5".to_string()),
                new_last_value: Some("5".to_string()),
                error: None,
            }
        );
    }

    #[tokio::test]
    async fn http_jsonpath_value_equal_skips() {
        let spec = http_spec("https://x", Some("$.a.b"));
        let fetcher = StubFetcher::http(Ok(r#"{"a":{"b":"5"}}"#.to_string()));
        let decision = evaluate_gate(&spec, Some("5"), None, &fetcher).await;
        assert_eq!(decision, GateDecision::skip());
    }

    #[tokio::test]
    async fn http_regex_extract_first_capture_group() {
        let spec = http_spec("https://x", Some(r"/version: (\d+\.\d+)/"));
        let fetcher = StubFetcher::http(Ok("build ok, version: 1.2, done".to_string()));
        let decision = evaluate_gate(&spec, None, None, &fetcher).await;
        assert_eq!(decision.gate_output.as_deref(), Some("1.2"));
        assert!(decision.run);
        assert!(decision.error.is_none());
    }

    #[tokio::test]
    async fn http_empty_extract_skips() {
        let spec = http_spec("https://x", Some("$.missing"));
        let fetcher = StubFetcher::http(Ok(r#"{"a":1}"#.to_string()));
        let decision = evaluate_gate(&spec, None, None, &fetcher).await;
        assert_eq!(decision, GateDecision::skip());
    }

    #[tokio::test]
    async fn http_no_extract_uses_whole_trimmed_body() {
        let spec = http_spec("https://x", None);
        let fetcher = StubFetcher::http(Ok("  hello  ".to_string()));
        let decision = evaluate_gate(&spec, Some("other"), None, &fetcher).await;
        assert_eq!(decision.gate_output.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn http_fetch_error_fails_open() {
        let spec = http_spec("https://x", Some("$.a"));
        let fetcher = StubFetcher::http(Err("connection refused".to_string()));
        let decision = evaluate_gate(&spec, None, None, &fetcher).await;
        assert!(decision.run);
        assert!(decision.error.is_some());
        assert_eq!(decision.gate_output, None);
        assert_eq!(decision.new_last_value, None);
    }

    #[tokio::test]
    async fn http_missing_url_fails_open() {
        let spec = GateSpec {
            kind: GateKind::Http,
            spec_json: serde_json::json!({}),
        };
        let fetcher = StubFetcher::http(Ok(String::new()));
        let decision = evaluate_gate(&spec, None, None, &fetcher).await;
        assert!(decision.run);
        assert!(decision.error.is_some());
    }

    // -- bash --------------------------------------------------------

    #[tokio::test]
    async fn bash_value_differs_runs_and_persists() {
        let spec = bash_spec("echo 5");
        let fetcher = StubFetcher::bash(Ok((0, "5".to_string())));
        let decision = evaluate_gate(&spec, Some("4"), None, &fetcher).await;
        assert_eq!(
            decision,
            GateDecision {
                run: true,
                gate_output: Some("5".to_string()),
                new_last_value: Some("5".to_string()),
                error: None,
            }
        );
    }

    #[tokio::test]
    async fn bash_value_equal_skips() {
        let spec = bash_spec("echo 5");
        let fetcher = StubFetcher::bash(Ok((0, "5".to_string())));
        let decision = evaluate_gate(&spec, Some("5"), None, &fetcher).await;
        assert_eq!(decision, GateDecision::skip());
    }

    #[tokio::test]
    async fn bash_nonzero_exit_fails_open() {
        let spec = bash_spec("exit 1");
        let fetcher = StubFetcher::bash(Ok((1, String::new())));
        let decision = evaluate_gate(&spec, None, None, &fetcher).await;
        assert!(decision.run);
        assert!(decision.error.is_some());
        assert_eq!(decision.gate_output, None);
        assert_eq!(decision.new_last_value, None);
    }

    #[tokio::test]
    async fn bash_exec_error_fails_open() {
        let spec = bash_spec("whatever");
        let fetcher = StubFetcher::bash(Err("spawn failed".to_string()));
        let decision = evaluate_gate(&spec, None, None, &fetcher).await;
        assert!(decision.run);
        assert!(decision.error.is_some());
    }

    #[tokio::test]
    async fn bash_empty_stdout_skips() {
        let spec = bash_spec("true");
        let fetcher = StubFetcher::bash(Ok((0, String::new())));
        let decision = evaluate_gate(&spec, None, None, &fetcher).await;
        assert_eq!(decision, GateDecision::skip());
    }

    // -- event ---------------------------------------------------------

    #[tokio::test]
    async fn event_match_runs_with_payload_json() {
        let spec = event_spec("type", Value::String("x".to_string()));
        let payload = serde_json::json!({ "type": "x" });
        let fetcher = StubFetcher::http(Ok(String::new()));
        let decision = evaluate_gate(&spec, None, Some(&payload), &fetcher).await;
        assert!(decision.run);
        assert!(decision.error.is_none());
        assert_eq!(decision.new_last_value, None);
        let out = decision.gate_output.expect("gate_output set on match");
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, payload);
    }

    #[tokio::test]
    async fn event_no_match_skips() {
        let spec = event_spec("type", Value::String("x".to_string()));
        let payload = serde_json::json!({ "type": "y" });
        let fetcher = StubFetcher::http(Ok(String::new()));
        let decision = evaluate_gate(&spec, None, Some(&payload), &fetcher).await;
        assert_eq!(decision, GateDecision::skip());
    }

    #[tokio::test]
    async fn event_missing_key_skips_not_fail_open() {
        let spec = event_spec("missing.path", Value::String("x".to_string()));
        let payload = serde_json::json!({ "type": "x" });
        let fetcher = StubFetcher::http(Ok(String::new()));
        let decision = evaluate_gate(&spec, None, Some(&payload), &fetcher).await;
        assert_eq!(decision, GateDecision::skip());
    }

    #[tokio::test]
    async fn event_missing_payload_fails_open() {
        let spec = event_spec("type", Value::String("x".to_string()));
        let fetcher = StubFetcher::http(Ok(String::new()));
        let decision = evaluate_gate(&spec, None, None, &fetcher).await;
        assert!(decision.run);
        assert!(decision.error.is_some());
    }

    #[tokio::test]
    async fn event_missing_spec_fields_fail_open() {
        let spec = GateSpec {
            kind: GateKind::Event,
            spec_json: serde_json::json!({}),
        };
        let payload = serde_json::json!({ "type": "x" });
        let fetcher = StubFetcher::http(Ok(String::new()));
        let decision = evaluate_gate(&spec, None, Some(&payload), &fetcher).await;
        assert!(decision.run);
        assert!(decision.error.is_some());
    }

    // -- truncation ------------------------------------------------------

    #[tokio::test]
    async fn truncation_is_char_safe_on_multibyte_body() {
        // "测" is a 3-byte UTF-8 char; 20_000 of them exceed the 16KB char
        // cap in char count while being far larger in byte count. A naive
        // byte-slice truncate([..16*1024]) would land mid-codepoint and
        // panic; `truncate_chars` must not.
        let body: String = std::iter::repeat('测').take(20_000).collect();
        let spec = http_spec("https://x", None);
        let fetcher = StubFetcher::http(Ok(body));
        let decision = evaluate_gate(&spec, None, None, &fetcher).await;

        let output = decision.gate_output.expect("non-empty body runs");
        assert_eq!(output.chars().count(), MAX_GATE_VALUE_CHARS);
        assert_eq!(decision.new_last_value.as_deref(), Some(output.as_str()));
        assert!(decision.run);
    }

    #[tokio::test]
    async fn truncation_leaves_short_values_untouched() {
        let spec = http_spec("https://x", None);
        let fetcher = StubFetcher::http(Ok("short".to_string()));
        let decision = evaluate_gate(&spec, None, None, &fetcher).await;
        assert_eq!(decision.gate_output.as_deref(), Some("short"));
    }
}
