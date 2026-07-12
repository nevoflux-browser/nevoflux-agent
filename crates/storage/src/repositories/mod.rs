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
pub use loop_record::{LoopRepository, RecentIteration};
pub use memory::{MemoryRepository, StaleChunk, CURRENT_EMBEDDING_VERSION};
pub use message::MessageRepository;
pub use permission::PermissionRepository;
pub use schedule::ScheduleRepository;
pub use session::SessionRepository;
pub use site_adaptation::SiteAdaptationRepository;
pub use tool_stat::ToolStatsRepository;
pub use traces::TraceRepository;

/// Cap a "final text" summary field at 4096 chars before writing it to a
/// summary row. Shared by [`LoopRepository::finish_iteration`] (`loop_iterations.final_text`)
/// and [`ScheduleRepository::record_run_end`] (`schedule_runs.final_text`) — same
/// cap `daemon::loops::events` applies to loop iteration event payloads, so a
/// long run response doesn't bloat these summary rows (full transcripts live
/// in messages, not here).
pub(crate) fn truncate_final_text(s: &str) -> String {
    if s.chars().count() > 4096 {
        s.chars().take(4096).collect()
    } else {
        s.to_string()
    }
}
