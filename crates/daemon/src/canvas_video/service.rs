//! CanvasVideoService — dependency bag + method surface.

use crate::canvas_video::create;
use crate::error::Result;
use nevoflux_protocol::canvas_video::{CreateCompositionRequest, CreateCompositionResponse};

/// Owns the pieces needed to create and render compositions.
/// Production instances are constructed from AppState in main.rs;
/// tests use `new_for_tests()`.
///
/// Phase B fills this with real deps (artifact repo, ffmpeg path cache,
/// bridge sender); for now the struct is empty so the create-composition
/// validation path can be exercised from tests.
pub struct CanvasVideoService {}

impl CanvasVideoService {
    pub fn new() -> Self {
        Self {}
    }

    /// Alias for `new()` — kept distinct so Phase B can diverge
    /// production vs. test wiring without churning call sites.
    pub fn new_for_tests() -> Self {
        Self::new()
    }

    pub async fn create_composition(
        &self,
        req: CreateCompositionRequest,
    ) -> Result<CreateCompositionResponse> {
        create::create(self, req).await
    }
}

impl Default for CanvasVideoService {
    fn default() -> Self {
        Self::new()
    }
}
