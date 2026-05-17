//! Stub — implemented in Task 8.
//!
//! See Task 8 of `docs/superpowers/plans/2026-05-17-eval-phase3d-real-data-and-polish.md`.

use crate::{EvalError, EvalResult, Task};
use std::path::Path;

#[allow(dead_code)]
pub fn load(_path: &Path, _category: &str, _prompt_suffix: &str) -> EvalResult<Vec<Task>> {
    Err(EvalError::Other("datasets::jsonl::load not implemented yet (Phase 3d Task 8 pending)".into()))
}
