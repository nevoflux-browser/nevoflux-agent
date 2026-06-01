//! Static configuration for the gbrain subprocess supervisor.
//!
//! See [`GbrainConfig`] for field-level documentation; values typically
//! come from `[knowledge_base]` in the daemon TOML config, with the
//! upstream gateway URL + bearer token plumbed in by `init_brain()`
//! from the live [`crate::llm_gateway`] state at boot.

use std::path::PathBuf;
use std::time::Duration;

/// Static config for spawning + supervising a `gbrain serve` subprocess.
///
/// All paths must be absolute and pre-validated by the caller — the
/// supervisor does not attempt path resolution or `which`-style lookup.
/// Empty / nonexistent paths cause spawn to fail and the supervisor to
/// transition to [`crate::gbrain::SupervisorState::Failed`] after the
/// restart budget is exhausted.
#[derive(Clone, Debug)]
pub struct GbrainConfig {
    /// Path to the bun executable. On Windows typically
    /// `C:\Users\<user>\.bun\bin\bun.exe`; on Unix typically the
    /// absolute path to `bun` in `$HOME/.bun/bin/bun` or system bin.
    pub bun_path: PathBuf,

    /// Path to gbrain's `cli.ts` entry point inside
    /// `node_modules/gbrain/src/cli.ts`.
    ///
    /// gbrain 0.40.8.1's `bin` field points at a TypeScript file that
    /// `bunx` can't resolve correctly, so the supervisor invokes
    /// `bun run <cli.ts> serve` directly (spike S5 finding).
    pub gbrain_cli_path: PathBuf,

    /// Brain repository directory. Forwarded to the subprocess as
    /// `$GBRAIN_BRAIN_DIR`.
    ///
    /// gbrain 0.40.8.1 ignores the `--brain-dir` CLI flag but does honor
    /// this env var (spike 附录 B operational quirk #1).
    pub brain_dir: PathBuf,

    /// Bare upstream gateway bind address (e.g. `http://127.0.0.1:54321`,
    /// no path). The supervisor appends `/v1` before forwarding it as
    /// `OPENAI_BASE_URL` (native `openai` embedding recipe) and
    /// `OPENROUTER_BASE_URL` (OpenAI-compatible `openrouter` chat recipe),
    /// because gbrain's OpenAI-protocol SDKs assume the base URL already
    /// includes the `/v1` segment and only append the operation path, while
    /// the gateway nests every route under `/v1`. Omitting the suffix makes
    /// embeds 404 as `[embed(...)] Not Found`.
    pub upstream_base_url: String,

    /// Bearer token gbrain presents on every upstream request.
    /// Forwarded as `OPENROUTER_API_KEY` (see [`Self::upstream_base_url`]).
    pub upstream_api_key: String,

    /// Max time to wait for the MCP `initialize` handshake after spawn.
    /// Spike S5 measured cold-start spawn -> initialize at ~3 s on a
    /// warm Bun cache; 10 s is a comfortable production default.
    pub initialize_timeout: Duration,

    /// Max time to wait for a single `tools/call` request.
    pub request_timeout: Duration,

    /// Max restarts allowed within [`Self::restart_window`] before the
    /// supervisor gives up and transitions to
    /// [`crate::gbrain::SupervisorState::Failed`].
    pub max_restarts_within_window: u32,

    /// Sliding window for [`Self::max_restarts_within_window`].
    pub restart_window: Duration,

    /// Initial restart backoff. Doubles after each restart, capped at
    /// [`Self::max_restart_backoff`].
    pub initial_restart_backoff: Duration,

    /// Upper bound on the exponentially-growing restart backoff.
    pub max_restart_backoff: Duration,
}

impl GbrainConfig {
    /// Minimal config for unit tests — every path is intentionally
    /// invalid so a test that accidentally reaches the spawn path fails
    /// fast rather than launching real `bun` / `gbrain` processes.
    #[cfg(test)]
    pub fn test_default() -> Self {
        Self {
            bun_path: PathBuf::from("/nonexistent/bun"),
            gbrain_cli_path: PathBuf::from("/nonexistent/gbrain/cli.ts"),
            brain_dir: PathBuf::from("/nonexistent/brain"),
            upstream_base_url: "http://127.0.0.1:1".into(),
            upstream_api_key: "test-key".into(),
            initialize_timeout: Duration::from_secs(120),
            request_timeout: Duration::from_secs(30),
            max_restarts_within_window: 3,
            restart_window: Duration::from_secs(60),
            initial_restart_backoff: Duration::from_millis(500),
            max_restart_backoff: Duration::from_secs(30),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_paths_are_nonexistent() {
        // Invariant: test_default must NOT be runnable end-to-end.
        let cfg = GbrainConfig::test_default();
        assert!(!cfg.bun_path.exists(), "test bun_path must not exist");
        assert!(
            !cfg.gbrain_cli_path.exists(),
            "test gbrain_cli_path must not exist"
        );
    }

    #[test]
    fn test_default_has_sane_durations() {
        let cfg = GbrainConfig::test_default();
        assert!(cfg.initialize_timeout > Duration::ZERO);
        assert!(cfg.request_timeout > Duration::ZERO);
        assert!(cfg.initial_restart_backoff > Duration::ZERO);
        assert!(cfg.max_restart_backoff >= cfg.initial_restart_backoff);
        assert!(cfg.restart_window > Duration::ZERO);
        assert!(cfg.max_restarts_within_window > 0);
    }

    #[test]
    fn test_config_is_clone() {
        let cfg = GbrainConfig::test_default();
        let cloned = cfg.clone();
        assert_eq!(cloned.upstream_base_url, cfg.upstream_base_url);
        assert_eq!(cloned.upstream_api_key, cfg.upstream_api_key);
    }
}
