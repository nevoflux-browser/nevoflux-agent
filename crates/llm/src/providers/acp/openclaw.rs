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
        args: vec!["acp".to_string()],
        env: vec![],
        env_remove: vec![],
        work_dir,
        session_mode: "default".to_string(),
        use_mcp_bridge: true,
        inject_mcp_url: false, // OpenClaw registers MCP via gateway config
    }
}
