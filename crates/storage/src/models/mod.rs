//! Data models for the storage layer.

pub(crate) mod artifact;
mod config;
pub(crate) mod knowledge;
mod memory;
mod message;
mod permission;
mod session;

pub use artifact::{ArtifactRecord, CreateArtifactParams};
pub use config::ConfigEntry;
pub use knowledge::{CreateKnowledgeParams, Knowledge};
pub use memory::MemoryChunk;
pub use message::{ContentType, CreateMessageParams, ListMessagesParams, Message, MessageRole};
pub use permission::{CheckPermissionParams, CreatePermissionParams, Permission, PermissionScope};
pub use session::{
    current_timestamp, uuid_v4, CleanupPolicy, CleanupResult, CreateSessionParams,
    ListSessionsParams, Session, SessionMode, UpdateSessionParams,
};
