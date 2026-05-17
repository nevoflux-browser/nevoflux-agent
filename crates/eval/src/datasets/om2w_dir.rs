//! Stub — implemented in Task 9.
//!
//! See Task 9 of `docs/superpowers/plans/2026-05-17-eval-phase3d-real-data-and-polish.md`.

use crate::{EvalError, EvalResult, Task};
use std::path::Path;

#[allow(dead_code)]
pub fn load(_data_dir: &Path) -> EvalResult<Vec<Task>> {
    Err(EvalError::Other("datasets::om2w_dir::load not implemented yet (Phase 3d Task 9 pending)".into()))
}
