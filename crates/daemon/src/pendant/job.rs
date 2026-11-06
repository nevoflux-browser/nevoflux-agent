//! One pairing (or Wi-Fi change) of a pendant on a USB port, step by step.
//!
//! ```text
//! probing → authorize → pairing → confirm → writing → replacing → waiting → done
//!                                                        (or failed / cancelled)
//! ```
//!
//! The order is what keeps a failure from leaving half a pairing behind:
//! nothing exists until the person authorizes; the pairing minted after that
//! is dropped again unless the pendant took it; and the pairing the pendant
//! held before is dropped only once the new one is written.
//!
//! Everything outside this file is behind [`Env`], so the order is tested
//! without a pendant, a relay or nevoflux.app.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::serial::{self, Line, ProvError};
use crate::remote::account::{DeviceCodeResp, PollOutcome};

/// After the write, how long to wait for the pendant to come up on its new
/// pairing before calling it done anyway (it may still be joining Wi-Fi).
pub const CONNECT_WAIT: Duration = Duration::from_secs(60);

/// A pairing this head just minted for the pendant.
#[derive(Debug, Clone)]
pub struct Minted {
    pub control_channel_id: String,
    pub data_channel_id: String,
    pub control_key_hex: String,
    pub data_key_hex: String,
}

/// What a job does to the world.
#[async_trait]
pub trait Env: Send + Sync {
    /// Blocking: runs on a blocking thread.
    fn open(&self, port: &str) -> Result<Box<dyn Line>, ProvError>;
    async fn device_code(&self) -> Result<DeviceCodeResp, String>;
    async fn poll(&self, device_code: &str) -> Result<PollOutcome, String>;
    async fn pair(&self, label: &str) -> Result<Minted, String>;
    async fn unpair(&self, control_channel_id: &str);
    /// Whether this head holds the pairing with this control channel.
    fn holds(&self, control_channel_id: &str) -> bool;
    fn link(&self, data_channel_id: &str) -> Option<&'static str>;
    fn relay_url(&self) -> String;
    /// How the pendant's BOOT prompt names this computer.
    fn host(&self) -> String;
    async fn sleep(&self, d: Duration);
    /// How long a pendant that is still booting is waited for.
    fn boot_timeout(&self) -> Duration {
        serial::BOOT_TIMEOUT
    }
    /// Between `prov.status` polls while the BOOT press is awaited.
    fn confirm_poll(&self) -> Duration {
        Duration::from_secs(1)
    }
}

pub enum Task {
    Pair { ssid: String, password: String },
    Wifi { ssid: String, password: String },
}

/// A job's progress, as the settings page receives it.
pub type Emit = Arc<dyn Fn(Value) + Send + Sync>;

#[derive(Default)]
pub struct Cancel(AtomicBool);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    fn is(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

enum Stop {
    Failed(&'static str, String),
    Cancelled,
}

type LineCell = Arc<Mutex<Box<dyn Line>>>;

/// Run a blocking step on the port.
async fn on_port<T: Send + 'static>(
    line: &LineCell,
    f: impl FnOnce(&mut dyn Line) -> Result<T, ProvError> + Send + 'static,
) -> Result<T, ProvError> {
    let line = line.clone();
    tokio::task::spawn_blocking(move || {
        let mut l = line.lock().map_err(|_| ProvError::Io("the port is unusable".into()))?;
        f(l.as_mut())
    })
    .await
    .map_err(|e| ProvError::Io(e.to_string()))?
}

/// Run one job to its end; the last thing emitted is `done`, `failed` or `cancelled`.
pub async fn run(env: Arc<dyn Env>, port: String, task: Task, cancel: Arc<Cancel>, emit: Emit) {
    let end = match drive(&env, &port, task, &cancel, &emit).await {
        Ok(done) => done,
        Err(Stop::Cancelled) => json!({ "phase": "cancelled" }),
        Err(Stop::Failed(phase, reason)) => json!({ "phase": "failed", "at": phase, "reason": reason }),
    };
    emit(end);
}

async fn drive(env: &Arc<dyn Env>, port: &str, task: Task, cancel: &Cancel, emit: &Emit) -> Result<Value, Stop> {
    let check = || if cancel.is() { Err(Stop::Cancelled) } else { Ok(()) };
    let fail = |phase: &'static str| move |e: ProvError| Stop::Failed(phase, e.to_string());

    emit(json!({ "phase": "probing", "port": port }));
    let line: LineCell = {
        let env = env.clone();
        let p = port.to_string();
        let l = tokio::task::spawn_blocking(move || env.open(&p))
            .await
            .map_err(|e| Stop::Failed("probing", e.to_string()))?
            .map_err(fail("probing"))?;
        Arc::new(Mutex::new(l))
    };
    let boot = env.boot_timeout();
    let st = on_port(&line, move |l| serial::status(l, boot)).await.map_err(fail("probing"))?;
    let name = st.name.clone().unwrap_or_else(|| "NevoFlux pendant".to_string());
    // Only a pairing this head holds is this head's to replace.
    let old = st.control_channel_id.clone().filter(|c| st.paired && env.holds(c));
    check()?;

    let (ssid, password) = match &task {
        Task::Pair { ssid, password } | Task::Wifi { ssid, password } => (ssid.clone(), password.clone()),
    };

    if let Task::Wifi { .. } = task {
        if ssid.is_empty() {
            return Err(Stop::Failed("writing", "a Wi-Fi name is needed".into()));
        }
        confirm(env, &line, emit, &name).await?;
        check()?;
        emit(json!({ "phase": "writing", "name": name }));
        on_port(&line, move |l| serial::set_wifi(l, &ssid, &password)).await.map_err(fail("writing"))?;
        drop(line);
        return Ok(json!({ "phase": "done", "name": name, "wifi": true }));
    }

    // The pendant's own account token: one more person-approved login.
    let code = env.device_code().await.map_err(|e| Stop::Failed("authorize", e))?;
    emit(json!({
        "phase": "authorize",
        "name": name,
        "url": code.verification_uri_complete.clone().unwrap_or_else(|| code.verification_uri.clone()),
        "user_code": code.user_code,
        "expires_in_secs": code.expires_in_secs,
    }));
    let mut interval = Duration::from_secs(code.interval_secs.max(1));
    let mut waited = Duration::ZERO;
    let token = loop {
        check()?;
        if waited >= Duration::from_secs(code.expires_in_secs.max(1)) {
            return Err(Stop::Failed("authorize", "the authorization was not given in time".into()));
        }
        env.sleep(interval).await;
        waited += interval;
        match env.poll(&code.device_code).await {
            Ok(PollOutcome::Token(t)) => break t,
            Ok(PollOutcome::Pending) => {}
            Ok(PollOutcome::SlowDown) => interval += Duration::from_secs(5),
            Ok(PollOutcome::Denied(d)) => return Err(Stop::Failed("authorize", format!("authorization refused: {d}"))),
            // One failed poll is a network blip, not a refusal.
            Err(e) => tracing::debug!(target: "pendant", error = %e, "authorization poll failed"),
        }
    };
    check()?;

    emit(json!({ "phase": "pairing", "name": name }));
    let minted = env.pair(&name).await.map_err(|e| Stop::Failed("pairing", e))?;

    // From here on the minted pairing is dropped again unless the pendant took it.
    let written = async {
        check()?;
        confirm(env, &line, emit, &name).await?;
        check()?;
        emit(json!({ "phase": "writing", "name": name }));
        let relay = env.relay_url();
        let m = minted.clone();
        on_port(&line, move |l| {
            serial::write(
                l,
                &serial::Credentials {
                    account_token: &token,
                    relay_url: &relay,
                    control_channel_id: &m.control_channel_id,
                    control_key_hex: &m.control_key_hex,
                    data_channel_id: &m.data_channel_id,
                    data_key_hex: &m.data_key_hex,
                    wifi_ssid: &ssid,
                    wifi_pass: &password,
                },
            )
        })
        .await
        .map_err(fail("writing"))
    }
    .await;
    if let Err(stop) = written {
        env.unpair(&minted.control_channel_id).await;
        return Err(stop);
    }
    drop(line); // the pendant restarts its link; the port is not needed again

    if let Some(old) = old.filter(|o| *o != minted.control_channel_id) {
        emit(json!({ "phase": "replacing", "name": name }));
        env.unpair(&old).await;
    }

    emit(json!({ "phase": "waiting", "name": name }));
    let mut waited = Duration::ZERO;
    let step = Duration::from_secs(1);
    let link = loop {
        if let Some(l) = env.link(&minted.data_channel_id) {
            break Some(l);
        }
        if waited >= CONNECT_WAIT || cancel.is() {
            break None; // written all the same: it connects when its Wi-Fi does
        }
        env.sleep(step).await;
        waited += step;
    };
    Ok(json!({ "phase": "done", "name": name, "link": link, "control_channel_id": minted.control_channel_id }))
}

async fn confirm(env: &Arc<dyn Env>, line: &LineCell, emit: &Emit, name: &str) -> Result<(), Stop> {
    let host = env.host();
    let poll = env.confirm_poll();
    let e = emit.clone();
    let n = name.to_string();
    on_port(line, move |l| {
        serial::offer_and_confirm(l, &host, poll, serial::CONFIRM_TIMEOUT, &mut || {
            e(json!({ "phase": "confirm", "name": n }));
        })
    })
    .await
    .map_err(|err| Stop::Failed("confirm", err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::serial::fake::Scripted;
    use super::*;
    use std::sync::Mutex as StdMutex;

    const STATUS_OLD: &str =
        r#"{"status":"idle","paired":true,"control_channel_id":"old","name":"NevoFlux-A1B2","fw":"nevoflux"}"#;

    struct FakeEnv {
        replies: StdMutex<Option<Scripted>>,
        polls: StdMutex<Vec<PollOutcome>>,
        holds: Vec<&'static str>,
        unpaired: StdMutex<Vec<String>>,
        connects: bool,
        sent: Arc<StdMutex<Vec<Value>>>,
    }

    struct Recording(Scripted, Arc<StdMutex<Vec<Value>>>);

    impl Line for Recording {
        fn send(&mut self, line: &str) -> std::io::Result<()> {
            self.1.lock().unwrap().push(serde_json::from_str(line).unwrap());
            self.0.send(line)
        }
        fn recv(&mut self, t: Duration) -> std::io::Result<Option<String>> {
            self.0.recv(t)
        }
    }

    impl FakeEnv {
        fn new(replies: &[&[&str]], polls: Vec<PollOutcome>) -> Self {
            Self {
                replies: StdMutex::new(Some(Scripted::new(replies))),
                polls: StdMutex::new(polls),
                holds: vec!["old"],
                unpaired: StdMutex::new(Vec::new()),
                connects: true,
                sent: Arc::new(StdMutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl Env for FakeEnv {
        fn open(&self, _port: &str) -> Result<Box<dyn Line>, ProvError> {
            let s = self.replies.lock().unwrap().take().ok_or(ProvError::NoAnswer)?;
            Ok(Box::new(Recording(s, self.sent.clone())))
        }
        async fn device_code(&self) -> Result<DeviceCodeResp, String> {
            Ok(DeviceCodeResp {
                device_code: "dc".into(),
                user_code: "ABCD-EFGH".into(),
                verification_uri: "https://nevoflux.app/device".into(),
                verification_uri_complete: Some("https://nevoflux.app/device?user_code=ABCD-EFGH".into()),
                interval_secs: 5,
                expires_in_secs: 600,
            })
        }
        async fn poll(&self, _dc: &str) -> Result<PollOutcome, String> {
            let mut p = self.polls.lock().unwrap();
            Ok(if p.is_empty() { PollOutcome::Pending } else { p.remove(0) })
        }
        async fn pair(&self, _label: &str) -> Result<Minted, String> {
            Ok(Minted {
                control_channel_id: "new".into(),
                data_channel_id: "new-data".into(),
                control_key_hex: "11".repeat(32),
                data_key_hex: "22".repeat(32),
            })
        }
        async fn unpair(&self, id: &str) {
            self.unpaired.lock().unwrap().push(id.to_string());
        }
        fn holds(&self, id: &str) -> bool {
            self.holds.contains(&id)
        }
        fn link(&self, _d: &str) -> Option<&'static str> {
            self.connects.then_some("lan")
        }
        fn relay_url(&self) -> String {
            "wss://relay.test".into()
        }
        fn host(&self) -> String {
            "PC".into()
        }
        async fn sleep(&self, _d: Duration) {}
        fn boot_timeout(&self) -> Duration {
            Duration::from_millis(30)
        }
        fn confirm_poll(&self) -> Duration {
            Duration::ZERO
        }
    }

    async fn go(env: FakeEnv, task: Task) -> (Arc<FakeEnv>, Vec<Value>) {
        let env = Arc::new(env);
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let s = seen.clone();
        let emit: Emit = Arc::new(move |v| s.lock().unwrap().push(v));
        run(env.clone(), "COM5".into(), task, Arc::new(Cancel::default()), emit).await;
        let v = seen.lock().unwrap().clone();
        (env, v)
    }

    fn phases(v: &[Value]) -> Vec<String> {
        v.iter().map(|e| e["phase"].as_str().unwrap().to_string()).collect()
    }

    fn pair() -> Task {
        Task::Pair { ssid: "HomeNet".into(), password: "pw".into() }
    }

    #[tokio::test]
    async fn pairs_replaces_the_old_pairing_and_waits_for_the_pendant() {
        let env = FakeEnv::new(
            &[&[STATUS_OLD], &[r#"{"status":"wait_boot"}"#], &[r#"{"status":"ready"}"#], &[r#"{"status":"ok"}"#]],
            vec![PollOutcome::Pending, PollOutcome::Token("tok7d".into())],
        );
        let (env, seen) = go(env, pair()).await;
        assert_eq!(
            phases(&seen),
            ["probing", "authorize", "pairing", "confirm", "writing", "replacing", "waiting", "done"]
        );
        assert_eq!(seen[1]["url"], "https://nevoflux.app/device?user_code=ABCD-EFGH");
        assert_eq!(seen[7]["link"], "lan");
        assert_eq!(*env.unpaired.lock().unwrap(), vec!["old".to_string()]);
        let sent = env.sent.lock().unwrap();
        let write = sent.iter().find(|v| v["cmd"] == "prov.write").unwrap();
        assert_eq!(write["account_token"], "tok7d");
        assert_eq!(write["control_channel_id"], "new");
        assert_eq!(write["relay_url"], "wss://relay.test");
        assert_eq!(write["wifi_ssid"], "HomeNet");
    }

    #[tokio::test]
    async fn a_refused_authorization_leaves_nothing_behind() {
        let env = FakeEnv::new(&[&[STATUS_OLD]], vec![PollOutcome::Denied("access_denied".into())]);
        let (env, seen) = go(env, pair()).await;
        assert_eq!(phases(&seen), ["probing", "authorize", "failed"]);
        assert_eq!(seen[2]["at"], "authorize");
        assert!(env.unpaired.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_boot_press_drops_the_new_pairing_and_keeps_the_old() {
        let env = FakeEnv::new(
            &[&[STATUS_OLD], &[r#"{"status":"wait_boot"}"#], &[r#"{"status":"idle"}"#]],
            vec![PollOutcome::Token("tok".into())],
        );
        let (env, seen) = go(env, pair()).await;
        assert_eq!(phases(&seen), ["probing", "authorize", "pairing", "confirm", "failed"]);
        assert_eq!(seen[4]["at"], "confirm");
        assert_eq!(*env.unpaired.lock().unwrap(), vec!["new".to_string()]);
    }

    #[tokio::test]
    async fn a_pairing_held_elsewhere_is_not_ours_to_drop() {
        let mut env = FakeEnv::new(
            &[&[STATUS_OLD], &[r#"{"status":"ready"}"#], &[r#"{"status":"ok"}"#]],
            vec![PollOutcome::Token("tok".into())],
        );
        env.holds = vec![];
        env.connects = false;
        let (env, seen) = go(env, pair()).await;
        assert_eq!(phases(&seen), ["probing", "authorize", "pairing", "writing", "waiting", "done"]);
        assert!(seen.last().unwrap()["link"].is_null());
        assert!(env.unpaired.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn wifi_only_needs_boot_but_no_authorization() {
        let env = FakeEnv::new(
            &[&[r#"{"status":"idle","paired":true}"#], &[r#"{"status":"wait_boot"}"#], &[r#"{"status":"ready"}"#], &[r#"{"status":"ok"}"#]],
            vec![],
        );
        let (env, seen) = go(env, Task::Wifi { ssid: "Cafe".into(), password: "".into() }).await;
        assert_eq!(phases(&seen), ["probing", "confirm", "writing", "done"]);
        assert!(env.sent.lock().unwrap().iter().any(|v| v["cmd"] == "prov.wifi" && v["wifi_ssid"] == "Cafe"));
    }

    #[tokio::test]
    async fn a_silent_port_fails_at_probing() {
        let env = FakeEnv::new(&[], vec![]);
        let (_env, seen) = go(env, pair()).await;
        assert_eq!(phases(&seen), ["probing", "failed"]);
    }
}
