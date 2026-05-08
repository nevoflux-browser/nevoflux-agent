//! /loop skill: scheduled and event-triggered re-runs of prompts or wrapped skills.
//!
//! Spec: docs/superpowers/specs/2026-04-22-loop-skill-design.md

pub mod expression;
pub mod registry;
pub mod scheduler;
pub mod tool_classes;
pub mod types;
pub use expression::{ParseError, TabRef, TriggerExpr};
pub use registry::LoopRegistry;
pub use scheduler::{LoopFireRequest, TriggerScheduler};
pub use types::{LoopId, LoopRuntime};
