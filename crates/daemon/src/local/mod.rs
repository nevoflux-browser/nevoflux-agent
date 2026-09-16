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
//! in `src/main.rs`). Task 2.7 adds `admission`: the P0 (interactive)
//! /P1 (background) token-budget admission controller
//! ([`admission::Admission`]) that gates every local-engine call through
//! `crate::wasm::local_llm::run_admitted`, plus the [`admission::PRIORITY`]
//! task-local / [`admission::background`] a caller uses to mark a
//! `tokio::spawn`ed job as P1. A fix round hardened this further: the
//! budget is runtime-reconfigurable ([`admission::Admission::set_capacity_tokens`] /
//! [`admission::Admission::set_queue_depth`]) rather than a boot-time
//! constant, a preempted background caller re-queues under the same
//! retry budget instead of losing its work, and a background caller never
//! cold-starts the engine.
//!
//! Task 2.9 adds `engine`: the supervisor that assembles all of the above
//! into one running process. It publishes to the endpoint registry (and
//! registers the cold-start hook `endpoint::ensure` falls back to), uses
//! `catalog` + `memory` + `hardware` + `release` + `install` + `harden` +
//! `integrity` + `marker`'s policy functions to decide what to launch and
//! how, spawns engines through `guard` on Unix, calls
//! [`admission::Admission::set_capacity_tokens`] with the real context pool
//! once launch confirms it (see that function's doc comment), and
//! re-exports [`admission::admission`] for convenience.
//!
//! Task 2.10 adds `rpc`: the `local.*` RPC commands the browser calls
//! (status/probe/models/plan/install/cancel/set_default/update_engine/
//! repair_engine/retry_backend/set_config), dispatched from `server.rs`
//! next to the `models.*` arms. It fills the one gap `engine` deliberately
//! leaves open -- `EngineSupervisor::cold_start` only ever launches an
//! install that already exists on disk -- by downloading the engine archive
//! (via `install::install`) and the model GGUF (its own
//! `rpc::download_model`, mirroring `crate::models::download_asset`) before
//! handing off to `EngineSupervisor::ensure_started`. Task 2.10's ruling R53
//! accessor for where downloaded model weights live lives beside its
//! `NEVOFLUX_LOCAL_CACHE_DIR` sibling instead: [`install::local_models_dir`],
//! re-exported below (fix round 1, Minor 11).

pub mod admission;
pub mod catalog;
pub mod config;
pub mod endpoint;
pub mod engine;
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
pub mod rpc;
pub mod state;
pub mod sync;
pub use config::*;
pub use install::local_models_dir;
pub use sync::{apply_gateway_upstream_for_latch, on_config_changed, publish_current_latch_state};
