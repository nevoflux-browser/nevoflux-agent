//! Schedules subsystem (`/schedule` — routines-style cron + one-off jobs).
//!
//! Task 1.2 scope: cron parsing/validation and the `ScheduleId` newtype
//! only. Task 1.4 adds `events` (the `system:schedule:*` EventBus surface).
//! Task 1.5 adds `manager` (due-tick engine, lifecycle, boot rearm, missed
//! detection, idle inhibitor) and `runner` (single-run executor). A later
//! task adds `tools`.

pub mod cron;
pub mod events;
pub mod manager;
pub mod runner;
pub mod types;
