//! Browser launch abstraction.
//!
//! Three modes, each with different signal grades and use cases:
//!
//! | Mode              | Use case                       | Signal grade  |
//! |-------------------|--------------------------------|---------------|
//! | `DaemonOnly`      | daemon-only tier; no browser   | Exploratory   |
//! | `ExternalDevInstance` | dev iteration loop         | Exploratory   |
//! | `ReleaseBinary`   | release verification / CI      | Authoritative |
//!
//! Implementation status:
//! - `DaemonOnlyBrowser` (DaemonOnly): ✅ complete
//! - `DevInstanceBrowser`:             🚧 stub — see dev_instance.rs
//! - `ReleaseBrowser`:                 🚧 stub — see release_binary.rs

use crate::daemon_client::lock::DaemonLock;
use crate::EvalResult;
use async_trait::async_trait;
use std::path::PathBuf;

pub mod daemon_only;
pub mod dev_instance;
pub mod release_binary;

pub use daemon_only::DaemonOnlyBrowser;

/// How to obtain a nevoflux browser instance to drive during eval.
#[derive(Debug, Clone)]
pub enum BrowserLaunchMode {
    /// No browser. Spawn daemon as subprocess.
    /// Tasks with `requires_browser = true` are skipped.
    /// Always produces Exploratory signal grade.
    DaemonOnly {
        /// Path to daemon binary. Default resolution in CLI.
        daemon_binary: PathBuf,
        /// State dir for lock + traces.db.
        state_dir: PathBuf,
    },

    /// Connect to an already-running nevoflux instance.
    /// User started it manually (e.g. `just dev` in the nevoflux repo).
    /// Always produces Exploratory signal grade — the browser is a dev build,
    /// not a published artifact users can install.
    ExternalDevInstance {
        /// Remote debugging endpoint, e.g. `http://localhost:5959`.
        endpoint: String,
    },

    /// Download and launch a published nevoflux release tarball.
    /// Produces Authoritative signal grade — reports reflect a binary
    /// users can actually install.
    ReleaseBinary {
        /// Release tag, e.g. `v0.3.2`.
        version: String,
        /// Cache directory for downloaded binaries.
        cache_dir: PathBuf,
    },
}

impl BrowserLaunchMode {
    pub fn signal_grade(&self) -> crate::SignalGrade {
        match self {
            Self::ReleaseBinary { .. } => crate::SignalGrade::Authoritative,
            Self::DaemonOnly { .. } | Self::ExternalDevInstance { .. } => {
                crate::SignalGrade::Exploratory
            }
        }
    }

    pub fn supports_browser_tasks(&self) -> bool {
        !matches!(self, Self::DaemonOnly { .. })
    }
}

/// A running (or virtual) browser instance the runner can drive.
#[async_trait]
pub trait BrowserHandle: Send + Sync {
    /// Block until the browser is ready to accept commands.
    async fn ensure_ready(&self) -> EvalResult<()>;

    /// Clean shutdown. Called by Drop fallback if not explicitly invoked.
    async fn shutdown(&self) -> EvalResult<()>;

    /// Human-readable version identifier for reports.
    /// Examples: "nevoflux-v0.3.2" / "dev-build (port 5959)" / "no-browser"
    fn version_string(&self) -> String;

    /// Whether this is a real navigable browser. `false` for DaemonOnly.
    fn is_real_browser(&self) -> bool;

    /// Returns the daemon lock if this handle owns a daemon subprocess
    /// (currently only DaemonOnlyBrowser). Other modes return None — they
    /// will gain their own discovery paths in Phase 3+.
    fn lock(&self) -> Option<&DaemonLock> {
        None
    }
}

/// Construct a browser handle from a launch mode.
pub async fn launch(mode: &BrowserLaunchMode) -> EvalResult<Box<dyn BrowserHandle>> {
    match mode {
        BrowserLaunchMode::DaemonOnly { daemon_binary, state_dir } => {
            Ok(Box::new(
                DaemonOnlyBrowser::spawn(daemon_binary.clone(), state_dir.clone()).await?,
            ))
        }
        BrowserLaunchMode::ExternalDevInstance { endpoint } => Ok(Box::new(
            dev_instance::DevInstanceBrowser::connect(endpoint.clone()).await?,
        )),
        BrowserLaunchMode::ReleaseBinary { version, cache_dir } => Ok(Box::new(
            release_binary::ReleaseBrowser::launch(version.clone(), cache_dir.clone()).await?,
        )),
    }
}
