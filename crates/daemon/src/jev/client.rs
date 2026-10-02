//! The Jev HTTP client: one long-lived reqwest client (connections are reused
//! — a new HTTPS connection costs ~270 ms, S4), a timeout per request, and the
//! LocalOnly latch checked before anything leaves the machine.

use std::collections::BTreeMap;
use std::time::Duration;

use super::wire::{JevRequest, JevResponse, Question};

#[derive(Debug, thiserror::Error)]
pub enum JevError {
    #[error("Jev is not configured (disabled, or no endpoint/key)")]
    NotConfigured,
    #[error("Jev request refused: {0}")]
    Refused(String),
    #[error("Jev did not answer in time")]
    Timeout,
    #[error("Jev returned HTTP {status}")]
    Http { status: u16 },
    #[error("Jev transport error: {0}")]
    Transport(String),
    #[error("Jev answer could not be read: {0}")]
    Decode(String),
}

/// Whether a request to `endpoint` may leave while the LocalOnly latch is
/// `latched` (spec §3.2): only loopback endpoints then.
pub fn egress_allowed(latched: bool, endpoint: &str) -> bool {
    !latched || crate::local::latch::is_loopback_url(endpoint)
}

/// Redirects are never followed (the latch approved the configured endpoint,
/// not wherever it points), and a loopback endpoint never goes through a
/// system proxy — that would carry the state and the key off the machine
/// with the LocalOnly latch on (same rule as `wasm/local_llm.rs`, R30/R33).
/// Remote endpoints keep the system proxy: some users need it to reach them.
fn http_policy(b: reqwest::ClientBuilder, endpoint: &str) -> reqwest::ClientBuilder {
    let b = b.redirect(reqwest::redirect::Policy::none());
    if crate::local::latch::is_loopback_url(endpoint) {
        b.no_proxy()
    } else {
        b
    }
}

/// A request left running after its caller gave up is still cut off here.
const ORPHAN_CAP: Duration = Duration::from_secs(30);

static SHARED: std::sync::Mutex<Option<((String, String, String), JevClient)>> =
    std::sync::Mutex::new(None);

/// The daemon's one Jev client for this configuration: connections are kept
/// across turns (a new HTTPS connection costs ~270 ms; a cold one measured
/// 821 ms against the 800 ms default timeout). A changed endpoint, key or
/// model replaces it.
pub fn shared(cfg: &crate::config::JevConfig) -> Result<JevClient, JevError> {
    if !cfg.is_usable() {
        return Err(JevError::NotConfigured);
    }
    let key = cfg.resolved_api_key().ok_or(JevError::NotConfigured)?;
    let id = (cfg.endpoint.clone(), key.clone(), cfg.model.clone());
    let mut slot = SHARED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((k, c)) = slot.as_ref() {
        if *k == id {
            return Ok(c.clone());
        }
    }
    let c = JevClient::new(&cfg.endpoint, &key, &cfg.model);
    *slot = Some((id, c.clone()));
    Ok(c)
}

#[derive(Clone)]
pub struct JevClient {
    http: reqwest::Client,
    endpoint: String,
    key: String,
    model: String,
}

impl std::fmt::Debug for JevClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JevClient")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl JevClient {
    pub fn new(endpoint: &str, api_key: &str, model: &str) -> Self {
        let builder = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .user_agent(concat!("nevoflux-agent/", env!("CARGO_PKG_VERSION")))
            .timeout(ORPHAN_CAP);
        let http = http_policy(builder, endpoint)
            .build()
            .expect("reqwest client");
        Self {
            http,
            endpoint: endpoint.to_string(),
            key: api_key.to_string(),
            model: model.to_string(),
        }
    }

    pub fn from_config(cfg: &crate::config::JevConfig) -> Result<Self, JevError> {
        if !cfg.is_usable() {
            return Err(JevError::NotConfigured);
        }
        let key = cfg.resolved_api_key().ok_or(JevError::NotConfigured)?;
        Ok(Self::new(&cfg.endpoint, &key, &cfg.model))
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Why nothing may be sent to the endpoint right now, if anything: the
    /// LocalOnly latch, or a remote endpoint without TLS (the key would
    /// travel in clear text).
    fn egress_refusal(&self) -> Option<JevError> {
        if !egress_allowed(crate::local::latch::is_on(), &self.endpoint) {
            return Some(JevError::Refused("on-device mode is on".into()));
        }
        if self
            .endpoint
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("http://")
            && !crate::local::latch::is_loopback_url(&self.endpoint)
        {
            return Some(JevError::Refused(
                "a remote Jev endpoint must use https".into(),
            ));
        }
        None
    }

    /// Open (or keep) the connection before the first real question: an
    /// unauthenticated HEAD, result ignored. Same latch, TLS and proxy rules
    /// as [`Self::ask`].
    pub async fn warm(&self) {
        if self.egress_refusal().is_some() {
            return;
        }
        let _ = self
            .http
            .head(&self.endpoint)
            .timeout(Duration::from_secs(5))
            .send()
            .await;
    }

    pub async fn ask(
        &self,
        state: serde_json::Value,
        questions: BTreeMap<String, Question>,
        timeout: Duration,
    ) -> Result<JevResponse, JevError> {
        if let Some(refused) = self.egress_refusal() {
            return Err(refused);
        }
        let req = JevRequest {
            state,
            model: self.model.clone(),
            questions,
        };
        let pending = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.key)
            .json(&req);
        // The request runs on its own task: when the caller's deadline passes
        // it keeps going, so a cold connection still finishes its handshake
        // and reaches the pool for the next decision point (a dropped request
        // would abort it, and every cold start would time out again).
        let task = tokio::spawn(async move {
            let resp = pending.send().await.map_err(|e| {
                if e.is_timeout() {
                    JevError::Timeout
                } else {
                    JevError::Transport(without_url(e))
                }
            })?;
            let status = resp.status();
            if !status.is_success() {
                return Err(JevError::Http {
                    status: status.as_u16(),
                });
            }
            resp.json::<JevResponse>().await.map_err(|e| {
                if e.is_timeout() {
                    JevError::Timeout
                } else {
                    JevError::Decode(without_url(e))
                }
            })
        });
        match tokio::time::timeout(timeout, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(join)) => Err(JevError::Transport(join.to_string())),
            Err(_) => Err(JevError::Timeout),
        }
    }
}

/// reqwest errors may print the URL; the key is never in it, but keep the
/// message short and free of request details.
fn without_url(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::test_support::{one_noul, spawn};
    use axum::{routing::post, Router};
    use std::sync::Arc;

    #[tokio::test]
    async fn asks_and_parses_and_sends_the_key_only_in_the_header() {
        let app = Router::new().route(
            "/v1/systemone",
            post(|headers: axum::http::HeaderMap, body: String| async move {
                assert_eq!(headers["authorization"], "Bearer k-123");
                assert!(!body.contains("k-123"));
                axum::Json(serde_json::json!({"answers": {"x": {"noul": 0.9}}, "usage": {"input_tokens": 5, "output_tokens": 1}}))
            }),
        );
        let url = spawn(app).await;
        let c = JevClient::new(&url, "k-123", "jev-latest");
        let r = c
            .ask(serde_json::json!({}), one_noul(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(r.noul("x"), Some(0.9));
    }

    #[tokio::test]
    async fn a_slow_answer_is_a_timeout() {
        let app = Router::new().route(
            "/v1/systemone",
            post(|| async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                axum::Json(serde_json::json!({}))
            }),
        );
        let url = spawn(app).await;
        let c = JevClient::new(&url, "k", "jev-latest");
        let e = c
            .ask(
                serde_json::json!({}),
                one_noul(),
                Duration::from_millis(100),
            )
            .await
            .unwrap_err();
        assert!(matches!(e, JevError::Timeout), "{e:?}");
    }

    #[tokio::test]
    async fn an_http_error_keeps_its_status_and_never_the_key() {
        let app = Router::new().route(
            "/v1/systemone",
            post(|| async { (axum::http::StatusCode::TOO_MANY_REQUESTS, "slow down") }),
        );
        let url = spawn(app).await;
        let c = JevClient::new(&url, "secret-key", "jev-latest");
        let e = c
            .ask(serde_json::json!({}), one_noul(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(e, JevError::Http { status: 429 }));
        assert!(!e.to_string().contains("secret-key"));
        assert!(!format!("{c:?}").contains("secret-key"));
    }

    #[tokio::test]
    async fn requests_reuse_one_connection() {
        let peers = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let p = peers.clone();
        let app = Router::new().route(
            "/v1/systemone",
            post(
                move |axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<
                    std::net::SocketAddr,
                >| {
                    let p = p.clone();
                    async move {
                        p.lock().unwrap().insert(addr);
                        axum::Json(serde_json::json!({"answers": {}, "usage": {}}))
                    }
                },
            ),
        );
        let url = spawn(app).await;
        let c = JevClient::new(&url, "k", "jev-latest");
        for _ in 0..3 {
            c.ask(serde_json::json!({}), one_noul(), Duration::from_secs(5))
                .await
                .unwrap();
        }
        assert_eq!(peers.lock().unwrap().len(), 1, "three asks, one connection");
    }

    #[tokio::test]
    async fn a_closed_connection_is_replaced() {
        let app = Router::new().route(
            "/v1/systemone",
            post(|| async {
                (
                    [(axum::http::header::CONNECTION, "close")],
                    axum::Json(serde_json::json!({"answers": {}, "usage": {}})),
                )
            }),
        );
        let url = spawn(app).await;
        let c = JevClient::new(&url, "k", "jev-latest");
        c.ask(serde_json::json!({}), one_noul(), Duration::from_secs(5))
            .await
            .unwrap();
        c.ask(serde_json::json!({}), one_noul(), Duration::from_secs(5))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_loopback_endpoint_never_goes_through_a_proxy() {
        // With the latch on, a loopback endpoint is the only one allowed; a
        // system proxy would carry the state and the key off the machine.
        let app = Router::new().route(
            "/v1/systemone",
            post(|| async { axum::Json(serde_json::json!({"answers": {}, "usage": {}})) }),
        );
        let url = spawn(app).await;
        let builder = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").expect("valid proxy url"));
        let http = http_policy(builder, &url).build().unwrap();
        let resp = http.post(&url).json(&serde_json::json!({})).send().await;
        assert!(resp.is_ok(), "the proxy must be bypassed: {:?}", resp.err());
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed() {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        let elsewhere = spawn(Router::new().route(
            "/v1/systemone",
            post(move || {
                h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { axum::Json(serde_json::json!({"answers": {}, "usage": {}})) }
            }),
        ))
        .await;
        let app = Router::new().route(
            "/v1/systemone",
            post(move || {
                let to = elsewhere.clone();
                async move {
                    (
                        axum::http::StatusCode::TEMPORARY_REDIRECT,
                        [(axum::http::header::LOCATION, to)],
                    )
                }
            }),
        );
        let url = spawn(app).await;
        let c = JevClient::new(&url, "k", "jev-latest");
        let e = c
            .ask(serde_json::json!({}), one_noul(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(e, JevError::Http { status: 307 }), "{e:?}");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_timed_out_request_still_warms_the_connection() {
        // A cold request that misses its deadline must still finish, so its
        // connection reaches the pool for the next decision point.
        let peers = Arc::new(std::sync::Mutex::new(Vec::new()));
        let p = peers.clone();
        let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let app = Router::new().route(
            "/v1/systemone",
            post(
                move |axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<
                    std::net::SocketAddr,
                >| {
                    let p = p.clone();
                    let first = first.clone();
                    async move {
                        p.lock().unwrap().push(addr);
                        if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                            tokio::time::sleep(Duration::from_millis(300)).await;
                        }
                        axum::Json(serde_json::json!({"answers": {}, "usage": {}}))
                    }
                },
            ),
        );
        let url = spawn(app).await;
        let c = JevClient::new(&url, "k", "jev-latest");
        let e = c
            .ask(
                serde_json::json!({}),
                one_noul(),
                Duration::from_millis(100),
            )
            .await
            .unwrap_err();
        assert!(matches!(e, JevError::Timeout), "{e:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        c.ask(serde_json::json!({}), one_noul(), Duration::from_secs(5))
            .await
            .unwrap();
        let peers = peers.lock().unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(
            peers[0], peers[1],
            "the second ask reuses the first connection"
        );
    }

    #[tokio::test]
    async fn the_key_never_goes_to_a_remote_endpoint_in_clear_text() {
        let c = JevClient::new("http://jev.example.invalid/v1/systemone", "k", "jev-latest");
        let e = c
            .ask(serde_json::json!({}), one_noul(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(e, JevError::Refused(_)), "{e:?}");
    }

    #[tokio::test]
    async fn shared_clients_share_one_connection_pool() {
        let peers = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let p = peers.clone();
        let app = Router::new().route(
            "/v1/systemone",
            post(
                move |axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<
                    std::net::SocketAddr,
                >| {
                    let p = p.clone();
                    async move {
                        p.lock().unwrap().insert(addr);
                        axum::Json(serde_json::json!({"answers": {}, "usage": {}}))
                    }
                },
            ),
        );
        let url = spawn(app).await;
        let mut cfg = crate::config::JevConfig::default();
        cfg.enabled = true;
        cfg.endpoint = url;
        cfg.api_key = "k".into();
        for _ in 0..2 {
            shared(&cfg)
                .unwrap()
                .ask(serde_json::json!({}), one_noul(), Duration::from_secs(5))
                .await
                .unwrap();
        }
        assert_eq!(peers.lock().unwrap().len(), 1, "two turns, one connection");
    }

    #[tokio::test]
    async fn warming_opens_the_connection_without_the_key() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = seen.clone();
        let app = Router::new().route(
            "/v1/systemone",
            axum::routing::any(
                move |axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<
                    std::net::SocketAddr,
                >,
                      method: axum::http::Method,
                      headers: axum::http::HeaderMap| {
                    let s = s.clone();
                    async move {
                        s.lock().unwrap().push((
                            addr,
                            method.to_string(),
                            headers.contains_key("authorization"),
                        ));
                        axum::Json(serde_json::json!({"answers": {}, "usage": {}}))
                    }
                },
            ),
        );
        let url = spawn(app).await;
        let c = JevClient::new(&url, "k", "jev-latest");
        c.warm().await;
        c.ask(serde_json::json!({}), one_noul(), Duration::from_secs(5))
            .await
            .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].1, "HEAD");
        assert!(!seen[0].2, "warm-up must not send the key");
        assert_eq!(seen[0].0, seen[1].0, "the ask reuses the warmed connection");
    }

    #[tokio::test]
    async fn warming_respects_the_https_rule() {
        // Nothing listens there; warm must return quietly without connecting.
        JevClient::new("http://jev.example.invalid/v1/systemone", "k", "m")
            .warm()
            .await;
    }

    #[test]
    fn the_local_only_latch_blocks_remote_endpoints() {
        assert!(egress_allowed(
            false,
            "https://api.typesafe.ai/v1/systemone"
        ));
        assert!(!egress_allowed(
            true,
            "https://api.typesafe.ai/v1/systemone"
        ));
        assert!(egress_allowed(true, "http://127.0.0.1:9/v1/systemone"));
    }
}
