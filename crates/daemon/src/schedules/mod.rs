//! Schedules subsystem (`/schedule` — routines-style cron + one-off jobs).
//!
//! Task 1.2 scope: cron parsing/validation and the `ScheduleId` newtype
//! only. Task 1.4 adds `events` (the `system:schedule:*` EventBus surface).
//! Later tasks add `manager`, `runner`, and `tools`.

pub mod cron;
pub mod events;
pub mod types;
