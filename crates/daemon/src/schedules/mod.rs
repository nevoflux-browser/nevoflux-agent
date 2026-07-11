//! Schedules subsystem (`/schedule` — routines-style cron + one-off jobs).
//!
//! Task 1.2 scope: cron parsing/validation and the `ScheduleId` newtype
//! only. Task 1.4 adds `events` (the `system:schedule:*` EventBus surface).
//! Task 1.5 adds `manager` (due-tick engine, lifecycle, boot rearm, missed
//! detection, idle inhibitor) and `runner` (single-run executor). Task 1.6
//! adds `tools` (the `schedule_*` LLM-callable tool dispatcher).

pub mod cron;
pub mod events;
pub mod manager;
pub mod runner;
pub mod tools;
pub mod types;

pub use manager::{CreateScheduleArgs, ScheduleManager};
pub use tools::{execute_schedule_tool, ScheduleToolContext};
pub use types::ScheduleId;

/// Process-global handle to the daemon's `ScheduleManager`, set once at daemon
/// startup (see `server.rs` right after the manager is constructed).
///
/// Mirrors [`crate::loops::CURRENT_LOOP_MANAGER`]. Used by
/// `agent_exec::run_agent_once` to back-fill `HostServices.schedule_manager`
/// into the per-run services clone: the automation/schedule-runner services
/// snapshots are captured BEFORE `with_schedule_manager` runs (chicken-and-egg
/// at boot), so an unattended run's read-only `schedule_*` tools
/// (`schedule_list` / `schedule_runs`) would otherwise fail with a misleading
/// "daemon was started without a ScheduleManager" error.
pub static CURRENT_SCHEDULE_MANAGER: std::sync::OnceLock<std::sync::Arc<ScheduleManager>> =
    std::sync::OnceLock::new();
