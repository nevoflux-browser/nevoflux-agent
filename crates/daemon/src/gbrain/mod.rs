//! Cross-platform gbrain subprocess supervisor (M3-1).
//!
//! This module is the production version of the spike supervisor in
//! `spike/supervisor/` (Windows-only at the time of the spike). It
//! spawns `bun run <gbrain cli.ts> serve`, holds the resulting child's
//! stdin open via a dedicated writer task, and restarts the subprocess
//! under a bounded budget when it dies.
//!
//! See:
//! - [`config`] — [`GbrainConfig`] struct + defaults.
//! - [`mcp_client`] — [`McpClient`], the line-delimited JSON-RPC 2.0
//!   stdio client. Reader + writer each run in their own tokio task.
//! - [`supervisor`] — [`GbrainSupervisor`], the lifecycle owner.
//!
//! M3-2 adds [`engine::GbrainEngine`] in this same module — a thin
//! [`nevoflux_brain::BrainEngine`] implementation that dispatches every
//! trait method to gbrain MCP `tools/call` requests via the supervisor.
//! Architecturally the engine lives next to the supervisor (not in
//! `crates/brain/`) because daemon owns gbrain's subprocess lifecycle
//! and the engine is the natural extension of the supervisor; the brain
//! crate stays trait-only.

pub mod config;
pub mod engine;
pub mod mcp_client;
pub mod page_index;
pub mod supervisor;

pub use config::GbrainConfig;
pub use engine::GbrainEngine;
pub use mcp_client::{McpClient, McpError, McpResult};
pub use page_index::{ListQuery, ListSlice, PageIndex, SortOrder};
pub use supervisor::{
    GbrainSupervisor, McpToolCaller, SupervisorError, SupervisorResult, SupervisorState,
};
