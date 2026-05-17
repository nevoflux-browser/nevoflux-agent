//! Real-data loaders for benchmark adapters.
//!
//! Each submodule parses a specific upstream format (BrowseComp encrypted
//! CSV, BrowseComp-ZH JSONL, Online-Mind2Web per-task directories) into a
//! `Vec<Task>`.  Adapters opt in by checking a Phase 3d env-var override
//! (`NEVOFLUX_BC_DATA_PATH`, `NEVOFLUX_BC_ZH_DATA_PATH`,
//! `NEVOFLUX_OM2W_DATA_PATH`); when unset the adapter falls back to the
//! checked-in Phase 3c fixture.
//!
//! See `eval/README-DATASETS.md` for upstream fetch procedures.

pub mod browsecomp_csv;
pub mod jsonl;
pub mod om2w_dir;
