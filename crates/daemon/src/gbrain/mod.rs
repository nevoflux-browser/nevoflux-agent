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
//! The module intentionally does NOT depend on `nevoflux-brain`. The
//! `BrainEngine` trait implementation that wraps this supervisor lives
//! in `crates/brain/` (M3-2).

pub mod config;
pub mod mcp_client;
pub mod supervisor;

pub use config::GbrainConfig;
pub use mcp_client::{McpClient, McpError, McpResult};
pub use supervisor::{GbrainSupervisor, SupervisorError, SupervisorResult, SupervisorState};
