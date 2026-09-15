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

use std::sync::OnceLock;

use serde_json::json;

use nevoflux_storage::connection::Database;

use crate::config::AgentConfig;
use crate::event_bus::{BusEvent, EventBus, PublisherIdentity};
use crate::local::endpoint;
use crate::local::latch::{self, TOPIC_LATCH};

/// Shared DB handle [`paused_counts`] queries for the counts published on
/// a latch transition. Set once at boot (`server.rs`).
pub(crate) static CURRENT_DB: OnceLock<Database> = OnceLock::new();

/// React to a config change: refresh the latch, re-apply the gateway's
/// upstream, and — only on an actual latch transition — publish
/// `system:local:latch_changed`.
///
/// Called after every `*shared_config.write() = Arc::new(config)` in
/// `server.rs`'s four `config.llm.*` handlers, and once at boot (after the
/// gateway starts, before the loop/schedule/goal managers are built, so a
/// latched boot never lets unattended work run against the cloud first).
pub async fn on_config_changed(cfg: &AgentConfig) {
    let changed = latch::refresh_from_config(cfg);

    // Re-resolve — not just re-read the boot snapshot — on every call: the
    // active cloud provider may have changed since the gateway booted,
    // even on a call where the latch itself doesn't flip.
    let cloud = crate::llm_gateway::resolve_upstream_config(&cfg.knowledge_base.gateway, cfg);
    crate::llm_gateway::set_cloud_upstream(cloud);

    apply_gateway_upstream_for_latch().await;

    if let Some(on) = changed {
        let (paused_loops, paused_schedules, paused_goals) =
            CURRENT_DB.get().map(paused_counts).unwrap_or((0, 0, 0));
        let bus = crate::kb_wizard::CURRENT_EVENT_BUS
            .get()
            .map(|b| b.as_ref());
        publish_latch_changed(bus, on, paused_loops, paused_schedules, paused_goals).await;
    }
}

/// Re-apply the gateway's upstream for the current latch state: the
/// on-device endpoint (or the closed loopback, if none is published yet)
/// while latched, or the last-resolved cloud upstream while not.
///
/// Exposed standalone — not just reachable via [`on_config_changed`] — so
/// the on-device engine supervisor (Task 2.9) can call it directly when a
/// local endpoint is published or cleared; that's an engine lifecycle
/// event, not a config change, so it has no `AgentConfig` in hand.
pub async fn apply_gateway_upstream_for_latch() {
    let Some(control) = crate::llm_gateway::gateway_control() else {
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
    // on_config_changed / apply_gateway_upstream_for_latch — full wiring.
    //
    // This is the ONLY test in the crate that calls
    // `llm_gateway::set_gateway_control` / `CURRENT_DB.set` — both are
    // `OnceLock`s new to Task 1.6 and touched nowhere else, so there is no
    // other test to race for "first setter wins". It also touches the
    // REAL global LocalOnly latch (via `on_config_changed` ->
    // `refresh_from_config`, which always bypasses the thread-local test
    // override — see `latch`'s module docs), so it holds `test_serial()`
    // for its entire duration like that module's own `refresh_tracks_*`
    // tests. `CURRENT_EVENT_BUS` may already be set by an unrelated test
    // elsewhere in this binary; that's fine — this test subscribes to
    // whichever bus instance is globally registered and filters on the
    // latch topic, which nothing else in the crate ever publishes to.
    #[tokio::test]
    async fn on_config_changed_wires_gateway_upstream_and_publishes_on_transitions() {
        let _guard = latch::test_serial();
        latch::set_global_for_test(false);

        let handle = nevoflux_llm_gateway::serve(test_gateway_config())
            .await
            .expect("gateway should start");
        crate::llm_gateway::set_gateway_control(handle.control());

        let db = Database::open_in_memory().expect("in-memory db");
        seed_rows(&db);
        let _ = CURRENT_DB.set(db);

        let bus = crate::kb_wizard::CURRENT_EVENT_BUS
            .get_or_init(|| std::sync::Arc::new(EventBus::new()))
            .clone();
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
        on_config_changed(&cfg).await;

        assert!(latch::is_on(), "latch must be on after switching to local");
        // No engine published yet -> the closed loopback.
        let snap = handle.upstream_snapshot().await;
        assert_eq!(snap.base_url, crate::llm_gateway::CLOSED_LOOPBACK_UPSTREAM);

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
        on_config_changed(&cfg).await;

        assert!(
            !latch::is_on(),
            "latch must be off after switching to anthropic"
        );
        let snap2 = handle.upstream_snapshot().await;
        assert_eq!(snap2.base_url, "https://cloud.example");
        assert_eq!(snap2.api_key, "cloud-key");
        assert_eq!(snap2.protocol, UpstreamProtocol::Anthropic);

        let evt2 = tokio::time::timeout(Duration::from_secs(2), sub.rx.recv())
            .await
            .expect("second latch_changed event within timeout")
            .expect("channel open");
        assert_eq!(evt2.payload["on"], json!(false));

        // --- a config change that does NOT flip the latch must not republish. ---
        cfg.llm.anthropic.model = Some("claude-updated".into());
        on_config_changed(&cfg).await;
        let no_third = tokio::time::timeout(Duration::from_millis(200), sub.rx.recv()).await;
        assert!(
            no_third.is_err(),
            "no latch_changed event when the latch didn't change"
        );
        // The gateway still tracks the live config even without a latch
        // flip (Task 1.6's "re-resolve on every call" requirement).
        let snap3 = handle.upstream_snapshot().await;
        assert_eq!(snap3.model_override, "claude-updated");

        latch::set_global_for_test(false);
        handle.shutdown().await;
    }

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
}
