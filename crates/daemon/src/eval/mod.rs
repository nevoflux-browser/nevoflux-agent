//! NEVOFLUX_EVAL_MODE support — see docs/superpowers/specs/2026-05-15-browser-use-eval-design.md §7.2

pub mod config;

pub use config::{from_env, EvalConfig, EvalConfigError};
