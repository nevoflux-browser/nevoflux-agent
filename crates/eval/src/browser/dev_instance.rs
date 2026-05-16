//! `DevInstanceBrowser` — connects to an already-running nevoflux instance
//! started by the user (typically via `just dev` in the nevoflux repo).
//!
//! Daily workflow:
//! ```bash
//! # In nevoflux repo:
//! npm run build         # once after JS changes
//! just dev              # starts nevoflux with --remote-debugging-port=5959
//!
//! # In nevoflux-agent repo:
//! just eval-dev online-mind2web 20
//! ```
//!
//! The handle does NOT own the browser lifecycle — user starts and stops it.
//! On shutdown the handle just disconnects.

use super::BrowserHandle;
use crate::{EvalError, EvalResult};
use async_trait::async_trait;
use tracing::{debug, info};

pub struct DevInstanceBrowser {
    endpoint: String,
}

impl DevInstanceBrowser {
    pub async fn connect(endpoint: String) -> EvalResult<Self> {
        info!(endpoint = %endpoint, "connecting to nevoflux dev instance");

        // Probe the remote debugging port to verify the dev instance is up.
        // TODO: replace with real CDP (Chrome DevTools Protocol) handshake or
        // nevoflux-specific IPC ping once the protocol is finalized.
        Self::probe(&endpoint).await?;

        Ok(Self { endpoint })
    }

    async fn probe(endpoint: &str) -> EvalResult<()> {
        // Light-touch TCP probe — full protocol negotiation happens per-task.
        let host_port = endpoint
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .split('/')
            .next()
            .unwrap_or(endpoint);

        match tokio::net::TcpStream::connect(host_port).await {
            Ok(_) => {
                debug!("dev instance probe OK");
                Ok(())
            }
            Err(e) => Err(EvalError::DaemonConnection(format!(
                "could not reach nevoflux dev instance at {}: {}. \
                 Did you run `just dev` in the nevoflux repo?",
                endpoint, e
            ))),
        }
    }
}

#[async_trait]
impl BrowserHandle for DevInstanceBrowser {
    async fn ensure_ready(&self) -> EvalResult<()> {
        Self::probe(&self.endpoint).await
    }

    async fn shutdown(&self) -> EvalResult<()> {
        // User owns the dev instance lifecycle — we just disconnect.
        debug!("disconnecting from dev instance (user retains ownership)");
        Ok(())
    }

    fn version_string(&self) -> String {
        format!("nevoflux-dev ({})", self.endpoint)
    }

    fn is_real_browser(&self) -> bool {
        true
    }
}
