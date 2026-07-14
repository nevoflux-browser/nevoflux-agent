//! Antigravity CLI ACP configuration.
//!
//! Google's `agy` CLI does not speak ACP; the community adapter
//! `antigravity-acp` (https://github.com/shubzkothekar/antigravity-acp)
//! bridges it: it spawns `agy` one-shot per prompt and translates its SQLite
//! conversation DB into ACP updates. Install: download the platform binary
//! from the adapter's GitHub releases, rename to `antigravity-acp`
//! (`antigravity-acp.exe` on Windows), and put it on PATH.
//!
//! Key adapter facts this config depends on (verified against v1.0.0):
//! - `newSession` DROPS `mcp_servers` — MCP is injected instead via a
//!   project-local `.agents/mcp_config.json` in the sandbox workspace
//!   (see `daemon::antigravity_setup`), hence `inject_mcp_url: false`.
//! - `AGY_EXTRA_ARGS` (documented env) is spliced verbatim into every agy
//!   spawn — used here for `--model` and `--dangerously-skip-permissions`.
//! - The adapter NEVER sends `session/request_permission`, so the daemon-side
//!   HTTP MCP gate must enforce permissions: `gate_tool_calls: true`.
//! - agy's one-shot `-p` mode has no TTY: its own interactive permission
//!   prompts would hang, so bypass is mandatory; the blast radius of agy's
//!   built-in coding tools is bounded by a sandbox work_dir instead.

use std::path::PathBuf;

use super::AcpProviderConfig;

/// Default binary name for the antigravity ACP adapter.
const ANTIGRAVITY_ACP_BINARY: &str = "antigravity-acp";

/// Build an [`AcpProviderConfig`] for the Antigravity ACP agent.
pub fn build_config(model: &str, work_dir: PathBuf) -> AcpProviderConfig {
    let command = crate::util::resolve_program(ANTIGRAVITY_ACP_BINARY);

    let mut extra = String::from("--dangerously-skip-permissions");
    if !model.is_empty() && model != "default" {
        extra = format!("--model {model} {extra}");
    }

    let mut env = vec![("AGY_EXTRA_ARGS".to_string(), extra)];
    // Point the adapter at the user's installed `agy`. The adapter does NOT
    // consult PATH: it looks in its own managed dir and otherwise auto-downloads
    // agy — and that download URL currently 404s
    // (github.com/google-antigravity/antigravity-cli/releases/.../agy_cli_windows_x64.zip),
    // so without this the provider fails at first prompt with "agy not found".
    // `resolve_program` returns an absolute path when agy is found on the
    // (npm-extended) PATH, or the bare name on miss — only set AGY_BIN on a hit.
    let agy = crate::util::resolve_program("agy");
    if agy.is_absolute() {
        if let Some(agy) = agy.to_str() {
            env.push(("AGY_BIN".to_string(), agy.to_string()));
        }
    }

    AcpProviderConfig {
        command,
        args: vec![],
        env,
        env_remove: vec![],
        work_dir,
        // Must match the adapter's default mode id so `session/set_mode`
        // (which the adapter does not implement) is never sent. Adjust per
        // Task 0 Step 4 probe if the adapter reports a different default.
        session_mode: "default".to_string(),
        use_mcp_bridge: true,
        inject_mcp_url: false,
        gate_tool_calls: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bypass_always_present_model_only_when_set() {
        let cfg = build_config("gemini-3-pro", PathBuf::from("."));
        let (k, v) = &cfg.env[0];
        assert_eq!(k, "AGY_EXTRA_ARGS");
        assert!(v.contains("--model gemini-3-pro"));
        assert!(v.contains("--dangerously-skip-permissions"));

        let cfg = build_config("default", PathBuf::from("."));
        assert_eq!(cfg.env[0].1, "--dangerously-skip-permissions");
        let cfg = build_config("", PathBuf::from("."));
        assert_eq!(cfg.env[0].1, "--dangerously-skip-permissions");
    }

    #[test]
    fn mcp_via_agents_config_not_session_injection() {
        let cfg = build_config("", PathBuf::from("."));
        assert!(cfg.use_mcp_bridge);
        assert!(!cfg.inject_mcp_url);
        assert!(cfg.gate_tool_calls);
    }
}
