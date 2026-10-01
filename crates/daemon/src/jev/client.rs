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
        let http = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .user_agent(concat!("nevoflux-agent/", env!("CARGO_PKG_VERSION")))
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

    pub async fn ask(
        &self,
        state: serde_json::Value,
        questions: BTreeMap<String, Question>,
        timeout: Duration,
    ) -> Result<JevResponse, JevError> {
        if !egress_allowed(crate::local::latch::is_on(), &self.endpoint) {
            return Err(JevError::Refused("on-device mode is on".into()));
        }
        let req = JevRequest {
            state,
            model: self.model.clone(),
            questions,
        };
        let resp = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.key)
            .json(&req)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| {
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
    use axum::{routing::post, Router};
    use std::future::IntoFuture;
    use std::sync::Arc;

    async fn spawn(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .into_future(),
        );
        format!("http://{addr}/v1/systemone")
    }

    fn one_noul() -> BTreeMap<String, Question> {
        let mut q = BTreeMap::new();
        q.insert(
            "x".into(),
            Question::Noul {
                instructions: "i".into(),
                when_true: "t".into(),
                when_false: "f".into(),
            },
        );
        q
    }

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
