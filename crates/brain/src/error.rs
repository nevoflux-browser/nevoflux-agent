//! Error types for the brain crate.
//!
//! v1 surfaces a single [`BrainError`] enum and [`BrainResult`] alias. The
//! [`BrainError::NotImplemented`] variant exists specifically to ease the
//! M1 -> M3 transition: stub impls can return it cheaply, and the daemon
//! can pattern-match to degrade gracefully until `GbrainEngine` lands.

use thiserror::Error;

/// Errors returned by [`crate::BrainEngine`] and friends.
#[derive(Debug, Error)]
pub enum BrainError {
    /// Returned by any method that has no v1 implementation yet. M3 will
    /// remove most of these as the gbrain backend wires up.
    #[error("not implemented (M1 skeleton; landed in M3)")]
    NotImplemented,

    /// The requested page slug does not exist in the backend.
    #[error("page not found: {0}")]
    NotFound(String),

    /// Underlying I/O failure (filesystem, subprocess pipes, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Catch-all for backend-reported failures (e.g., gbrain CLI errors).
    #[error("backend error: {0}")]
    Backend(String),
}

/// Result alias used throughout the brain crate.
pub type BrainResult<T> = Result<T, BrainError>;
