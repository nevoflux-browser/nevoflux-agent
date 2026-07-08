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
