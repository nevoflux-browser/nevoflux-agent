//! `DevInstanceBrowser` — connects to an already-running nevoflux instance.
//!
//! **Status: stub for Phase 2.** Full implementation in Phase 3 (browser
//! adapters for benchmark adapters). This stub returns an error on construction
//! so the workspace compiles cleanly.

use super::BrowserHandle;
use crate::{EvalError, EvalResult};
use async_trait::async_trait;

pub struct DevInstanceBrowser {
    _endpoint: String,
}

impl DevInstanceBrowser {
    pub async fn connect(_endpoint: String) -> EvalResult<Self> {
        Err(EvalError::Other(
            "ExternalDevInstance browser mode not implemented in Phase 2; \
             slated for Phase 3 with benchmark adapters."
                .into(),
        ))
    }
}

#[async_trait]
impl BrowserHandle for DevInstanceBrowser {
    async fn ensure_ready(&self) -> EvalResult<()> {
        unreachable!("constructor errors before instance is created")
    }
    async fn shutdown(&self) -> EvalResult<()> {
        Ok(())
    }
    fn version_string(&self) -> String {
        "external-dev-instance (stub)".into()
    }
    fn is_real_browser(&self) -> bool {
        true
    }
}
