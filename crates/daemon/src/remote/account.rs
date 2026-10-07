//! A1 Device Authorization Grant (RFC 8628) client + account-token state for
//! remote-gateway login (design §4b). Before `/remote-control` the daemon logs
//! in to nevoflux.app; the resulting account token gates login, mints the
//! Durable-Object admission JWT (C2), and claims the device (C3).
//!
//! The HTTP boundary is kept thin: the OAuth/response *parsing* lives in pure
//! functions (`parse_*`) that are unit-tested offline; the `async` reqwest
//! wrappers just call them, so they need a live nevoflux.app only for
//! integration testing (which lands with the www deploy).

use serde_json::{json, Value};

use crate::error::{DaemonError, Result};

const DEVICE_CODE_PATH: &str = "/api/auth/device/code";
const DEVICE_TOKEN_PATH: &str = "/api/auth/device/token";
const JWT_TOKEN_PATH: &str = "/api/auth/token";
const CLAIM_PATH: &str = "/api/devices/claim";

/// Response to `POST /api/auth/device/code` (RFC 8628 §3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCodeResp {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    /// `verification_uri` with `?user_code=` already appended, when the auth
    /// server supplies it (RFC 8628 §3.2 `verification_uri_complete`). The
    /// device page reads that query parameter and prefills its input, so this
    /// is the URL worth opening for the user.
    pub verification_uri_complete: Option<String>,
    pub interval_secs: u64,
    pub expires_in_secs: u64,
}

/// Outcome of one `POST /api/auth/device/token` poll (RFC 8628 §3.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// `authorization_pending` — keep polling at the current interval.
    Pending,
    /// `slow_down` — increase the polling interval before the next poll.
    SlowDown,
    /// The user approved; the account token is issued.
    Token(String),
    /// Terminal error (`expired_token`, `access_denied`, …).
    Denied(String),
}

/// Parse a device-code response body. Accepts `verification_uri` or the
/// `_complete` form; defaults `interval`/`expires_in` to RFC-typical values.
pub fn parse_device_code_resp(body: &Value) -> Result<DeviceCodeResp> {
    let s = |k: &str| body.get(k).and_then(|v| v.as_str());
    let miss = |f: &str| DaemonError::InvalidRequest(format!("device code response missing {f}"));
    Ok(DeviceCodeResp {
        device_code: s("device_code")
            .ok_or_else(|| miss("device_code"))?
            .to_string(),
        user_code: s("user_code").ok_or_else(|| miss("user_code"))?.to_string(),
        verification_uri: s("verification_uri")
            .or_else(|| s("verification_uri_complete"))
            .ok_or_else(|| miss("verification_uri"))?
            .to_string(),
        verification_uri_complete: s("verification_uri_complete").map(String::from),
        interval_secs: body.get("interval").and_then(Value::as_u64).unwrap_or(5),
        expires_in_secs: body
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(1800),
    })
}

/// Map a token-poll response body to a [`PollOutcome`]. A token (`access_token`
/// or `token`) wins; otherwise the `error` code (string, or nested `{error}`)
/// selects pending / slow_down / terminal.
pub fn parse_token_poll(body: &Value) -> PollOutcome {
    if let Some(tok) = body
        .get("access_token")
        .and_then(Value::as_str)
        .or_else(|| body.get("token").and_then(Value::as_str))
    {
        return PollOutcome::Token(tok.to_string());
    }
    let err = body.get("error").and_then(Value::as_str).or_else(|| {
        body.get("error")
            .and_then(|e| e.get("error"))
            .and_then(Value::as_str)
    });
    match err {
        Some("authorization_pending") => PollOutcome::Pending,
        Some("slow_down") => PollOutcome::SlowDown,
        Some(other) => PollOutcome::Denied(other.to_string()),
        None => PollOutcome::Denied("unrecognized token response".into()),
    }
}

/// Extract the DO-admission JWT: better-auth returns it in the `set-auth-jwt`
/// response header, falling back to a `{ token }` body.
pub fn parse_jwt_resp(header_jwt: Option<&str>, body: &Value) -> Result<String> {
    if let Some(j) = header_jwt {
        if !j.is_empty() {
            return Ok(j.to_string());
        }
    }
    body.get("token")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| {
            DaemonError::InvalidRequest("no JWT in set-auth-jwt header or token body".into())
        })
}

/// Where the daemon's nevoflux account token lives. Behind a trait so the login
/// flow is testable without disk; production uses [`FileTokenStore`]. (Headless
/// bypasses this: its token is the `NEVOFLUX_SERVICE_TOKEN` env, §4b.3.)
pub trait TokenStore: Send + Sync {
    fn save(&self, token: &str) -> Result<()>;
    fn load(&self) -> Result<Option<String>>;
    fn clear(&self) -> Result<()>;
}

/// Plain-file token store. NOTE: a follow-up should move this behind the OS
/// keyring / an encrypted-at-rest store; the token is account-bearer material.
pub struct FileTokenStore {
    path: std::path::PathBuf,
}

impl FileTokenStore {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl TokenStore for FileTokenStore {
    fn save(&self, token: &str) -> Result<()> {
        std::fs::write(&self.path, token)
            .map_err(|e| DaemonError::InternalError(format!("token save: {e}")))
    }
    fn load(&self) -> Result<Option<String>> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(DaemonError::InternalError(format!("token load: {e}"))),
        }
    }
    fn clear(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(DaemonError::InternalError(format!("token clear: {e}"))),
        }
    }
}

/// True when a non-empty account token is stored. Validity/expiry is checked
/// lazily by the first authenticated call (a stale token surfaces as a 401).
/// This is what `/remote-control` gates on (design §4b).
pub fn is_logged_in(store: &dyn TokenStore) -> bool {
    matches!(store.load(), Ok(Some(t)) if !t.trim().is_empty())
}

// --- thin async HTTP wrappers (integration-tested against a live nevoflux.app) ---

/// `POST /api/auth/device/code` — start the device grant.
/// A network error with its cause, not just its headline.
///
/// `reqwest` reports the outermost layer — "error sending request for url" —
/// and keeps what actually happened in the source chain. Printed with `{e}`
/// that is thrown away, leaving a message that says a request failed and
/// nothing about whether the name did not resolve, the handshake was refused,
/// or the connection timed out. Those need different answers from whoever is
/// reading the log, which is the whole reason to print an error at all.
fn because(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        let text = c.to_string();
        // Layers routinely repeat the layer below verbatim; say it once.
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        cause = c.source();
    }
    out
}

pub async fn request_device_code(base_url: &str, client_id: &str) -> Result<DeviceCodeResp> {
    let resp = reqwest::Client::new()
        .post(format!("{base_url}{DEVICE_CODE_PATH}"))
        .json(&json!({ "client_id": client_id, "scope": "openid profile email" }))
        .send()
        .await
        .map_err(|e| DaemonError::InternalError(format!("device code request: {}", because(&e))))?;
    let body: Value = resp
        .json()
        .await
        .map_err(|e| DaemonError::InternalError(format!("device code decode: {e}")))?;
    parse_device_code_resp(&body)
}

/// `POST /api/auth/device/token` — poll once for the account token.
pub async fn poll_device_token(
    base_url: &str,
    client_id: &str,
    device_code: &str,
) -> Result<PollOutcome> {
    let resp = reqwest::Client::new()
        .post(format!("{base_url}{DEVICE_TOKEN_PATH}"))
        .json(&json!({
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            "device_code": device_code,
            "client_id": client_id,
        }))
        .send()
        .await
        .map_err(|e| DaemonError::InternalError(format!("device token poll: {}", because(&e))))?;
    let body: Value = resp.json().await.unwrap_or_else(|_| json!({}));
    Ok(parse_token_poll(&body))
}

/// Why a relay JWT could not be minted.
///
/// Split in two because the two need opposite answers. An expired session will
/// never mint again, however often it is retried — the person has to sign in.
/// Anything else (no network, a 5xx, a malformed reply) may well clear up on
/// its own, and the token that produced it is still the right one to keep.
#[derive(Debug, thiserror::Error)]
pub enum MintError {
    /// HTTP 401: the account session behind the token is over (better-auth
    /// sessions expire after about a week).
    #[error("the nevoflux.app session has expired")]
    SessionExpired,
    /// Everything else, with what actually happened.
    #[error("{0}")]
    Other(String),
}

/// `GET /api/auth/token` with the account token — mint the DO-admission JWT.
pub async fn mint_do_jwt(
    base_url: &str,
    account_token: &str,
) -> std::result::Result<String, MintError> {
    let resp = reqwest::Client::new()
        .get(format!("{base_url}{JWT_TOKEN_PATH}"))
        .bearer_auth(account_token)
        .send()
        .await
        .map_err(|e| MintError::Other(format!("mint jwt request: {}", because(&e))))?;
    let status = resp.status();
    // Checked before the body: a 401 carries no JWT, and reporting it as the
    // parse failure that follows ("no JWT in set-auth-jwt header…") hid the one
    // fact that mattered — the person has to sign in again.
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(MintError::SessionExpired);
    }
    if !status.is_success() {
        return Err(MintError::Other(format!("mint jwt failed: HTTP {status}")));
    }
    let header_jwt = resp
        .headers()
        .get("set-auth-jwt")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let body: Value = resp.json().await.unwrap_or_else(|_| json!({}));
    parse_jwt_resp(header_jwt.as_deref(), &body).map_err(|e| MintError::Other(e.to_string()))
}

/// Move an expired account token out of the way, keeping it.
///
/// Renamed to `<name>.expired-YYYYMMDD` (then `-2`, `-3`, … for a second
/// retirement the same day) rather than deleted: it is the only evidence of
/// which session expired, and a token retired by mistake can be put back by
/// hand. Returns the new path, or `None` when there was no token to retire.
pub fn retire_expired_token(
    path: &std::path::Path,
    today: chrono::NaiveDate,
) -> std::io::Result<Option<std::path::PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "account-token".into());
    let stem = format!("{name}.expired-{}", today.format("%Y%m%d"));
    let mut target = path.with_file_name(&stem);
    let mut n = 2u32;
    while target.exists() {
        target = path.with_file_name(format!("{stem}-{n}"));
        n += 1;
    }
    match std::fs::rename(path, &target) {
        Ok(()) => Ok(Some(target)),
        // Gone between the check and the rename: somebody else retired it.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// `POST /api/devices/claim` with the account token — claim `device_id`.
pub async fn claim_device(
    base_url: &str,
    account_token: &str,
    device_id: &str,
    name: Option<&str>,
) -> Result<()> {
    let resp = reqwest::Client::new()
        .post(format!("{base_url}{CLAIM_PATH}"))
        .bearer_auth(account_token)
        .json(&json!({ "device_id": device_id, "name": name }))
        .send()
        .await
        .map_err(|e| {
            DaemonError::InternalError(format!("claim device request: {}", because(&e)))
        })?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(DaemonError::InvalidRequest(format!(
            "device claim failed: HTTP {}",
            resp.status()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_code_resp() {
        let b = json!({
            "device_code": "DC", "user_code": "U-1234",
            "verification_uri": "https://nevoflux.app/device",
            "interval": 5, "expires_in": 1800
        });
        let r = parse_device_code_resp(&b).unwrap();
        assert_eq!(r.device_code, "DC");
        assert_eq!(r.user_code, "U-1234");
        assert_eq!(r.verification_uri, "https://nevoflux.app/device");
        assert_eq!(r.interval_secs, 5);
        assert_eq!(r.expires_in_secs, 1800);
    }

    #[test]
    fn device_code_resp_defaults_and_complete_uri() {
        let b = json!({ "device_code": "DC", "user_code": "U", "verification_uri_complete": "/device?x" });
        let r = parse_device_code_resp(&b).unwrap();
        assert_eq!(r.verification_uri, "/device?x");
        assert_eq!(r.interval_secs, 5, "interval defaults to 5");
        assert_eq!(r.expires_in_secs, 1800, "expires_in defaults to 1800");
    }

    #[test]
    fn device_code_resp_missing_required_errors() {
        assert!(parse_device_code_resp(&json!({ "user_code": "U" })).is_err());
    }

    #[test]
    fn device_code_resp_carries_complete_uri() {
        let b = json!({
            "device_code": "DC", "user_code": "U-1234",
            "verification_uri": "https://nevoflux.app/device",
            "verification_uri_complete": "https://nevoflux.app/device?user_code=U-1234"
        });
        let r = parse_device_code_resp(&b).unwrap();
        assert_eq!(r.verification_uri, "https://nevoflux.app/device");
        assert_eq!(
            r.verification_uri_complete.as_deref(),
            Some("https://nevoflux.app/device?user_code=U-1234"),
            "the prefilled URL must survive parsing — it is what the sidebar opens"
        );
    }

    #[test]
    fn device_code_resp_complete_uri_absent_is_none() {
        let b = json!({
            "device_code": "DC", "user_code": "U",
            "verification_uri": "https://nevoflux.app/device"
        });
        assert_eq!(
            parse_device_code_resp(&b)
                .unwrap()
                .verification_uri_complete,
            None
        );
    }

    #[test]
    fn token_poll_maps_all_outcomes() {
        assert_eq!(
            parse_token_poll(&json!({ "error": "authorization_pending" })),
            PollOutcome::Pending
        );
        assert_eq!(
            parse_token_poll(&json!({ "error": "slow_down" })),
            PollOutcome::SlowDown
        );
        assert_eq!(
            parse_token_poll(&json!({ "access_token": "AT" })),
            PollOutcome::Token("AT".into())
        );
        assert_eq!(
            parse_token_poll(&json!({ "token": "TK" })),
            PollOutcome::Token("TK".into())
        );
        assert_eq!(
            parse_token_poll(&json!({ "error": "access_denied" })),
            PollOutcome::Denied("access_denied".into())
        );
    }

    #[test]
    fn token_poll_handles_nested_error() {
        assert_eq!(
            parse_token_poll(&json!({ "error": { "error": "slow_down" } })),
            PollOutcome::SlowDown
        );
    }

    #[test]
    fn jwt_from_header_then_body() {
        assert_eq!(parse_jwt_resp(Some("HJWT"), &json!({})).unwrap(), "HJWT");
        assert_eq!(
            parse_jwt_resp(None, &json!({ "token": "BJWT" })).unwrap(),
            "BJWT"
        );
        assert!(
            parse_jwt_resp(Some(""), &json!({})).is_err(),
            "empty header + no body token errors"
        );
    }

    #[test]
    fn token_store_roundtrip_and_is_logged_in() {
        let dir = std::env::temp_dir().join(format!("nf-token-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = FileTokenStore::new(dir.join("account-token"));

        assert!(!is_logged_in(&store));
        assert_eq!(store.load().unwrap(), None);

        store.save("TOK").unwrap();
        assert!(is_logged_in(&store));
        assert_eq!(store.load().unwrap().as_deref(), Some("TOK"));

        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None);
        assert!(!is_logged_in(&store));
    }

    fn day(y: i32, m: u32, d: u32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn an_expired_token_is_renamed_with_the_date_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account-token");
        std::fs::write(&path, "OLD").unwrap();

        let moved = retire_expired_token(&path, day(2026, 10, 7)).unwrap();
        let expected = dir.path().join("account-token.expired-20261007");
        assert_eq!(moved.as_deref(), Some(expected.as_path()));
        assert!(!path.exists(), "the live token is gone");
        assert_eq!(
            std::fs::read_to_string(&expected).unwrap(),
            "OLD",
            "retired, never deleted"
        );
    }

    #[test]
    fn a_second_retirement_the_same_day_gets_a_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account-token");
        std::fs::write(&path, "FIRST").unwrap();
        retire_expired_token(&path, day(2026, 10, 7)).unwrap();
        std::fs::write(&path, "SECOND").unwrap();
        let moved = retire_expired_token(&path, day(2026, 10, 7)).unwrap();
        assert_eq!(
            moved.as_deref(),
            Some(
                dir.path()
                    .join("account-token.expired-20261007-2")
                    .as_path()
            )
        );
        std::fs::write(&path, "THIRD").unwrap();
        let moved = retire_expired_token(&path, day(2026, 10, 7)).unwrap();
        assert_eq!(
            moved.as_deref(),
            Some(
                dir.path()
                    .join("account-token.expired-20261007-3")
                    .as_path()
            )
        );
        // Every earlier one is still there, untouched.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("account-token.expired-20261007")).unwrap(),
            "FIRST"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("account-token.expired-20261007-2")).unwrap(),
            "SECOND"
        );
    }

    #[test]
    fn retiring_a_missing_token_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account-token");
        assert_eq!(retire_expired_token(&path, day(2026, 10, 7)).unwrap(), None);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_401_is_an_expired_session() {
        let srv = test_server::AccountServer::start(
            401,
            r#"{"message":"Unauthorized","code":"UNAUTHORIZED"}"#,
        )
        .await;
        match mint_do_jwt(&srv.base, "tok").await {
            Err(MintError::SessionExpired) => {}
            other => panic!("expected SessionExpired, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_200_with_a_token_mints() {
        let srv = test_server::AccountServer::start(200, r#"{"token":"j"}"#).await;
        assert_eq!(mint_do_jwt(&srv.base, "tok").await.unwrap(), "j");
        assert_eq!(srv.hits(), 1);
    }

    #[tokio::test]
    async fn a_500_is_not_an_expired_session() {
        let srv = test_server::AccountServer::start(500, "{}").await;
        match mint_do_jwt(&srv.base, "tok").await {
            Err(MintError::Other(_)) => {}
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_200_without_a_jwt_is_not_an_expired_session() {
        let srv = test_server::AccountServer::start(200, "{}").await;
        match mint_do_jwt(&srv.base, "tok").await {
            Err(MintError::Other(m)) => assert!(m.contains("no JWT"), "{m}"),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unreachable_account_service_is_not_an_expired_session() {
        match mint_do_jwt("http://127.0.0.1:1", "tok").await {
            Err(MintError::Other(_)) => {}
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn a_network_error_says_what_actually_happened() {
        // `reqwest` reports "error sending request for url" and keeps the real
        // reason underneath. Printed with `{e}` that is lost, and a log that
        // cannot tell a blocked handshake from an unresolved name sends the
        // reader looking in the wrong place -- which it did.
        #[derive(Debug)]
        struct Layer(&'static str, Option<Box<Layer>>);
        impl std::fmt::Display for Layer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::error::Error for Layer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.1
                    .as_deref()
                    .map(|l| l as &(dyn std::error::Error + 'static))
            }
        }

        let e = Layer(
            "error sending request",
            Some(Box::new(Layer(
                "tcp connect error",
                Some(Box::new(Layer("connection timed out", None))),
            ))),
        );
        assert_eq!(
            because(&e),
            "error sending request: tcp connect error: connection timed out"
        );

        // A layer that merely repeats the one below adds nothing.
        let doubled = Layer("dns error", Some(Box::new(Layer("dns error", None))));
        assert_eq!(because(&doubled), "dns error");
    }
}

/// A stand-in for nevoflux.app's `GET /api/auth/token`, shared by the remote
/// module's tests: answers every mint with one fixed status and body, and
/// counts how often it was asked.
#[cfg(test)]
pub(crate) mod test_server {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    pub(crate) struct AccountServer {
        pub base: String,
        hits: Arc<AtomicUsize>,
    }

    impl AccountServer {
        pub async fn start(status: u16, body: &'static str) -> Self {
            let hits = Arc::new(AtomicUsize::new(0));
            let counter = hits.clone();
            let app = axum::Router::new().route(
                super::JWT_TOKEN_PATH,
                axum::routing::get(move || {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        (
                            axum::http::StatusCode::from_u16(status).unwrap(),
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            body,
                        )
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Self {
                base: format!("http://{addr}"),
                hits,
            }
        }

        pub fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }
}
