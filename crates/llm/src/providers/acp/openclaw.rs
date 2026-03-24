//! OpenClaw ACP configuration.
//!
//! Uses the OpenClaw CLI with the `acp` subcommand:
//! `npm install -g openclaw@latest`

use std::path::PathBuf;

use super::AcpProviderConfig;

/// Default binary name for the OpenClaw ACP agent.
const OPENCLAW_BINARY: &str = "openclaw";

/// Build an [`AcpProviderConfig`] for the OpenClaw ACP agent.
pub fn build_config(work_dir: PathBuf) -> AcpProviderConfig {
    let command = crate::util::resolve_program(OPENCLAW_BINARY);
    AcpProviderConfig {
        command,
        args: vec!["acp".to_string(), "--reset-session".to_string()],
        env: vec![],
        env_remove: vec![],
        work_dir,
        session_mode: "high".to_string(), // OpenClaw uses thinking levels: off/minimal/low/medium/high/adaptive
        use_mcp_bridge: false, // OpenClaw doesn't support HTTP MCP; use <tool_call> XML extraction
        inject_mcp_url: false,
    }
}
