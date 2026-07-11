//! Repository implementations for the storage layer.

mod artifact;
pub mod composition_asset;
mod config;
mod goal;
mod knowledge;
mod learning_metrics;
mod loop_record;
mod memory;
mod message;
mod permission;
mod schedule;
mod session;
mod site_adaptation;
mod tool_stat;
pub mod traces;

pub use artifact::ArtifactRepository;
pub use composition_asset::{CompositionAsset, CompositionAssetRepository};
pub use config::ConfigRepository;
pub use goal::GoalRepository;
pub use knowledge::KnowledgeRepository;
pub use learning_metrics::LearningMetricsRepository;
pub use loop_record::LoopRepository;
pub use memory::{MemoryRepository, StaleChunk, CURRENT_EMBEDDING_VERSION};
pub use message::MessageRepository;
pub use permission::PermissionRepository;
pub use schedule::ScheduleRepository;
pub use session::SessionRepository;
pub use site_adaptation::SiteAdaptationRepository;
pub use tool_stat::ToolStatsRepository;
pub use traces::TraceRepository;
