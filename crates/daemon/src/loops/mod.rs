//! /loop skill: scheduled and event-triggered re-runs of prompts or wrapped skills.
//!
//! Spec: docs/superpowers/specs/2026-04-22-loop-skill-design.md

pub mod combinator;
pub mod dynamic;
pub mod events;
pub mod executor;
pub mod expression;
pub mod manager;
pub mod registry;
pub mod scheduler;
pub mod tool_classes;
pub mod tools;
pub mod types;
pub use executor::{ExecResult, IterationExecutor};
pub use expression::{ParseError, TabRef, TriggerExpr};
pub use manager::{CreateLoopArgs, LoopManager};
pub use registry::LoopRegistry;
pub use scheduler::{LoopFireRequest, TriggerScheduler};
pub use tools::{execute_loop_tool, ToolCallContext};
pub use types::{LoopId, LoopRuntime};
