//! Schedules subsystem (`/schedule` — routines-style cron + one-off jobs).
//!
//! Task 1.2 scope: cron parsing/validation and the `ScheduleId` newtype
//! only. Later tasks add `events`, `manager`, `runner`, and `tools`.

pub mod cron;
pub mod types;
