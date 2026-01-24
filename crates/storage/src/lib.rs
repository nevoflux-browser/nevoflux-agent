//! NevoFlux Storage - SQLite-based persistence layer
//!
//! Provides repository pattern access to sessions, messages, permissions, and config.

pub mod connection;
pub mod error;
mod migrations;

pub use connection::Database;
pub use error::{Result, StorageError};
