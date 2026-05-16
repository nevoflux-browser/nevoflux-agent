//! `ReleaseBrowser` — downloads + launches a published nevoflux release.
//!
//! **Status: stub for Phase 2.** Full implementation in Phase 4 (CI +
//! release pipeline). This stub returns an error on construction so the
//! workspace compiles cleanly.

use super::BrowserHandle;
use crate::{EvalError, EvalResult};
use async_trait::async_trait;
use std::path::PathBuf;

pub struct ReleaseBrowser {
    _version: String,
}

impl ReleaseBrowser {
    pub async fn launch(_version: String, _cache_dir: PathBuf) -> EvalResult<Self> {
        Err(EvalError::Other(
            "ReleaseBinary browser mode not implemented in Phase 2; \
             slated for Phase 4 with the nevoflux release pipeline."
                .into(),
        ))
    }
}

#[async_trait]
impl BrowserHandle for ReleaseBrowser {
    async fn ensure_ready(&self) -> EvalResult<()> {
        unreachable!("constructor errors before instance is created")
    }
    async fn shutdown(&self) -> EvalResult<()> {
        Ok(())
    }
    fn version_string(&self) -> String {
        "release-binary (stub)".into()
    }
    fn is_real_browser(&self) -> bool {
        true
    }
}
