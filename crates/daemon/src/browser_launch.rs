//! Spawn and supervise the headless browser (automation/`--headless` mode, P2).
//! The daemon owns the lifecycle of exactly ONE browser: it launches the Gecko
//! build with a dedicated cloned profile, then waits for the extension to
//! auto-connect + register (readiness barrier) before the task starts.

use crate::registry::{BrowserEntry, BrowserRegistry};
use std::collections::HashSet;
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

impl BrowserHandle {
    /// Reap the spawned child. On Windows the child is the short-lived *launcher*
    /// process (it relaunches the real browser as a separate, re-parented tree
    /// and exits), so killing it by pid is both useless and unsafe (pid reuse) —
    /// the real teardown is done by [`kill_profile_processes`], which matches the
    /// unique clone-profile path. Here we only start_kill + wait to close the
    /// handle and avoid a zombie.
    pub async fn terminate(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

/// Kill every process whose command line references `profile_dir` — the browser
/// launched with that cloned profile plus its content processes. This is the
/// reliable teardown: the Windows launcher process exits after relaunching the
/// real browser under a new (soon-orphaned) pid, so pid-based kills miss it, but
/// the relaunched process still carries `-profile <clone>` on its command line.
/// A digit boundary keeps `default-5` from also matching `default-50`.
pub async fn kill_profile_processes(profile_dir: &Path) {
    let path = profile_dir.to_string_lossy();
    #[cfg(windows)]
    {
        let escaped = path.replace('\'', "''");
        // -match on an escaped literal path followed by a non-digit (or end).
        let script = format!(
            "Get-CimInstance Win32_Process | Where-Object {{ $_.CommandLine -match ([regex]::Escape('{escaped}') + '([^0-9]|$)') }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}"
        );
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    #[cfg(unix)]
    {
        // pkill -f matches the pattern (ERE) against the whole command line.
        let pattern = format!("{}([^0-9]|$)", regex_escape(&path));
        let _ = Command::new("pkill")
            .args(["-9", "-f", &pattern])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
}

/// Minimal ERE metacharacter escaping for the Unix `pkill -f` pattern.
#[cfg(unix)]
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Firefox/Gecko CLI args: dedicated profile, single instance (no remote).
pub fn browser_launch_args(profile_dir: &Path) -> Vec<String> {
    vec![
        "-no-remote".to_string(),
        "-profile".to_string(),
        profile_dir.to_string_lossy().into_owned(),
    ]
}

/// Build and spawn the browser process (shared by [`spawn_and_supervise`] and
/// [`spawn_and_supervise_excluding`]) — env/arg setup lives here exactly once.
fn spawn_browser_process(cfg: &BrowserLaunchConfig) -> Result<Child, BrowserLaunchError> {
    let mut cmd = Command::new(&cfg.browser_bin);
    cmd.args(browser_launch_args(&cfg.profile_dir));
    if let Some(display) = &cfg.display {
        cmd.env("DISPLAY", display);
    }
    // The native-messaging proxy is spawned by the browser and inherits its env.
    // NEVOFLUX_PROXY_ROLE=browser makes the proxy declare role:"browser" so the
    // daemon registers it as the automation target (P2 T5). Propagate
    // NEVOFLUX_DATA_DIR (if set) so the proxy discovers THIS daemon's port file.
    cmd.env("NEVOFLUX_PROXY_ROLE", "browser");
    if let Ok(data_dir) = std::env::var("NEVOFLUX_DATA_DIR") {
        cmd.env("NEVOFLUX_DATA_DIR", data_dir);
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    Ok(cmd.spawn()?)
}

/// Spawn the browser and wait until it registers (its extension auto-connects,
/// P1). Returns once a browser is in the registry, or times out.
pub async fn spawn_and_supervise(
    cfg: BrowserLaunchConfig,
    registry: Arc<BrowserRegistry>,
) -> Result<BrowserHandle, BrowserLaunchError> {
    let child = spawn_browser_process(&cfg)?;

    if registry
        .wait_for_browser(cfg.register_timeout)
        .await
        .is_err()
    {
        return Err(BrowserLaunchError::RegisterTimeout(cfg.register_timeout));
    }
    Ok(BrowserHandle { child })
}

/// Spawn the browser and wait until a browser *not already in* `exclude`
/// registers, returning the bound [`BrowserEntry`] alongside the handle.
/// Used by headless scheduled launches so the newly-spawned instance is the
/// one that gets bound, even while the user's already-registered live browser
/// (in `exclude`) is still connected — see [`BrowserRegistry::wait_for_new_browser`].
pub async fn spawn_and_supervise_excluding(
    cfg: BrowserLaunchConfig,
    registry: Arc<BrowserRegistry>,
    exclude: &HashSet<String>,
) -> Result<(BrowserHandle, BrowserEntry), BrowserLaunchError> {
    let child = spawn_browser_process(&cfg)?;

    match registry
        .wait_for_new_browser(exclude, cfg.register_timeout)
        .await
    {
        Ok(entry) => Ok((BrowserHandle { child }, entry)),
        Err(_) => Err(BrowserLaunchError::RegisterTimeout(cfg.register_timeout)),
    }
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
