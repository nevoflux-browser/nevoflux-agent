//! Error types for the daemon.

use thiserror::Error;

/// Result type for daemon operations.
pub type Result<T> = std::result::Result<T, DaemonError>;

/// Errors that can occur during daemon operations.
#[derive(Debug, Error)]
pub enum DaemonError {
    /// Storage error.
    #[error("Storage error: {0}")]
    StorageError(#[from] nevoflux_storage::StorageError),

    /// Session not found.
    #[error("Session not found: {0}")]
    SessionNotFound(String),

    /// Proxy not found.
    #[error("Proxy not found: {0}")]
    ProxyNotFound(String),

    /// Request not found.
    #[error("Request not found: {0}")]
    RequestNotFound(String),

    /// Session already has an active request.
    #[error("Session {0} already has an active request")]
    SessionBusy(String),

    /// Configuration error.
    #[error("Configuration error: {0}")]
    ConfigError(String),

    /// Router error.
    #[error("Router error: {0}")]
    RouterError(String),

    /// Context building error.
    #[error("Context error: {0}")]
    ContextError(String),

    /// Permission denied.
    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    /// IO error.
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    /// Serialization error.
    #[error("Serialization error: {0}")]
    SerializationError(String),

    /// Internal error.
    #[error("Internal error: {0}")]
    InternalError(String),

    /// No available port in range.
    #[error("No available port in range")]
    PortExhausted,
}

impl From<serde_json::Error> for DaemonError {
    fn from(err: serde_json::Error) -> Self {
        DaemonError::SerializationError(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = DaemonError::SessionNotFound("sess-001".to_string());
        assert!(err.to_string().contains("sess-001"));
    }

    #[test]
    fn test_error_from_serde() {
        let json_err = serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
        let daemon_err: DaemonError = json_err.into();
        assert!(matches!(daemon_err, DaemonError::SerializationError(_)));
    }
}
