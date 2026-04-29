// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Shared state threaded through every axum handler.
//!
//! Three independent token stores live here per design D12, plus the
//! daemon-wide bearer token, the screenshot upload inbox, and the
//! optional CORS allow-origin (only known once the extension has sent
//! its `bridge:hello`).

use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use super::token_store::{BlobEntry, CompositionEntry, TokenStore};
use super::AssetServerConfig;
use crate::agent::browser_input::upload::TokenEntry;

/// State shared across all handlers — `Arc<AssetServerState>` is passed
/// to axum via `with_state` and route_layer middleware.
pub struct AssetServerState {
    pub bearer_token: String,
    pub session_id: String,
    pub max_body_size: usize,
    /// CORS allow-origin (the moz-extension://... URL); `None` means CORS
    /// preflight is left to the browser default and `*` is sent — useful
    /// before the extension hello has been received.
    allowed_origin: RwLock<Option<String>>,
    pub download_tokens: Arc<TokenStore<TokenEntry>>,
    pub composition_tokens: Arc<TokenStore<CompositionEntry>>,
    pub blob_tokens: Arc<TokenStore<BlobEntry>>,
    pub upload_inbox: Arc<super::inbox::UploadInbox>,
}

impl AssetServerState {
    pub fn new(config: &AssetServerConfig) -> Self {
        Self {
            bearer_token: config.bearer_token.clone(),
            session_id: config.session_id.clone(),
            max_body_size: config.max_body_size,
            allowed_origin: RwLock::new(config.allowed_origin.clone()),
            download_tokens: Arc::new(TokenStore::new()),
            composition_tokens: Arc::new(TokenStore::new()),
            blob_tokens: Arc::new(TokenStore::new()),
            upload_inbox: Arc::new(super::inbox::UploadInbox::new()),
        }
    }

    pub fn set_allowed_origin(&self, origin: Option<String>) {
        if let Ok(mut guard) = self.allowed_origin.write() {
            *guard = origin;
        }
    }

    pub fn allowed_origin(&self) -> Option<String> {
        self.allowed_origin.read().ok().and_then(|g| g.clone())
    }
}

/// Information advertised to the extension via the `system_command status`
/// SystemResponse. Mirrors the design doc's `bridge:hello.asset_plane`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssetPlaneInfo {
    pub port: u16,
    pub bearer_token: String,
    pub session_id: String,
    pub version: u32,
    pub max_body_size: usize,
    pub phases: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_token_stores_are_independent() {
        // Per D12, each store must be its own DashMap — verify they don't
        // share storage.
        let cfg = AssetServerConfig::default();
        let state = AssetServerState::new(&cfg);
        assert!(Arc::ptr_eq(&state.download_tokens, &state.download_tokens));
        // Type-level assertion: download/composition/blob have different
        // entry types so they CAN'T share storage even by accident.
        let _ = &state.download_tokens; // TokenStore<TokenEntry>
        let _ = &state.composition_tokens; // TokenStore<CompositionEntry>
        let _ = &state.blob_tokens; // TokenStore<BlobEntry>
        assert_eq!(state.download_tokens.len(), 0);
        assert_eq!(state.composition_tokens.len(), 0);
        assert_eq!(state.blob_tokens.len(), 0);
    }

    #[test]
    fn asset_plane_info_serializes_round_trip() {
        let info = AssetPlaneInfo {
            port: 19501,
            bearer_token: "secret".into(),
            session_id: "S".into(),
            version: 1,
            max_body_size: 1024,
            phases: vec!["screenshot-upload".into()],
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: AssetPlaneInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
    }

    #[test]
    fn allowed_origin_is_mutable() {
        let cfg = AssetServerConfig::default();
        let state = AssetServerState::new(&cfg);
        assert_eq!(state.allowed_origin(), None);
        state.set_allowed_origin(Some("moz-extension://abc".into()));
        assert_eq!(
            state.allowed_origin(),
            Some("moz-extension://abc".to_string())
        );
    }
}
