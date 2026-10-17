//! The daemon's one task engine, installed (M3 §2, §3.8, §5.1).
//!
//! Its own test binary: `remote::task::install` publishes a process-wide
//! engine that cannot be taken back, and the daemon's lib tests must keep
//! running without one.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use nevoflux_daemon::pending::{self, Pending};
use nevoflux_daemon::remote::agent_status::AgentStatus;
use nevoflux_daemon::remote::pairing::{Pairing, PairingKind, PairingStore};
use nevoflux_daemon::remote::start::{self, ControlDeps};
use nevoflux_daemon::remote::task::engine::{TaskEngine, TaskHost};
use nevoflux_daemon::wasm::services::{BrowserRequest, BrowserResponse};
use nevoflux_storage::Database;
use serde_json::Value;
use tokio::sync::mpsc;

fn agent(id: &str) -> Pairing {
    Pairing {
        control_channel_id: id.into(),
        data_channel_id: String::new(),
        control_key: "cc".repeat(32),
        data_key: String::new(),
        created_at: 2,
        label: None,
        push: None,
        kind: PairingKind::Agent,
        tier: None,
        paused: false,
        muse_may_approve_guarded: false,
    }
}

/// A sidebar that is always there; every channel is a live agent pairing.
struct Host {
    db: Arc<Database>,
    injected: Mutex<Vec<Value>>,
}

#[async_trait]
impl TaskHost for Host {
    fn sidebar(&self) -> Option<String> {
        Some("sidebar".into())
    }
    fn is_connected(&self, _proxy_id: &str) -> bool {
        true
    }
    fn database(&self) -> Arc<Database> {
        self.db.clone()
    }
    fn pairing(&self, channel: &str) -> Option<Pairing> {
        Some(agent(channel))
    }
    fn pending(&self) -> Arc<Pending> {
        pending::shared()
    }
    async fn send(&self, _proxy_id: &str, _payload: Value) {}
    async fn inject(&self, _proxy_id: &str, payload: Value) -> bool {
        self.injected.lock().unwrap().push(payload);
        true
    }
    async fn browser(&self, _request: BrowserRequest) -> Option<BrowserResponse> {
        None
    }
}

fn engine() -> Arc<TaskEngine> {
    static HOST: std::sync::OnceLock<Arc<Host>> = std::sync::OnceLock::new();
    let host = HOST.get_or_init(|| {
        Arc::new(Host {
            db: Arc::new(Database::open_in_memory().unwrap()),
            injected: Mutex::new(Vec::new()),
        })
    });
    nevoflux_daemon::remote::task::install(Arc::new(TaskEngine::new(
        host.clone(),
        Arc::new(AgentStatus::new()),
    )));
    nevoflux_daemon::remote::task::engine().expect("installed")
}

async fn start(engine: &TaskEngine, channel: &str) -> (String, String) {
    let v = engine
        .start(channel, "Generate a calculator Canvas", None)
        .await
        .expect("started");
    (
        v["task_id"].as_str().unwrap().to_string(),
        v["session"].as_str().unwrap().to_string(),
    )
}

async fn state(engine: &TaskEngine, channel: &str, task_id: &str) -> Value {
    engine
        .wait(channel, "mcp-1", task_id, Duration::ZERO, None)
        .await
        .expect("known")
        .state["state"]
        .clone()
}

fn deps(dir: &std::path::Path) -> ControlDeps {
    let database = Arc::new(Database::open_in_memory().unwrap());
    let (msg_tx, _rx) = mpsc::channel(8);
    ControlDeps {
        tracker: Arc::new(nevoflux_daemon::remote::runtime_state::RuntimeTracker::new()),
        sessions: Arc::new(
            nevoflux_daemon::remote::control_gateway::StorageSessions::new(database.clone()),
        ),
        pairings: Arc::new(PairingStore::new(dir.join("pairings.json"))),
        agent_pairings: Arc::new(PairingStore::new(dir.join("agent-pairings.json"))),
        database,
        vapid_public: None,
        msg_tx,
        injector_proxy_id: "remote-control".into(),
        registry: Arc::new(tokio::sync::Mutex::new(
            nevoflux_daemon::remote::gateway::GatewayRegistry::new(),
        )),
        agent_status: Arc::new(AgentStatus::new()),
    }
}

#[tokio::test]
async fn a_question_from_a_delegated_session_is_held_thirty_minutes() {
    let engine = engine();
    let (task_id, session) = start(&engine, "hold-chan").await;
    assert_eq!(pending::hold_for(&session), Duration::from_secs(1800));
    assert_eq!(
        pending::hold_for("a-session-of-the-persons-own"),
        Duration::from_secs(600)
    );
    engine.cancel(Some("hold-chan"), &task_id).await.unwrap();
    assert_eq!(
        pending::hold_for(&session),
        Duration::from_secs(600),
        "the task is over"
    );
}

#[tokio::test]
async fn pausing_an_agent_cancels_its_task() {
    let engine = engine();
    let dir = tempfile::tempdir().unwrap();
    let deps = deps(dir.path());
    deps.agent_pairings.add(agent("pause-chan")).unwrap();
    let (task_id, _) = start(&engine, "pause-chan").await;
    start::pause_agent(&deps, "pause-chan").await.unwrap();
    assert_eq!(state(&engine, "pause-chan", &task_id).await, "cancelled");
}

#[tokio::test]
async fn unpairing_an_agent_cancels_its_task() {
    let engine = engine();
    let dir = tempfile::tempdir().unwrap();
    let deps = deps(dir.path());
    deps.agent_pairings.add(agent("unpair-chan")).unwrap();
    let (task_id, _) = start(&engine, "unpair-chan").await;
    assert!(start::unpair_device(&deps, "unpair-chan").await);
    assert_eq!(state(&engine, "unpair-chan", &task_id).await, "cancelled");
}
