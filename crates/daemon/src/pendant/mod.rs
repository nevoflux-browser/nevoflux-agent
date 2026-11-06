//! Provisioning a NevoFlux pendant from `nevoflux://settings`, so nobody has
//! to run `provision.py` (`docs/plans/nevoflux-pendant-settings-provisioning.md`
//! in the pendant repo).
//!
//! The browser cannot open a serial port (no WebSerial in Firefox); this head
//! runs on the computer the pendant is plugged into, and is the other end of
//! the pairing anyway — so it mints the pairing itself, asks nevoflux.app for
//! the pendant's own token (one Device Grant the person approves), and writes
//! both down the USB console.
//!
//! - [`serial`] the console protocol and the port
//! - [`wifi`] the computer's Wi-Fi, to fill the form in
//! - [`job`] one pairing or Wi-Fi change, step by step
//!
//! Commands (`pendant.*`): `ports`, `wifi`, `pair`, `set_wifi`, `cancel`,
//! `list`, `unpair`. A job's progress is the sticky EventBus topic
//! `system:pendant:job`.

pub mod job;
pub mod serial;
pub mod wifi;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::event_bus::{BusEvent, PublisherIdentity};
use crate::kb_wizard::{err_response, ok_response};
use crate::remote::account::{DeviceCodeResp, PollOutcome};
use job::{Cancel, Env, Minted, Task};
use serial::{Line, ProvError};

pub const TOPIC_JOB: &str = "system:pendant:job";
/// The Device Grant client a pendant's token is issued to (as provision.py does).
const CLIENT_ID: &str = "nevoflux-daemon";

// ── Which pairings are pendants ───────────────────────────────

/// `<data dir>/pendants.json`: the pairings made here for a pendant, by
/// control channel id. A pairing record does not say what it pairs with;
/// this does, so the settings page lists pendants and not phones.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Known {
    #[serde(default)]
    pendants: BTreeMap<String, KnownPendant>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KnownPendant {
    name: String,
    paired_at: i64,
}

fn known_path() -> PathBuf {
    crate::paths::resolve_from_daemon().data_dir.join("pendants.json")
}

fn load_known() -> Known {
    std::fs::read(known_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_known(k: &Known) {
    let p = known_path();
    let tmp = p.with_extension("json.tmp");
    let ok = serde_json::to_vec_pretty(k)
        .ok()
        .and_then(|b| std::fs::write(&tmp, b).ok())
        .and_then(|_| std::fs::rename(&tmp, &p).ok());
    if ok.is_none() {
        tracing::warn!(target: "pendant", "could not write {}", p.display());
    }
}

// ── The real world ────────────────────────────────────────────

fn account_base() -> String {
    std::env::var("NEVOFLUX_ACCOUNT_URL").unwrap_or_else(|_| "https://nevoflux.app".to_string())
}

struct RealEnv;

#[async_trait]
impl Env for RealEnv {
    fn open(&self, port: &str) -> Result<Box<dyn Line>, ProvError> {
        Ok(Box::new(serial::SerialLine::open(port)?))
    }

    async fn device_code(&self) -> Result<DeviceCodeResp, String> {
        crate::remote::account::request_device_code(&account_base(), CLIENT_ID)
            .await
            .map_err(|e| format!("could not reach nevoflux.app: {e}"))
    }

    async fn poll(&self, device_code: &str) -> Result<PollOutcome, String> {
        crate::remote::account::poll_device_token(&account_base(), CLIENT_ID, device_code)
            .await
            .map_err(|e| e.to_string())
    }

    async fn pair(&self, label: &str) -> Result<Minted, String> {
        let deps = crate::remote::start::control_deps().ok_or("the agent is still starting")?;
        let (p, _code) = crate::remote::start::pair_device(deps).await.map_err(|e| match e {
            crate::remote::start::OpenError::NotLoggedIn => {
                "this computer is not signed in to NevoFlux; sign in first, then pair again".to_string()
            }
            e => e.to_string(),
        })?;
        if let Err(e) = deps.pairings.set_label(&p.control_channel_id, label) {
            tracing::warn!(target: "pendant", "could not name the pairing: {e}");
        }
        let mut k = load_known();
        k.pendants.insert(
            p.control_channel_id.clone(),
            KnownPendant { name: label.to_string(), paired_at: p.created_at },
        );
        save_known(&k);
        Ok(Minted {
            control_channel_id: p.control_channel_id,
            data_channel_id: p.data_channel_id,
            control_key_hex: p.control_key,
            data_key_hex: p.data_key,
        })
    }

    async fn unpair(&self, control_channel_id: &str) {
        forget(control_channel_id).await;
    }

    fn holds(&self, control_channel_id: &str) -> bool {
        crate::remote::start::control_deps()
            .map(|d| d.pairings.load().unwrap_or_default().iter().any(|p| p.control_channel_id == control_channel_id))
            .unwrap_or(false)
    }

    fn link(&self, data_channel_id: &str) -> Option<&'static str> {
        crate::remote::presence::link(data_channel_id).map(|l| l.as_str())
    }

    fn relay_url(&self) -> String {
        crate::remote::start::relay_base()
    }

    fn host(&self) -> String {
        hostname()
    }

    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "NevoFlux-PC".to_string())
}

async fn forget(control_channel_id: &str) {
    if let Some(deps) = crate::remote::start::control_deps() {
        crate::remote::start::unpair_device(deps, control_channel_id).await;
    }
    crate::remote::presence::forget(control_channel_id);
    let mut k = load_known();
    if k.pendants.remove(control_channel_id).is_some() {
        save_known(&k);
    }
}

// ── The one job at a time ─────────────────────────────────────

struct Running {
    id: String,
    port: String,
    cancel: Arc<Cancel>,
}

fn running() -> &'static Mutex<Option<Running>> {
    static R: OnceLock<Mutex<Option<Running>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(None))
}

fn busy_port() -> Option<String> {
    running().lock().expect("pendant job").as_ref().map(|r| r.port.clone())
}

async fn publish(payload: Value) {
    let Some(bus) = crate::kb_wizard::CURRENT_EVENT_BUS.get() else { return };
    if let Err(e) = bus.publish(BusEvent::sticky(TOPIC_JOB, payload, PublisherIdentity::Internal)).await {
        tracing::warn!(target: "pendant", error = %e, "could not publish job progress");
    }
}

fn start(port: String, task: Task) -> Result<String, String> {
    let mut slot = running().lock().expect("pendant job");
    if slot.is_some() {
        return Err("a pendant is already being set up".into());
    }
    let id = uuid::Uuid::new_v4().to_string();
    let cancel = Arc::new(Cancel::default());
    *slot = Some(Running { id: id.clone(), port: port.clone(), cancel: cancel.clone() });
    let job_id = id.clone();
    let kind = if matches!(task, Task::Pair { .. }) { "pair" } else { "wifi" };
    let emit: job::Emit = Arc::new(move |mut v: Value| {
        if let Some(o) = v.as_object_mut() {
            o.insert("job".into(), Value::String(job_id.clone()));
            o.insert("kind".into(), Value::String(kind.into()));
        }
        tracing::info!(target: "pendant", phase = %v["phase"], "pendant job");
        tokio::spawn(publish(v));
    });
    let done_id = id.clone();
    tokio::spawn(async move {
        job::run(Arc::new(RealEnv), port, task, cancel, emit).await;
        let mut slot = running().lock().expect("pendant job");
        if slot.as_ref().is_some_and(|r| r.id == done_id) {
            *slot = None;
        }
    });
    Ok(id)
}

// ── Commands ──────────────────────────────────────────────────

fn str_arg<'a>(params: &'a Value, k: &str) -> &'a str {
    params.get(k).and_then(Value::as_str).unwrap_or("")
}

pub async fn handle(cmd: &str, params: &Value) -> Value {
    let rid = params.get("request_id").and_then(Value::as_str).unwrap_or("");
    match cmd {
        "pendant.ports" => {
            let busy = busy_port();
            let rows = tokio::task::spawn_blocking(move || probe_ports(busy.as_deref())).await.unwrap_or_default();
            ok_response(rid, cmd, json!({ "ports": rows }))
        }
        "pendant.wifi" => {
            let v = tokio::task::spawn_blocking(wifi::view).await.unwrap_or_default();
            ok_response(rid, cmd, serde_json::to_value(v).unwrap_or_default())
        }
        "pendant.pair" | "pendant.set_wifi" => {
            let port = str_arg(params, "port").to_string();
            let ssid = str_arg(params, "ssid").to_string();
            let password = str_arg(params, "password").to_string();
            if port.is_empty() {
                return err_response(rid, cmd, "BAD_REQUEST", "port is required");
            }
            if cmd == "pendant.set_wifi" && ssid.is_empty() {
                return err_response(rid, cmd, "BAD_REQUEST", "ssid is required");
            }
            let task = if cmd == "pendant.pair" { Task::Pair { ssid, password } } else { Task::Wifi { ssid, password } };
            match start(port, task) {
                Ok(job) => ok_response(rid, cmd, json!({ "job": job })),
                Err(e) => err_response(rid, cmd, "BUSY", e),
            }
        }
        "pendant.cancel" => {
            if let Some(r) = running().lock().expect("pendant job").as_ref() {
                r.cancel.cancel();
            }
            ok_response(rid, cmd, json!({}))
        }
        "pendant.list" => ok_response(rid, cmd, json!({ "pendants": list() })),
        "pendant.unpair" => {
            let id = str_arg(params, "control_channel_id");
            if id.is_empty() {
                return err_response(rid, cmd, "BAD_REQUEST", "control_channel_id is required");
            }
            forget(id).await;
            ok_response(rid, cmd, json!({ "removed": true }))
        }
        _ => err_response(rid, cmd, "UNKNOWN_COMMAND", format!("unknown command {cmd}")),
    }
}

/// Each pendant-looking port and what it says it is. A port a job holds is
/// reported busy rather than opened under it.
fn probe_ports(busy: Option<&str>) -> Vec<Value> {
    let held: Vec<String> = crate::remote::start::control_deps()
        .map(|d| d.pairings.load().unwrap_or_default().into_iter().map(|p| p.control_channel_id).collect())
        .unwrap_or_default();
    serial::ports()
        .into_iter()
        .map(|p| {
            if busy == Some(p.port.as_str()) {
                return json!({ "port": p.port, "busy": true });
            }
            let st = serial::SerialLine::open(&p.port).and_then(|mut l| serial::status(&mut l, Duration::from_secs(3)));
            match st {
                Ok(s) => {
                    let here = s.control_channel_id.as_ref().is_some_and(|c| s.paired && held.contains(c));
                    json!({
                        "port": p.port,
                        "name": s.name,
                        "fw": s.fw,
                        "paired": s.paired,
                        "paired_here": here,
                        "control_channel_id": s.control_channel_id,
                    })
                }
                Err(e) => json!({ "port": p.port, "product": p.product, "error": e.to_string() }),
            }
        })
        .collect()
}

/// The pendants paired here that are still paired.
fn list() -> Vec<Value> {
    let known = load_known();
    let Some(deps) = crate::remote::start::control_deps() else { return Vec::new() };
    let pairings = deps.pairings.load().unwrap_or_default();
    known
        .pendants
        .iter()
        .filter_map(|(id, k)| {
            let p = pairings.iter().find(|p| &p.control_channel_id == id)?;
            let link = crate::remote::presence::link(&p.data_channel_id).map(|l| l.as_str());
            Some(json!({
                "control_channel_id": id,
                "name": p.label.clone().unwrap_or_else(|| k.name.clone()),
                "paired_at": k.paired_at,
                "online": link.is_some(),
                "link": link,
            }))
        })
        .collect()
}
