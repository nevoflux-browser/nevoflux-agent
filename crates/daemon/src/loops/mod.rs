//! /loop skill: scheduled and event-triggered re-runs of prompts or wrapped skills.
//!
//! Spec: docs/superpowers/specs/2026-04-22-loop-skill-design.md

pub mod expression;
pub use expression::{ParseError, TabRef, TriggerExpr};
