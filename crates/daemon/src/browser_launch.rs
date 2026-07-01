//! Spawn and supervise the headless browser (automation/`--headless` mode, P2).
//! The daemon owns the lifecycle of exactly ONE browser: it launches the Gecko
//! build with a dedicated cloned profile, then waits for the extension to
//! auto-connect + register (readiness barrier) before the task starts.

use crate::registry::BrowserRegistry;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, Command};

/// How to launch the browser.
#[derive(Debug, Clone)]
pub struct BrowserLaunchConfig {
    /// Path to the nevoflux (Gecko fork) binary.
    pub browser_bin: PathBuf,
    /// Cloned per-task profile directory.
    pub profile_dir: PathBuf,
    /// X11 display (e.g. `:99` for Xvfb); `None` inherits env.
    pub display: Option<String>,
    /// How long to wait for the browser to register before failing.
    pub register_timeout: Duration,
}

/// Error launching or awaiting the browser.
#[derive(Debug, thiserror::Error)]
pub enum BrowserLaunchError {
    /// Failed to spawn the process.
    #[error("failed to spawn browser: {0}")]
    Spawn(#[from] std::io::Error),
    /// The browser did not register within the timeout.
    #[error("browser did not register within {0:?}")]
    RegisterTimeout(Duration),
}

/// Handle to a spawned browser process.
pub struct BrowserHandle {
    /// The child process (kept so it can be waited on / killed).
    pub child: Child,
}

/// Firefox/Gecko CLI args: dedicated profile, single instance (no remote).
pub fn browser_launch_args(profile_dir: &Path) -> Vec<String> {
    vec![
        "-no-remote".to_string(),
        "-profile".to_string(),
        profile_dir.to_string_lossy().into_owned(),
    ]
}

/// Spawn the browser and wait until it registers (its extension auto-connects,
/// P1). Returns once a browser is in the registry, or times out.
pub async fn spawn_and_supervise(
    cfg: BrowserLaunchConfig,
    registry: Arc<BrowserRegistry>,
) -> Result<BrowserHandle, BrowserLaunchError> {
    let mut cmd = Command::new(&cfg.browser_bin);
    cmd.args(browser_launch_args(&cfg.profile_dir));
    if let Some(display) = &cfg.display {
        cmd.env("DISPLAY", display);
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let child = cmd.spawn()?;

    if registry
        .wait_for_browser(cfg.register_timeout)
        .await
        .is_err()
    {
        return Err(BrowserLaunchError::RegisterTimeout(cfg.register_timeout));
    }
    Ok(BrowserHandle { child })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_args_include_profile_and_no_remote() {
        let args = browser_launch_args(Path::new("/tmp/clone"));
        assert!(args.contains(&"-no-remote".to_string()));
        let i = args
            .iter()
            .position(|a| a == "-profile")
            .expect("-profile present");
        assert_eq!(args[i + 1], "/tmp/clone");
    }
}
