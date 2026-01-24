//! NevoFlux Storage - SQLite-based persistence layer
//!
//! Provides repository pattern access to sessions, messages, permissions, and config.

pub mod connection;
pub mod error;
mod migrations;
pub mod models;
pub mod repositories;

pub use connection::Database;
pub use error::{Result, StorageError};

// Re-export model types for convenience
pub use models::{
    CheckPermissionParams, ConfigEntry, ContentType, CreateMessageParams, CreatePermissionParams,
    CreateSessionParams, ListMessagesParams, ListSessionsParams, Message, MessageRole, Permission,
    PermissionScope, Session, SessionMode, UpdateSessionParams,
};

// Re-export repository types for convenience
pub use repositories::{
    ConfigRepository, MessageRepository, PermissionRepository, SessionRepository,
};
