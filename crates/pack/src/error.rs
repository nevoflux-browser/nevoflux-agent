//! Phase-tagged errors for the pack engine.

use semver::Version;

pub type PackResult<T> = Result<T, PackError>;

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("manifest error: {0}")]
    Manifest(String),
    #[error("compatibility error: {0}")]
    Compat(String),
    #[error("host I/O error: {0}")]
    Host(String),
    #[error("pack '{name}' already installed at version {version}")]
    AlreadyInstalled { name: String, version: Version },
    #[error("pack '{0}' is not installed")]
    NotInstalled(String),
    #[error("install rolled back: {reason}")]
    RolledBack { reason: String },
}
