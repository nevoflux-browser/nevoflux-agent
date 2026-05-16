//! `ReleaseBrowser` — downloads a published nevoflux release tarball and
//! launches it headlessly. The only mode that produces `Authoritative` reports.
//!
//! Triggered by:
//! ```bash
//! # In nevoflux-agent repo:
//! just eval-release v0.3.2 online-mind2web
//! ```
//!
//! Or via CI on `repository_dispatch` from nevoflux's release workflow.
//!
//! Cache strategy: binaries are cached by version under `cache_dir`. A given
//! `v0.3.2` tarball is downloaded once and reused across runs. GHA `actions/cache`
//! mirrors this on the CI side so download cost is amortized to ~once per release.

use super::BrowserHandle;
use crate::{EvalError, EvalResult};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{info, warn};

const NEVOFLUX_REPO: &str = "dorisgyl/nevoflux";

pub struct ReleaseBrowser {
    version: String,
    install_dir: PathBuf,
    child: Mutex<Option<Child>>,
}

impl ReleaseBrowser {
    pub async fn launch(version: String, cache_dir: PathBuf) -> EvalResult<Self> {
        let install_dir = cache_dir.join(&version);

        if !install_dir.exists() {
            info!(version = %version, cache = ?cache_dir, "downloading nevoflux release");
            Self::download_release(&version, &cache_dir, &install_dir).await?;
        } else {
            info!(version = %version, "using cached nevoflux release");
        }

        let child = Self::spawn_headless(&install_dir).await?;

        Ok(Self {
            version,
            install_dir,
            child: Mutex::new(Some(child)),
        })
    }

    async fn download_release(version: &str, cache_dir: &Path, install_dir: &Path) -> EvalResult<()> {
        tokio::fs::create_dir_all(cache_dir).await?;

        // Resolve platform-specific asset name.
        let asset_name = Self::platform_asset_name(version)?;

        // Prefer `gh` CLI (handles auth) — falls back to curl with public URL.
        let download_cmd = if which::which("gh").is_ok() {
            format!(
                "gh release download {} --repo {} --pattern '{}' --dir {}",
                version,
                NEVOFLUX_REPO,
                asset_name,
                cache_dir.display()
            )
        } else {
            let url = format!(
                "https://github.com/{}/releases/download/{}/{}",
                NEVOFLUX_REPO, version, asset_name
            );
            format!("curl -L -o {}/{} {}", cache_dir.display(), asset_name, url)
        };

        let status = Command::new("sh").arg("-c").arg(&download_cmd).status().await?;
        if !status.success() {
            return Err(EvalError::Other(format!(
                "download failed: `{}` exited {}",
                download_cmd, status
            )));
        }

        // Extract.
        tokio::fs::create_dir_all(install_dir).await?;
        let tarball = cache_dir.join(&asset_name);
        let status = Command::new("tar")
            .arg("xzf")
            .arg(&tarball)
            .arg("-C")
            .arg(install_dir)
            .status()
            .await?;
        if !status.success() {
            return Err(EvalError::Other(format!(
                "extract failed: tar xzf {:?}",
                tarball
            )));
        }

        Ok(())
    }

    fn platform_asset_name(version: &str) -> EvalResult<String> {
        let v = version.trim_start_matches('v');
        let target = if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
            "linux-x86_64"
        } else if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
            "darwin-arm64"
        } else if cfg!(target_os = "macos") && cfg!(target_arch = "x86_64") {
            "darwin-x86_64"
        } else if cfg!(target_os = "windows") {
            "windows-x86_64"
        } else {
            return Err(EvalError::Other(format!(
                "no nevoflux release artifact mapping for current platform"
            )));
        };
        Ok(format!("nevoflux-{}-{}.tar.gz", v, target))
    }

    async fn spawn_headless(install_dir: &Path) -> EvalResult<Child> {
        // The actual binary path inside the tarball — adjust to match nevoflux's
        // release packaging convention. Common layout: `<install>/nevoflux/zen`.
        let binary = install_dir.join("nevoflux").join("zen");
        let binary = if binary.exists() {
            binary
        } else {
            // Fallback: search for a likely entry point.
            install_dir.join("zen")
        };

        info!(binary = ?binary, "launching nevoflux headless");

        let child = Command::new(&binary)
            .arg("--headless")
            .arg("--remote-debugging-port=5959")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                EvalError::Other(format!(
                    "failed to spawn nevoflux binary at {:?}: {}",
                    binary, e
                ))
            })?;

        // Give it a moment to bind the port; replace with real readiness probe.
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;

        Ok(child)
    }
}

#[async_trait]
impl BrowserHandle for ReleaseBrowser {
    async fn ensure_ready(&self) -> EvalResult<()> {
        // TODO: real readiness probe — open a debugging-port socket and check.
        Ok(())
    }

    async fn shutdown(&self) -> EvalResult<()> {
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take() {
            if let Err(e) = child.kill().await {
                warn!(error = %e, "failed to kill nevoflux child cleanly");
            }
        }
        Ok(())
    }

    fn version_string(&self) -> String {
        format!("nevoflux-{}", self.version)
    }

    fn is_real_browser(&self) -> bool {
        true
    }
}

impl Drop for ReleaseBrowser {
    fn drop(&mut self) {
        // Best-effort sync kill on Drop. Prefer explicit `shutdown().await` in code.
        if let Ok(mut guard) = self.child.try_lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.start_kill();
            }
        }
    }
}

// Tiny shim to avoid pulling in the full `which` crate for one call.
mod which {
    use std::path::PathBuf;
    pub fn which(name: &str) -> Result<PathBuf, ()> {
        let path = std::env::var_os("PATH").ok_or(())?;
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
        Err(())
    }
}
