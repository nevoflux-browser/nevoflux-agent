//! NevoFlux Storage - SQLite-based persistence layer
//!
//! Provides repository pattern access to sessions, messages, permissions, and config.

pub mod error;

pub use error::{Result, StorageError};
