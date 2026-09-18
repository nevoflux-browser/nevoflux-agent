//! State model, errors and events for on-device (local) inference.
//!
//! Task 2.4 had to create this file early (commit `0c41b46`) because
//! `install()` (`crate::local::install`) needs *a* shared error type to
//! return before this task ever runs — it defined only the five
//! [`LocalError`] variants installation itself can produce, using the
//! exact shape this task's brief already specified for them. This task
//! (2.8, "Local state, errors and events") owns the full shape the rest of
//! the on-device pipeline grows into:
//!
//! - [`LocalState`]: the state machine Task 2.9's engine supervisor drives
//!   and Task 2.10's RPC surface serializes to the browser.
//! - The remaining eight [`LocalError`] variants (`BackendUnavailable`,
//!   `EngineCrash`, `EngineInsecure`, `EngineCorrupt`, `EngineUpdateRequired`,
//!   `ModelTooLargeForMemory`, `Busy`, `Offline`, plus `EngineUnreadable` —
//!   see its own doc comment) that only later engine-supervisor tasks
//!   produce.
//! - The `system:local:*` event topics ([`TOPIC_STATE`], [`TOPIC_PROGRESS`],
//!   and the [`TOPIC_LATCH`] re-export) and [`publish_state`], the one place
//!   a [`LocalState`] transition reaches the browser.
//!
//! The five original variants (`DownloadFailed`, `ChecksumMismatch`,
//! `ArchiveCorrupt`, `SentinelMissing`, `NoSpace`) keep the exact wire
//! shapes Task 2.4 gave them — `install.rs` and its tests already bind to
//! them, and a serde test below pins `{"code":"no_space","needed":…}`.

use std::sync::Arc;

use crate::event_bus::{BusEvent, EventBus, PublisherIdentity};
use crate::local::hardware::Backend;

/// Something that stopped on-device inference from becoming ready.
#[derive(Debug, Clone, PartialEq, serde::Serialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum LocalError {
    #[error("download failed")]
    DownloadFailed { detail: String },
    #[error("checksum mismatch")]
    ChecksumMismatch,
    #[error("archive corrupt")]
    ArchiveCorrupt,
    #[error("engine files incomplete")]
    SentinelMissing { missing: String },
    #[error("backend unavailable")]
    BackendUnavailable,
    #[error("engine crashed")]
    EngineCrash { detail: String },
    #[error("engine failed its security self-check")]
    EngineInsecure { detail: String },
    #[error("engine files are damaged")]
    EngineCorrupt { detail: String },
    #[error("engine update required")]
    EngineUpdateRequired,
    #[error("model too large for this machine")]
    ModelTooLargeForMemory { reason: String },
    #[error("not enough disk space")]
    NoSpace { needed: u64, available: u64 },
    #[error("another NevoFlux instance is using on-device inference")]
    Busy,
    #[error("offline")]
    Offline,
    /// The landing place for `IntegrityError::Io` (`crate::local::integrity`):
    /// a cold-start manifest check that could not even complete — a
    /// permissions error, a failing disk, a directory that could not be
    /// listed — as distinct from a file that hashed to something wrong
    /// ([`LocalError::EngineCorrupt`]). Task 2.9's cold start maps
    /// `IntegrityError::Io` here rather than onto `EngineCorrupt`, because
    /// telling a user to reinstall does not fix — and may not even be
    /// reachable under — a plain I/O failure. `IntegrityError`'s
    /// `Missing`/`SizeMismatch`/`HashMismatch`/`Unexpected` variants still
    /// map to `EngineCorrupt`.
    #[error("engine files could not be read")]
    EngineUnreadable { detail: String },
}

/// The on-device inference state machine. Task 2.9's engine supervisor
/// drives it; Task 2.10's RPC surface serializes it to the browser via
/// [`publish_state`] on [`TOPIC_STATE`].
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LocalState {
    Idle,
    Probing,
    EstimatingFit,
    AwaitingConsent {
        engine_bytes: u64,
        model_bytes: u64,
        backend: Backend,
        sources: Vec<String>,
    },
    DownloadingEngine {
        done: u64,
        total: u64,
    },
    InstallingEngine,
    VerifyingEngine,
    DownloadingModel {
        done: u64,
        total: u64,
    },
    EngineStarting,
    Ready {
        backend: Backend,
        degraded: bool,
        reason: Option<String>,
        ctx: u32,
        gpu_layers: u32,
        layer_count: u32,
    },
    /// Idle-unloaded: the engine was installed and has run before, but is
    /// not currently resident. Cold-starts on demand.
    Stopped {
        backend: Backend,
    },
    Failed {
        error: LocalError,
        retryable: bool,
    },
}

/// Sticky. Payload: [`LocalState`]. The current on-device state, replayed
/// to any subscriber that joins after the transition — see [`publish_state`].
pub const TOPIC_STATE: &str = "system:local:state";

/// Ephemeral. In-flight progress (e.g. download byte counts) between two
/// [`LocalState`] transitions, published directly by whichever step is
/// running rather than through [`publish_state`] (which only carries the
/// coarser state machine itself).
pub const TOPIC_PROGRESS: &str = "system:local:progress";

/// Sticky `{on, paused_loops, paused_schedules, paused_goals}`. Re-exported
/// (not defined here — ruling R2) so callers that need the on-device topic
/// strings can reach all three from this module without also knowing that
/// `crate::local::latch` and `crate::local::sync` implement the latch's own
/// publish. The definition, with its full rationale, lives at
/// `crate::local::latch::TOPIC_LATCH`; `crate::local::sync` already
/// publishes to it in 11 places, so redefining the string here would fork
/// it.
pub use crate::local::latch::TOPIC_LATCH;

/// Publish `state` to [`TOPIC_STATE`] (sticky) on the process-global
/// EventBus (`crate::kb_wizard::CURRENT_EVENT_BUS`).
///
/// Synchronous by design — Task 2.9's supervisor calls this from many
/// branches of a state machine, not all of them already `async` — so the
/// actual publish (`EventBus::publish` is itself `async`) runs on a
/// spawned task, mirroring `crate::kb_wizard::make_emit_for_global`. A
/// missing EventBus (not yet bound at boot, or a unit test that never set
/// it) is a silent no-op rather than a panic, for the same reason
/// `crate::local::sync`'s publishes degrade gracefully — see that module's
/// docs.
pub fn publish_state(state: &LocalState) {
    let bus = crate::kb_wizard::CURRENT_EVENT_BUS.get().cloned();
    publish_state_with(bus, state);
}

/// Core of [`publish_state`], parameterized over the EventBus so tests can
/// inject their own instance instead of touching the process global (same
/// rationale as `crate::local::sync`'s `_with`-suffixed functions).
fn publish_state_with(bus: Option<Arc<EventBus>>, state: &LocalState) {
    let Some(bus) = bus else {
        tracing::debug!(?state, "publish_state: no EventBus bound yet, dropping");
        return;
    };
    let payload = match serde_json::to_value(state) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("failed to serialize LocalState for {TOPIC_STATE}: {e}");
            return;
        }
    };
    let event = BusEvent::sticky(TOPIC_STATE, payload, PublisherIdentity::Internal);
    // `EventBus::publish` is async; this function's signature must stay
    // sync (see the doc comment above), so hand the actual send to a
    // spawned task rather than block the caller on it.
    tokio::spawn(async move {
        if let Err(e) = bus.publish(event).await {
            tracing::warn!("failed to publish {TOPIC_STATE}: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_bus::{BackpressurePolicy, Delivery, SubscriberIdentity, TopicPattern};
    use std::time::Duration;

    #[test]
    fn serializes_with_a_code_tag_matching_task_2_8s_shape() {
        let v = serde_json::to_value(LocalError::NoSpace {
            needed: 10,
            available: 3,
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"code": "no_space", "needed": 10, "available": 3})
        );

        let v = serde_json::to_value(LocalError::ChecksumMismatch).unwrap();
        assert_eq!(v, serde_json::json!({"code": "checksum_mismatch"}));
    }

    /// The 5 variants Task 2.4 already shipped (and `install.rs` already
    /// depends on) must keep serializing exactly as before — ruling R54.
    #[test]
    fn the_five_pre_existing_local_error_variants_are_unchanged() {
        assert_eq!(
            serde_json::to_value(LocalError::DownloadFailed {
                detail: "timed out".into()
            })
            .unwrap(),
            serde_json::json!({"code": "download_failed", "detail": "timed out"})
        );
        assert_eq!(
            serde_json::to_value(LocalError::ChecksumMismatch).unwrap(),
            serde_json::json!({"code": "checksum_mismatch"})
        );
        assert_eq!(
            serde_json::to_value(LocalError::ArchiveCorrupt).unwrap(),
            serde_json::json!({"code": "archive_corrupt"})
        );
        assert_eq!(
            serde_json::to_value(LocalError::SentinelMissing {
                missing: "llama-server".into()
            })
            .unwrap(),
            serde_json::json!({"code": "sentinel_missing", "missing": "llama-server"})
        );
        assert_eq!(
            serde_json::to_value(LocalError::NoSpace {
                needed: 10,
                available: 3
            })
            .unwrap(),
            serde_json::json!({"code": "no_space", "needed": 10, "available": 3})
        );
    }

    #[test]
    fn engine_unreadable_serializes_to_its_specced_shape() {
        let v = serde_json::to_value(LocalError::EngineUnreadable {
            detail: "permission denied".into(),
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"code": "engine_unreadable", "detail": "permission denied"})
        );
    }

    /// Every new `LocalError` variant added by this task, not just the two
    /// pinned above — covers the full set the brief lists (fix any future
    /// rename here, not just the two shapes explicitly spec'd).
    #[test]
    fn remaining_new_local_error_variants_serialize_with_expected_shapes() {
        assert_eq!(
            serde_json::to_value(LocalError::BackendUnavailable).unwrap(),
            serde_json::json!({"code": "backend_unavailable"})
        );
        assert_eq!(
            serde_json::to_value(LocalError::EngineCrash {
                detail: "signal 11".into()
            })
            .unwrap(),
            serde_json::json!({"code": "engine_crash", "detail": "signal 11"})
        );
        assert_eq!(
            serde_json::to_value(LocalError::EngineInsecure {
                detail: "argv mismatch".into()
            })
            .unwrap(),
            serde_json::json!({"code": "engine_insecure", "detail": "argv mismatch"})
        );
        assert_eq!(
            serde_json::to_value(LocalError::EngineCorrupt {
                detail: "hash mismatch".into()
            })
            .unwrap(),
            serde_json::json!({"code": "engine_corrupt", "detail": "hash mismatch"})
        );
        assert_eq!(
            serde_json::to_value(LocalError::EngineUpdateRequired).unwrap(),
            serde_json::json!({"code": "engine_update_required"})
        );
        assert_eq!(
            serde_json::to_value(LocalError::ModelTooLargeForMemory {
                reason: "needs 12GB, have 8GB".into()
            })
            .unwrap(),
            serde_json::json!({"code": "model_too_large_for_memory", "reason": "needs 12GB, have 8GB"})
        );
        assert_eq!(
            serde_json::to_value(LocalError::Busy).unwrap(),
            serde_json::json!({"code": "busy"})
        );
        assert_eq!(
            serde_json::to_value(LocalError::Offline).unwrap(),
            serde_json::json!({"code": "offline"})
        );
    }

    #[test]
    fn local_state_ready_serializes_to_its_specced_shape() {
        let v = serde_json::to_value(LocalState::Ready {
            backend: Backend::Cuda,
            degraded: false,
            reason: None,
            ctx: 8192,
            gpu_layers: 32,
            layer_count: 32,
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "state": "ready",
                "backend": "cuda",
                "degraded": false,
                "reason": null,
                "ctx": 8192,
                "gpu_layers": 32,
                "layer_count": 32,
            })
        );
    }

    #[test]
    fn local_state_simple_variants_serialize_to_bare_state_tags() {
        assert_eq!(
            serde_json::to_value(LocalState::Idle).unwrap(),
            serde_json::json!({"state": "idle"})
        );
        assert_eq!(
            serde_json::to_value(LocalState::Probing).unwrap(),
            serde_json::json!({"state": "probing"})
        );
        assert_eq!(
            serde_json::to_value(LocalState::EstimatingFit).unwrap(),
            serde_json::json!({"state": "estimating_fit"})
        );
        assert_eq!(
            serde_json::to_value(LocalState::InstallingEngine).unwrap(),
            serde_json::json!({"state": "installing_engine"})
        );
        assert_eq!(
            serde_json::to_value(LocalState::VerifyingEngine).unwrap(),
            serde_json::json!({"state": "verifying_engine"})
        );
        assert_eq!(
            serde_json::to_value(LocalState::EngineStarting).unwrap(),
            serde_json::json!({"state": "engine_starting"})
        );
    }

    #[test]
    fn local_state_struct_variants_serialize_with_expected_shapes() {
        assert_eq!(
            serde_json::to_value(LocalState::AwaitingConsent {
                engine_bytes: 100,
                model_bytes: 200,
                backend: Backend::Vulkan,
                sources: vec!["huggingface".into(), "mirror".into()],
            })
            .unwrap(),
            serde_json::json!({
                "state": "awaiting_consent",
                "engine_bytes": 100,
                "model_bytes": 200,
                "backend": "vulkan",
                "sources": ["huggingface", "mirror"],
            })
        );
        assert_eq!(
            serde_json::to_value(LocalState::DownloadingEngine { done: 1, total: 2 }).unwrap(),
            serde_json::json!({"state": "downloading_engine", "done": 1, "total": 2})
        );
        assert_eq!(
            serde_json::to_value(LocalState::DownloadingModel { done: 3, total: 4 }).unwrap(),
            serde_json::json!({"state": "downloading_model", "done": 3, "total": 4})
        );
        assert_eq!(
            serde_json::to_value(LocalState::Stopped {
                backend: Backend::Cpu
            })
            .unwrap(),
            serde_json::json!({"state": "stopped", "backend": "cpu"})
        );
        assert_eq!(
            serde_json::to_value(LocalState::Failed {
                error: LocalError::Busy,
                retryable: true,
            })
            .unwrap(),
            serde_json::json!({
                "state": "failed",
                "error": {"code": "busy"},
                "retryable": true,
            })
        );
    }

    /// Every `system:local:*` topic string this module exposes must satisfy
    /// `system:<a>:<b>` — the extension's subscribe permission depends on
    /// the `system:` prefix (`crate::event_bus::permissions`), and
    /// `crate::event_bus::validate_topic` is the actual rule the bus itself
    /// enforces on every publish.
    #[test]
    fn topic_names_satisfy_system_a_b_and_pass_bus_validation() {
        for topic in [TOPIC_STATE, TOPIC_PROGRESS, TOPIC_LATCH] {
            crate::event_bus::validate_topic(topic)
                .unwrap_or_else(|e| panic!("{topic} fails bus topic validation: {e}"));
            let segments: Vec<&str> = topic.split(':').collect();
            assert_eq!(
                segments.len(),
                3,
                "{topic} must have exactly 3 segments (system:<a>:<b>)"
            );
            assert_eq!(segments[0], "system", "{topic} must start with `system:`");
        }
    }

    #[tokio::test]
    async fn publish_state_with_sends_a_sticky_event_carrying_the_state() {
        let bus = Arc::new(EventBus::new());
        let mut sub = bus
            .subscribe(
                TopicPattern::exact(TOPIC_STATE),
                SubscriberIdentity::Internal,
                BackpressurePolicy::DropNewest,
                8,
            )
            .expect("subscribe should succeed");

        let state = LocalState::Stopped {
            backend: Backend::Metal,
        };
        publish_state_with(Some(bus.clone()), &state);

        let evt = tokio::time::timeout(Duration::from_secs(2), sub.rx.recv())
            .await
            .expect("event within timeout")
            .expect("channel open");
        assert_eq!(evt.topic, TOPIC_STATE);
        assert_eq!(evt.delivery, Delivery::Sticky);
        assert_eq!(
            evt.payload,
            serde_json::json!({"state": "stopped", "backend": "metal"})
        );
    }

    #[tokio::test]
    async fn publish_state_with_is_a_no_op_without_a_bus() {
        // Must not panic when no bus is available.
        publish_state_with(None, &LocalState::Idle);
    }
}
