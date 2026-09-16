//! On-device (local) LLM inference.
//!
//! Task 1.1 added the `[llm.local]` config shape. Task 1.2 added the
//! `LocalOnly` latch and its `egress_guard` (`crate::local::latch`). Task
//! 1.4 adds the endpoint registry (`crate::local::endpoint`) and the
//! raw-HTTP provider itself (`crate::wasm::local_llm`). Task 1.6 adds
//! `crate::local::sync`: the latch's config-change hook, which keeps the
//! llm-gateway's upstream (`crate::llm_gateway::apply_latch`) and a
//! `system:local:latch_changed` broadcast (topic:
//! `crate::local::latch::TOPIC_LATCH`) in sync with the latch. Task 2.1
//! adds three pure-logic modules with no engine-process dependency:
//! `gguf` (a hand-rolled GGUF header reader), `catalog` (the known models
//! and their download sources), and `memory` (KV-cache/compute size
//! estimation and GPU/CPU fit selection). Task 2.2 adds `hardware`: a
//! [`hardware::probe`] of this host's NVIDIA/Vulkan/cudart/RAM situation,
//! never run at daemon startup, plus the pure [`hardware::fallback_chain`]
//! that turns a probe + backend preference into the ordered list of
//! [`hardware::InstallKind`]s to try. Task 2.3 adds `release`: the pinned
//! table of what the engine release's downloadable archives actually ARE
//! (name, size, sha256, mirror/original download sources), generated from a
//! verified staging manifest by `scripts/engine-release/gen_release_rs.py`.
//! Task 2.4 adds `install`: downloading, extracting, verifying and
//! atomically installing one release's archives for a given `InstallKind`,
//! plus `marker` (the on-disk record of a completed install) and `state`
//! (today, just the shared `LocalError` type `install` returns -- Task 2.8
//! grows this into the full state/event model). Task 2.5 adds `harden`
//! (the exact argv/env an engine launch is allowed, so the process
//! Task 2.9 spawns can never expose `llama-server`'s web UI, agent tools,
//! or MCP attachment), `integrity` (a cold-start full re-hash of an
//! installed directory against its marker's manifest), and grows `marker`
//! with the upgrade/GC policy (`marker::tag_status`, `marker::should_gc`).
//! Task 2.6 adds `guard`: the `--engine-guard` subcommand that on Unix
//! directly parents a spawned engine process so it cannot outlive the
//! daemon that launched it (Windows gets this from the daemon's
//! kill-on-close Job Object instead; see `assign_self_to_kill_on_close_job`
//! in `src/main.rs`).
//!
//! A later task adds the engine supervisor that publishes to the endpoint
//! registry, calls [`apply_gateway_upstream_for_latch`] when it does, and
//! uses `catalog` + `memory` + `hardware` + `release` + `install` +
//! `harden` + `integrity` + `marker`'s policy functions to decide what to
//! install/launch and how -- and spawns engines through `guard` on Unix.

pub mod catalog;
pub mod config;
pub mod endpoint;
pub mod gguf;
pub mod guard;
pub mod harden;
pub mod hardware;
pub mod install;
pub mod integrity;
pub mod latch;
pub mod marker;
pub mod memory;
pub mod release;
pub mod state;
pub mod sync;
pub use config::*;
pub use sync::{apply_gateway_upstream_for_latch, on_config_changed, publish_current_latch_state};
