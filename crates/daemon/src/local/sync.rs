//! Config-change hook for the LocalOnly latch (Task 1.6).
//!
//! [`on_config_changed`] is the one place a config change (`config.llm.set`,
//! `config.llm.custom.{create,update,delete}`, and the daemon's own boot)
//! funnels through to:
//!
//! 1. [`crate::local::latch::refresh_from_config`] — recompute the latch
//!    from the new config.
//! 2. Re-apply the gateway's upstream for the (possibly unchanged) latch
//!    state — see [`apply_gateway_upstream_for_latch`]. This always runs,
//!    even when the latch itself didn't flip: the active *cloud* provider
//!    can change while the latch is off (or while latched, ready for when
//!    it next unlatches), so the cached cloud upstream
//!    (`crate::llm_gateway::set_cloud_upstream`) is re-resolved from `cfg`
//!    on every call, not just the boot-time snapshot.
//! 3. If the latch *changed*, publish a sticky `system:local:latch_changed`
//!    broadcast carrying how much unattended work (loops/schedules/goals)
//!    just got paused or resumed.
//!
//! Every step degrades gracefully instead of panicking: no gateway yet
//! (failed to bind at boot) → the upstream re-apply is a no-op; no DB
//! handle published yet → zero counts; no event bus yet → skip the
//! publish. None of these should happen in production — `server.rs`
//! publishes all three (`llm_gateway::set_gateway_control`, [`CURRENT_DB`],
//! `kb_wizard::CURRENT_EVENT_BUS`) before the first call — but a caller
//! that hasn't wired them (unit tests, an early crash path) must not bring
//! the config-change handler down with it.
//!
//! ## Concurrency (fix round 1, item 2)
//!
//! [`SYNC_MUTEX`] serializes "read the latch, decide the upstream, write
//! it" as one section across every caller — [`on_config_changed`] (which
//! also writes the latch, via `refresh_from_config`) and
//! [`apply_gateway_upstream_for_latch`] both take it before touching
//! `latch::is_on()`. Without this, two nearly-simultaneous calls (e.g. two
//! `config.llm.*` RPCs) could interleave: call A reads the latch as ON,
//! call B flips it OFF and writes the cloud upstream, then A's stale
//! local/unavailable write lands *after* B's — leaving a gateway pointed
//! at the wrong upstream for the now-current latch state. With the mutex,
//! whichever call is logically last to run the whole section always wins,
//! and its write matches what it read.
//!
//! ## Testability (fix round 1, item 3)
//!
//! The public, no-argument [`on_config_changed`] / [`apply_gateway_upstream_for_latch`]
//! read real process globals (`crate::llm_gateway::gateway_control`,
//! [`CURRENT_DB`], `crate::kb_wizard::CURRENT_EVENT_BUS`) — but `server.rs`'s
//! own `start_server` (exercised by `server::tests::test_server_start_and_shutdown`)
//! ALSO sets those same globals during boot, so a test can't assume it's
//! the exclusive setter. Both public functions are thin wrappers around
//! `_with`-suffixed inner functions that take every dependency as an
//! explicit parameter; tests call those directly with their own
//! `GatewayControl` / `Database` / `EventBus` instances and never touch
//! the process globals at all, so they're immune to what any other test
//! in the binary does with them, run order included.

use std::sync::OnceLock;

use serde_json::json;

use nevoflux_llm_gateway::GatewayControl;
use nevoflux_storage::connection::Database;

use crate::config::AgentConfig;
use crate::event_bus::{BusEvent, EventBus, PublisherIdentity};
use crate::local::endpoint;
use crate::local::latch::{self, TOPIC_LATCH};

/// Shared DB handle [`paused_counts`] queries for the counts published on
/// a latch transition. Set once at boot (`server.rs`).
pub(crate) static CURRENT_DB: OnceLock<Database> = OnceLock::new();

/// Serializes "read the latch, decide the upstream, apply it" across
/// every caller of [`on_config_changed`] / [`apply_gateway_upstream_for_latch`]
/// — see the module docs' Concurrency section (fix round 1, item 2).
static SYNC_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// React to a config change: refresh the latch, re-apply the gateway's
/// upstream, and — only on an actual latch transition — publish
/// `system:local:latch_changed`.
///
/// Called after every `*shared_config.write() = Arc::new(config)` in
/// `server.rs`'s four `config.llm.*` handlers, and once at boot (right
/// after the gateway starts and its control handle is published, before
/// gbrain is spawned or the loop/schedule/goal managers are built, so a
/// latched boot never lets unattended work run against the cloud first —
/// fix round 1, item 6).
///
/// Reads its dependencies from process globals and delegates to
/// [`on_config_changed_with`] — see the module docs' Testability section.
pub async fn on_config_changed(cfg: &AgentConfig) {
    let control = crate::llm_gateway::gateway_control();
    let db = CURRENT_DB.get().cloned();
    let bus = crate::kb_wizard::CURRENT_EVENT_BUS.get().cloned();
    on_config_changed_with(cfg, control.as_ref(), db.as_ref(), bus.as_deref()).await;
}

/// Core of [`on_config_changed`], parameterized over its dependencies so
/// tests can inject their own instances instead of touching process
/// globals (fix round 1, item 3).
async fn on_config_changed_with(
    cfg: &AgentConfig,
    control: Option<&GatewayControl>,
    db: Option<&Database>,
    bus: Option<&EventBus>,
) {
    let changed = {
        let _guard = SYNC_MUTEX.lock().await;
        let changed = latch::refresh_from_config(cfg);
        // Re-resolve — not just re-read the boot snapshot — on every
        // call: the active cloud provider may have changed since the
        // gateway booted, even on a call where the latch itself doesn't
        // flip.
        let cloud = crate::llm_gateway::resolve_upstream_config(&cfg.knowledge_base.gateway, cfg);
        crate::llm_gateway::set_cloud_upstream(cloud);
        apply_gateway_upstream_for_latch_locked(control).await;
        changed
        // `_guard` drops here — publishing below doesn't need the lock.
    };

    if let Some(on) = changed {
        let (paused_loops, paused_schedules, paused_goals) =
            db.map(paused_counts).unwrap_or((0, 0, 0));
        publish_latch_changed(bus, on, paused_loops, paused_schedules, paused_goals).await;
    }
}

/// Re-apply the gateway's upstream for the current latch state: the
/// on-device endpoint (or `unavailable`, if none is published yet) while
/// latched, or the last-resolved cloud upstream while not.
///
/// Exposed standalone — not just reachable via [`on_config_changed`] — so
/// the on-device engine supervisor (Task 2.9) can call it directly when a
/// local endpoint is published or cleared; that's an engine lifecycle
/// event, not a config change, so it has no `AgentConfig` in hand.
///
/// Reads its dependency from the process global and delegates to
/// [`apply_gateway_upstream_for_latch_locked`], taking [`SYNC_MUTEX`]
/// itself first (unlike that inner function, which assumes the caller —
/// here, or [`on_config_changed_with`] — already holds it).
pub async fn apply_gateway_upstream_for_latch() {
    let control = crate::llm_gateway::gateway_control();
    let _guard = SYNC_MUTEX.lock().await;
    apply_gateway_upstream_for_latch_locked(control.as_ref()).await;
}

/// Core of [`apply_gateway_upstream_for_latch`] — assumes [`SYNC_MUTEX`]
/// is already held by the caller. Reads `latch::is_on()` and writes the
/// resulting upstream as one atomic step from the mutex's point of view,
/// closing the read-latch/await-write race described in the module docs'
/// Concurrency section.
async fn apply_gateway_upstream_for_latch_locked(control: Option<&GatewayControl>) {
    let Some(control) = control else {
        // No gateway (it failed to bind at boot, or this ran before boot
        // wired the global) — nothing to point anywhere.
        return;
    };
    let latched = latch::is_on();
    let up = if latched {
        crate::llm_gateway::upstream_for_local(endpoint::current().as_ref())
    } else {
        match crate::llm_gateway::cloud_upstream() {
            Some(cloud) => crate::llm_gateway::upstream_for_cloud(&cloud),
            // Nothing resolved yet (called before the first
            // on_config_changed ever ran) — leave the gateway's current
            // upstream (its boot-time GatewayConfig) alone rather than
            // guessing at a cloud upstream we have no basis for.
            None => return,
        }
    };
    control.set_upstream(up).await;
}

/// How much unattended work is currently active: loops not yet terminal,
/// schedules still armed, and goals both active and NOT driven by a
/// deterministic check (`check_json IS NULL` — a checked goal isn't
/// "paused" by the latch the same way, since W5-style verify goals don't
/// themselves make LLM calls between checks). Zero on any DB error —
/// best-effort, a broken count must not block the latch broadcast.
fn paused_counts(db: &Database) -> (i64, i64, i64) {
    let loops = count(
        db,
        "SELECT COUNT(*) FROM loops WHERE state NOT IN ('cancelled','failed')",
    );
    let schedules = count(db, "SELECT COUNT(*) FROM schedules WHERE status = 'active'");
    let goals = count(
        db,
        "SELECT COUNT(*) FROM goals WHERE status = 'active' AND check_json IS NULL",
    );
    (loops, schedules, goals)
}

fn count(db: &Database, sql: &str) -> i64 {
    db.with_connection(|c| {
        c.query_row(sql, [], |r| r.get(0))
            .map_err(nevoflux_storage::error::StorageError::from)
    })
    .unwrap_or(0)
}

/// Publish `system:local:latch_changed` (sticky) if `bus` is available. A
/// missing bus is a silent no-op — see the module docs.
async fn publish_latch_changed(
    bus: Option<&EventBus>,
    on: bool,
    paused_loops: i64,
    paused_schedules: i64,
    paused_goals: i64,
) {
    let Some(bus) = bus else { return };
    let payload = json!({
        "on": on,
        "paused_loops": paused_loops,
        "paused_schedules": paused_schedules,
        "paused_goals": paused_goals,
    });
    let event = BusEvent::sticky(TOPIC_LATCH, payload, PublisherIdentity::Internal);
    if let Err(e) = bus.publish(event).await {
        tracing::warn!("failed to publish {TOPIC_LATCH}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_bus::{BackpressurePolicy, Delivery, SubscriberIdentity, TopicPattern};
    use crate::llm_gateway::tests::{clear_resolver_env, ENV_MUTEX};
    use nevoflux_llm_gateway::{GatewayConfig, UpstreamProtocol};
    use std::time::Duration;

    // ------------------------------------------------------------------
    // paused_counts — real in-memory DB, no process globals touched.
    // ------------------------------------------------------------------

    fn seed_rows(db: &Database) {
        db.with_connection(|c| {
            c.execute_batch(
                "INSERT INTO sessions (id, created_at, updated_at) VALUES ('sess-1', 0, 0);
                 INSERT INTO sessions (id, created_at, updated_at) VALUES ('sess-2', 0, 0);

                 INSERT INTO loops
                     (id, session_id, trigger_expr, prompt_text, state, created_at, updated_at)
                     VALUES ('loop-active', 'sess-1', 'every 1h', 'do it', 'active', 0, 0);
                 INSERT INTO loops
                     (id, session_id, trigger_expr, prompt_text, state, created_at, updated_at)
                     VALUES ('loop-cancelled', 'sess-1', 'every 1h', 'do it', 'cancelled', 0, 0);
                 INSERT INTO loops
                     (id, session_id, trigger_expr, prompt_text, state, created_at, updated_at)
                     VALUES ('loop-failed', 'sess-1', 'every 1h', 'do it', 'failed', 0, 0);

                 INSERT INTO schedules (id, name, cron_expr, prompt_text, status, created_at, updated_at)
                     VALUES ('sched-active', 'nightly', '* * * * *', 'do it', 'active', 0, 0);
                 INSERT INTO schedules (id, name, cron_expr, prompt_text, status, created_at, updated_at)
                     VALUES ('sched-paused', 'nightly-2', '* * * * *', 'do it', 'paused', 0, 0);
                 INSERT INTO schedules (id, name, cron_expr, prompt_text, status, created_at, updated_at)
                     VALUES ('sched-cancelled', 'nightly-3', '* * * * *', 'do it', 'cancelled', 0, 0);

                 INSERT INTO goals (id, session_id, condition, status, created_at, updated_at)
                     VALUES ('goal-active-unchecked', 'sess-1', 'done', 'active', 0, 0);
                 INSERT INTO goals (id, session_id, condition, status, check_json, created_at, updated_at)
                     VALUES ('goal-active-checked', 'sess-2', 'done too', 'active', '{}', 0, 0);",
            )
            .map_err(nevoflux_storage::error::StorageError::from)
        })
        .expect("seed rows");
    }

    #[test]
    fn paused_counts_is_zero_on_empty_db() {
        let db = Database::open_in_memory().expect("in-memory db");
        assert_eq!(paused_counts(&db), (0, 0, 0));
    }

    #[test]
    fn paused_counts_counts_only_active_relevant_rows() {
        let db = Database::open_in_memory().expect("in-memory db");
        seed_rows(&db);
        // 1 active loop (cancelled/failed excluded), 1 active schedule
        // (paused/cancelled excluded), 1 active+unchecked goal (the
        // checked one is excluded by `check_json IS NULL`).
        assert_eq!(paused_counts(&db), (1, 1, 1));
    }

    // ------------------------------------------------------------------
    // publish_latch_changed — local EventBus, no process globals touched.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn publish_latch_changed_sends_sticky_event_with_expected_payload() {
        let bus = EventBus::new();
        let mut sub = bus
            .subscribe(
                TopicPattern::exact(TOPIC_LATCH),
                SubscriberIdentity::Internal,
                BackpressurePolicy::DropNewest,
                8,
            )
            .expect("subscribe should succeed");

        publish_latch_changed(Some(&bus), true, 2, 1, 3).await;

        let evt = tokio::time::timeout(Duration::from_secs(2), sub.rx.recv())
            .await
            .expect("event within timeout")
            .expect("channel open");
        assert_eq!(evt.topic, TOPIC_LATCH);
        assert_eq!(evt.delivery, Delivery::Sticky);
        assert_eq!(evt.payload["on"], json!(true));
        assert_eq!(evt.payload["paused_loops"], json!(2));
        assert_eq!(evt.payload["paused_schedules"], json!(1));
        assert_eq!(evt.payload["paused_goals"], json!(3));
    }

    #[tokio::test]
    async fn publish_latch_changed_is_a_no_op_without_a_bus() {
        // Must not panic when no bus is available (module docs' "publish
        // without failing" contract).
        publish_latch_changed(None, true, 0, 0, 0).await;
    }

    // ------------------------------------------------------------------
    // on_config_changed_with / apply_gateway_upstream_for_latch_locked —
    // full logic, but via dependency injection: every test here builds
    // its OWN GatewayControl / Database / EventBus and never touches a
    // process global, so none of them can race `start_server`'s boot
    // (which sets the same globals) or each other, regardless of run
    // order (fix round 1, item 3). They still touch the REAL global
    // LocalOnly latch (via `refresh_from_config`, which always bypasses
    // the thread-local test override — see `latch`'s module docs) and so
    // hold `test_serial()` for their duration, same as that module's own
    // `refresh_tracks_*` tests.
    //
    // Config resolution reads env-var fallbacks
    // (`NEVOFLUX_LLM_GATEWAY_UPSTREAM_*`), so every test here also holds
    // `llm_gateway::tests::ENV_MUTEX` — shared with that module's own env
    // var tests — for its duration.
    // ------------------------------------------------------------------

    fn test_gateway_config() -> GatewayConfig {
        GatewayConfig {
            bind_addr: "127.0.0.1:0".parse().expect("loopback addr"),
            bearer_token: "on-config-changed-test-token".into(),
            upstream_base_url: "https://boot.example".into(),
            upstream_api_key: String::new(),
            upstream_model_remap: None,
            anthropic_version: nevoflux_llm_gateway::DEFAULT_ANTHROPIC_VERSION.to_string(),
            upstream_request_timeout: nevoflux_llm_gateway::DEFAULT_UPSTREAM_REQUEST_TIMEOUT,
            upstream_connect_timeout: nevoflux_llm_gateway::DEFAULT_UPSTREAM_CONNECT_TIMEOUT,
            upstream_stream_idle_timeout:
                nevoflux_llm_gateway::DEFAULT_UPSTREAM_STREAM_IDLE_TIMEOUT,
            upstream_retry_max_wait: nevoflux_llm_gateway::DEFAULT_UPSTREAM_RETRY_MAX_WAIT,
            advertised_models: vec![],
            upstream_protocol: UpstreamProtocol::Anthropic,
            acp_config: None,
        }
    }

    #[tokio::test]
    async fn on_config_changed_with_wires_gateway_upstream_and_publishes_on_transitions() {
        let _latch_guard = latch::test_serial();
        let _env_guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        clear_resolver_env();
        latch::set_global_for_test(false);

        let handle = nevoflux_llm_gateway::serve(test_gateway_config())
            .await
            .expect("gateway should start");
        let control = handle.control();

        let db = Database::open_in_memory().expect("in-memory db");
        seed_rows(&db);

        let bus = EventBus::new();
        let mut sub = bus
            .subscribe(
                TopicPattern::exact(TOPIC_LATCH),
                SubscriberIdentity::Internal,
                BackpressurePolicy::DropNewest,
                8,
            )
            .expect("subscribe should succeed");

        // --- latch OFF -> ON: switch to the local provider. ---
        let mut cfg = AgentConfig::default();
        cfg.llm.provider = Some("local".into());
        cfg.llm.local.enabled = true;
        on_config_changed_with(&cfg, Some(&control), Some(&db), Some(&bus)).await;

        assert!(latch::is_on(), "latch must be on after switching to local");
        // No engine published yet -> unavailable (fix round 1, item 5).
        let snap = control.upstream_snapshot().await;
        assert!(snap.unavailable);

        let evt = tokio::time::timeout(Duration::from_secs(2), sub.rx.recv())
            .await
            .expect("latch_changed event within timeout")
            .expect("channel open");
        assert_eq!(evt.topic, TOPIC_LATCH);
        assert_eq!(evt.payload["on"], json!(true));
        assert_eq!(evt.payload["paused_loops"], json!(1));
        assert_eq!(evt.payload["paused_schedules"], json!(1));
        assert_eq!(evt.payload["paused_goals"], json!(1));

        // --- latch ON -> OFF: switch back to a cloud provider. ---
        cfg.llm.provider = Some("anthropic".into());
        cfg.llm.anthropic.api_key = Some("cloud-key".into());
        cfg.llm.anthropic.base_url = Some("https://cloud.example".into());
        on_config_changed_with(&cfg, Some(&control), Some(&db), Some(&bus)).await;

        assert!(
            !latch::is_on(),
            "latch must be off after switching to anthropic"
        );
        let snap2 = control.upstream_snapshot().await;
        assert_eq!(snap2.base_url, "https://cloud.example");
        assert_eq!(snap2.api_key, "cloud-key");
        assert_eq!(snap2.protocol, UpstreamProtocol::Anthropic);
        assert!(!snap2.unavailable);

        let evt2 = tokio::time::timeout(Duration::from_secs(2), sub.rx.recv())
            .await
            .expect("second latch_changed event within timeout")
            .expect("channel open");
        assert_eq!(evt2.payload["on"], json!(false));

        // --- a config change that does NOT flip the latch must not republish. ---
        cfg.llm.anthropic.model = Some("claude-updated".into());
        on_config_changed_with(&cfg, Some(&control), Some(&db), Some(&bus)).await;
        let no_third = tokio::time::timeout(Duration::from_millis(200), sub.rx.recv()).await;
        assert!(
            no_third.is_err(),
            "no latch_changed event when the latch didn't change"
        );
        // The gateway still tracks the live config even without a latch
        // flip (Task 1.6's "re-resolve on every call" requirement).
        let snap3 = control.upstream_snapshot().await;
        assert_eq!(snap3.model_override, "claude-updated");

        latch::set_global_for_test(false);
        clear_resolver_env();
        handle.shutdown().await;
    }

    /// Fix round 1, item 2: fire many `on_config_changed_with` calls that
    /// alternate between latching on (local) and off (cloud) concurrently.
    /// Whatever the LAST one to actually complete its `SYNC_MUTEX`
    /// section decided the latch to be, the gateway's upstream must match
    /// it — never a stale write from an earlier call landing after a
    /// later one already moved the latch on. Entirely dependency-injected
    /// (own `GatewayControl`), so this can run alongside every other test
    /// in the binary — only the real global latch is shared, hence
    /// `test_serial()`.
    #[tokio::test]
    async fn concurrent_alternating_latch_toggles_leave_upstream_matching_final_latch_state() {
        let _latch_guard = latch::test_serial();
        let _env_guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        clear_resolver_env();
        latch::set_global_for_test(false);

        let handle = nevoflux_llm_gateway::serve(test_gateway_config())
            .await
            .expect("gateway should start");
        let control = handle.control();

        let mut local_cfg = AgentConfig::default();
        local_cfg.llm.provider = Some("local".into());
        local_cfg.llm.local.enabled = true;

        let mut cloud_cfg = AgentConfig::default();
        cloud_cfg.llm.provider = Some("anthropic".into());
        cloud_cfg.llm.anthropic.api_key = Some("cloud-key".into());
        cloud_cfg.llm.anthropic.base_url = Some("https://cloud.example".into());

        let mut tasks = Vec::new();
        for i in 0..40u32 {
            let cfg = if i % 2 == 0 {
                local_cfg.clone()
            } else {
                cloud_cfg.clone()
            };
            let control = control.clone();
            tasks.push(tokio::spawn(async move {
                on_config_changed_with(&cfg, Some(&control), None, None).await;
            }));
        }
        for t in tasks {
            t.await.expect("task should not panic");
        }

        // Whatever the latch ended up as, the upstream must agree with it
        // — this is the property that would break under the pre-fix race.
        let final_latched = latch::is_on();
        let snap = control.upstream_snapshot().await;
        if final_latched {
            assert!(
                snap.unavailable,
                "latched with no endpoint published must leave the upstream unavailable"
            );
        } else {
            assert_eq!(snap.base_url, "https://cloud.example");
            assert_eq!(snap.protocol, UpstreamProtocol::Anthropic);
            assert!(!snap.unavailable);
        }

        latch::set_global_for_test(false);
        clear_resolver_env();
        handle.shutdown().await;
    }
}
