//! Meetings a pendant records (`docs/plans/nevoflux-pendant-meeting-design.md`
//! in the pendant repo).
//!
//! The pendant only marks a meeting's start and end and sends its sentences
//! as ordinary `speech_*` utterances tagged with the meeting id. Here they are
//! kept: each final transcript becomes a transcript entry, is tidied by a
//! model one at a time in order, and every step is published on the EventBus
//! for `nevoflux://meeting`. The pendant gets its `speech_final` back as usual
//! (it counts sentences) but no partials — nobody reads them on its screen.
//!
//! - [`store`] what is on disk
//! - [`ogg`] the audio container
//! - [`speaker`] the live "me or someone else" guess
//! - [`llm`] the tidying and minutes calls
//! - [`summary`] the minutes
//! - [`rpc`] what the browser asks

pub mod llm;
pub mod ogg;
pub mod rpc;
pub mod speaker;
pub mod store;
pub mod summary;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};

use crate::event_bus::{BusEvent, EventBus, PublisherIdentity};
use speaker::SpeakerModel;
use store::{Entry, Kind, Meta, Store};

pub const TOPIC_STATE: &str = "system:meeting:state";
pub const TOPIC_PARTIAL: &str = "system:meeting:partial";
pub const TOPIC_ENTRY: &str = "system:meeting:entry";
pub const TOPIC_ENDED: &str = "system:meeting:ended";
pub const TOPIC_SUMMARY: &str = "system:meeting:summary";

/// A live meeting nothing has been heard from for this long was abandoned
/// (the pendant died or was taken off the network for good).
const ABANDON_AFTER: Duration = Duration::from_secs(30 * 60);
/// A segment whose final transcript never came is let go after this.
const SEGMENT_STALE: Duration = Duration::from_secs(10 * 60);
const PENDANT_INPUT_RATE: u32 = 16_000;

/// `meetings/settings.json`. Every field has the default the design fixed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Open `nevoflux://meeting` when a pendant starts a meeting.
    pub auto_open: bool,
    /// Tidy each sentence; off, the page shows what was said.
    pub clean: bool,
    pub clean_provider: Option<String>,
    pub clean_model: Option<String>,
    pub summary_provider: Option<String>,
    pub summary_model: Option<String>,
    /// Days a meeting's audio is kept; `None` keeps it forever.
    pub audio_days: Option<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_open: true,
            clean: true,
            clean_provider: None,
            clean_model: None,
            summary_provider: None,
            summary_model: None,
            audio_days: Some(30),
        }
    }
}

struct Live {
    next_seg: u32,
    speaker: SpeakerModel,
    /// Each speech segment's level, for a correction to teach the model.
    levels: HashMap<u32, f32>,
    clean_tx: Option<mpsc::UnboundedSender<Entry>>,
    cleaner: Option<tokio::task::JoinHandle<()>>,
    last_seen: Instant,
}

/// One utterance of a meeting, from `speech_start` until its final transcript.
struct Seg {
    meeting_id: String,
    seg: u32,
    offset_ms: u64,
    late: bool,
    packets: Vec<Vec<u8>>,
    dur_ms: u64,
    rms_db: Option<f32>,
    opened: Instant,
}

/// Sends a frame down to the pendant recording a meeting.
pub type Downlink = Arc<dyn Fn(Value) + Send + Sync>;

#[derive(Default)]
struct Inner {
    live: HashMap<String, Live>,
    /// By utterance id.
    segs: HashMap<String, Seg>,
    /// The way back to each meeting's pendant, from its last `meeting_start`.
    devices: HashMap<String, Downlink>,
}

pub struct Meetings {
    store: Store,
    bus: Option<Arc<EventBus>>,
    inner: Mutex<Inner>,
}

static GLOBAL: OnceLock<Arc<Meetings>> = OnceLock::new();

/// The daemon's meetings. First use (inside the runtime) loads any meeting a
/// previous run left live and starts the housekeeping task.
pub fn global() -> Arc<Meetings> {
    GLOBAL
        .get_or_init(|| {
            let m = Arc::new(Meetings::new(
                Store::new(Store::default_root()),
                crate::kb_wizard::CURRENT_EVENT_BUS.get().cloned(),
            ));
            let weak = Arc::downgrade(&m);
            tokio::spawn(async move {
                if let Some(m) = weak.upgrade() {
                    m.adopt_left_live().await;
                }
                let mut tick = tokio::time::interval(Duration::from_secs(60));
                let mut last_purge: Option<Instant> = None;
                loop {
                    tick.tick().await;
                    let Some(m) = weak.upgrade() else { return };
                    m.sweep().await;
                    if last_purge.map_or(true, |t| t.elapsed() > Duration::from_secs(24 * 3600)) {
                        last_purge = Some(Instant::now());
                        if let Some(days) = m.settings().audio_days {
                            let store = m.store.clone();
                            let _ = tokio::task::spawn_blocking(move || store.purge_audio(days, chrono::Utc::now())).await;
                        }
                    }
                }
            });
            m
        })
        .clone()
}

impl Meetings {
    pub fn new(store: Store, bus: Option<Arc<EventBus>>) -> Self {
        Self { store, bus, inner: Mutex::new(Inner::default()) }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn settings(&self) -> Settings {
        std::fs::read(self.settings_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn put_settings(&self, s: &Settings) -> std::io::Result<()> {
        let p = self.settings_path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, serde_json::to_vec_pretty(s).map_err(std::io::Error::other)?)
    }

    fn settings_path(&self) -> std::path::PathBuf {
        self.store.root().join("settings.json")
    }

    async fn publish(&self, topic: &str, payload: Value, sticky: bool) {
        let Some(bus) = &self.bus else { return };
        let ev = if sticky {
            BusEvent::sticky(topic, payload, PublisherIdentity::Internal)
        } else {
            BusEvent::ephemeral(topic, payload, PublisherIdentity::Internal)
        };
        if let Err(e) = bus.publish(ev).await {
            tracing::warn!(target: "meeting", topic, error = %e, "publish failed");
        }
    }

    async fn publish_state(&self, inner: &Inner) {
        let mut live: Vec<&String> = inner.live.keys().collect();
        live.sort();
        let auto_open = self.settings().auto_open;
        self.publish(TOPIC_STATE, json!({ "live": live, "auto_open": auto_open }), true).await;
    }

    /// `meeting_start`. Repeats (a reconnect re-sends it) are no-ops.
    pub async fn start(self: &Arc<Self>, id: &str) -> bool {
        let mut inner = self.inner.lock().await;
        self.ensure_live(&mut inner, id).await
    }

    async fn ensure_live(self: &Arc<Self>, inner: &mut Inner, id: &str) -> bool {
        if let Some(l) = inner.live.get_mut(id) {
            l.last_seen = Instant::now();
            return true;
        }
        let meta = match self.store.meta(id) {
            Some(m) if m.status == "live" => m,
            // An ended meeting whose late audio is still arriving gets it kept,
            // but is not reopened.
            Some(_) => return false,
            None => Meta {
                id: id.to_string(),
                started_at: chrono::Utc::now().to_rfc3339(),
                status: "live".into(),
                ..Default::default()
            },
        };
        if let Err(e) = self.store.create(&meta) {
            tracing::warn!(target: "meeting", id, error = %e, "cannot create the meeting");
            return false;
        }
        let next_seg = self.store.transcript(id).iter().map(|e| e.seg + 1).max().unwrap_or(0);
        let (tx, rx) = mpsc::unbounded_channel();
        let cleaner = tokio::spawn(clean_worker(self.clone(), id.to_string(), rx));
        inner.live.insert(
            id.to_string(),
            Live {
                next_seg,
                speaker: SpeakerModel::default(),
                levels: HashMap::new(),
                clean_tx: Some(tx),
                cleaner: Some(cleaner),
                last_seen: Instant::now(),
            },
        );
        tracing::info!(target: "meeting", id, "meeting started");
        self.publish_state(inner).await;
        true
    }

    /// `speech_start` carrying `meeting_id`: this utterance is a meeting
    /// sentence. False if it cannot be kept (bad id, or a meeting long over).
    pub async fn begin_segment(self: &Arc<Self>, utterance_id: &str, meeting_id: &str, offset_ms: u64, late: bool) -> bool {
        let mut inner = self.inner.lock().await;
        if !inner.live.contains_key(meeting_id) && !self.ensure_live(&mut inner, meeting_id).await {
            return false;
        }
        let live = inner.live.get_mut(meeting_id).expect("ensured above");
        live.last_seen = Instant::now();
        let seg = live.next_seg;
        live.next_seg += 1;
        inner.segs.insert(
            utterance_id.to_string(),
            Seg {
                meeting_id: meeting_id.to_string(),
                seg,
                offset_ms,
                late,
                packets: Vec::new(),
                dur_ms: 0,
                rms_db: None,
                opened: Instant::now(),
            },
        );
        true
    }

    pub async fn is_segment(&self, utterance_id: &str) -> bool {
        self.inner.lock().await.segs.contains_key(utterance_id)
    }

    /// One Opus packet of a meeting sentence.
    pub async fn audio(&self, utterance_id: &str, opus: Vec<u8>) {
        if let Some(s) = self.inner.lock().await.segs.get_mut(utterance_id) {
            s.packets.push(opus);
        }
    }

    /// `speech_end`: the sentence's audio is complete and goes to disk; its
    /// transcript is still on the way.
    pub async fn end_segment(&self, utterance_id: &str, rms_db: Option<f32>) {
        let (id, seg, packets) = {
            let mut inner = self.inner.lock().await;
            let Some(s) = inner.segs.get_mut(utterance_id) else { return };
            s.rms_db = rms_db;
            s.dur_ms = ogg::duration_ms(&s.packets);
            (s.meeting_id.clone(), s.seg, std::mem::take(&mut s.packets))
        };
        if packets.is_empty() {
            return;
        }
        let store = self.store.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) = store.put_audio(&id, seg, &ogg::encode(&packets, PENDANT_INPUT_RATE)) {
                tracing::warn!(target: "meeting", id, seg, error = %e, "cannot keep a segment's audio");
            }
        })
        .await;
    }

    pub async fn cancel_segment(&self, utterance_id: &str) {
        self.inner.lock().await.segs.remove(utterance_id);
    }

    /// A partial transcript: for the page only.
    pub async fn partial(&self, utterance_id: &str, text: &str) {
        let target = {
            let inner = self.inner.lock().await;
            inner.segs.get(utterance_id).map(|s| (s.meeting_id.clone(), s.seg, s.offset_ms))
        };
        if let Some((id, seg, offset_ms)) = target {
            self.publish(TOPIC_PARTIAL, json!({ "meeting_id": id, "seg": seg, "offset_ms": offset_ms, "text": text }), false)
                .await;
        }
    }

    /// The authoritative transcript: the sentence joins the record and is
    /// queued for tidying. A rejected or empty one (noise) is dropped.
    pub async fn finalize(&self, utterance_id: &str, text: &str, accepted: bool) {
        let mut inner = self.inner.lock().await;
        let Some(s) = inner.segs.remove(utterance_id) else { return };
        let text = text.trim();
        if !accepted || text.is_empty() {
            self.publish(TOPIC_PARTIAL, json!({ "meeting_id": s.meeting_id, "seg": s.seg, "text": "", "dropped": true }), false)
                .await;
            return;
        }
        let live = inner.live.get_mut(&s.meeting_id);
        let who = match (s.rms_db, &live) {
            (Some(db), Some(l)) => if l.speaker.is_me(db) { "me" } else { "other" },
            _ => "other",
        };
        let entry = Entry {
            seg: s.seg,
            offset_ms: s.offset_ms,
            dur_ms: s.dur_ms,
            kind: Kind::Speech,
            who: who.into(),
            raw: text.to_string(),
            clean: None,
            clean_failed: false,
            late: s.late,
            who_fixed: false,
        };
        if let Some(l) = live {
            if let Some(db) = s.rms_db {
                l.speaker.observe(db);
                l.levels.insert(s.seg, db);
            }
            l.last_seen = Instant::now();
            if let Some(tx) = &l.clean_tx {
                let _ = tx.send(entry.clone());
            }
        }
        drop(inner);
        self.save_entry(&s.meeting_id, &entry).await;
    }

    /// `meeting_note`: the wearer's own annotation.
    pub async fn note(self: &Arc<Self>, meeting_id: &str, text: &str, offset_ms: u64) -> bool {
        self.add_own(meeting_id, Kind::Note, text, None, offset_ms).await
    }

    /// A question asked during the meeting and its answer.
    pub async fn qa(self: &Arc<Self>, meeting_id: &str, question: &str, answer: &str, offset_ms: u64) -> bool {
        self.add_own(meeting_id, Kind::Qa, question, Some(answer.to_string()), offset_ms).await
    }

    async fn add_own(self: &Arc<Self>, meeting_id: &str, kind: Kind, raw: &str, clean: Option<String>, offset_ms: u64) -> bool {
        let seg = {
            let mut inner = self.inner.lock().await;
            if !inner.live.contains_key(meeting_id) && !self.ensure_live(&mut inner, meeting_id).await {
                return false;
            }
            let l = inner.live.get_mut(meeting_id).expect("ensured above");
            l.last_seen = Instant::now();
            l.next_seg += 1;
            l.next_seg - 1
        };
        let entry = Entry {
            seg,
            offset_ms,
            dur_ms: 0,
            kind,
            who: "me".into(),
            raw: raw.trim().to_string(),
            clean,
            clean_failed: false,
            late: false,
            who_fixed: false,
        };
        self.save_entry(meeting_id, &entry).await;
        true
    }

    async fn save_entry(&self, meeting_id: &str, entry: &Entry) {
        let store = self.store.clone();
        let (id, e) = (meeting_id.to_string(), entry.clone());
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(err) = store.put_entry(&id, &e) {
                tracing::warn!(target: "meeting", id, seg = e.seg, error = %err, "cannot record a sentence");
            }
        })
        .await;
        self.publish(TOPIC_ENTRY, json!({ "meeting_id": meeting_id, "entry": entry }), false).await;
    }

    /// Answer a question asked during the meeting from what has been said so
    /// far, and keep both in the record.
    pub async fn ask(self: &Arc<Self>, meeting_id: &str, question: &str, offset_ms: u64) -> String {
        let transcript = summary::render_transcript(&self.store.transcript(meeting_id));
        let config = crate::config::AgentConfig::load().unwrap_or_default();
        let answer = match llm::ask(&config, &transcript, question).await {
            Ok(a) if !a.is_empty() => a,
            Ok(_) => "抱歉，这个我没听到。".to_string(),
            Err(e) => {
                tracing::warn!(target: "meeting", meeting_id, error = %e, "cannot answer");
                "抱歉，现在没法回答。".to_string()
            }
        };
        self.qa(meeting_id, question, &answer, offset_ms).await;
        answer
    }

    /// The page corrected who said a sentence.
    pub async fn set_speaker(&self, meeting_id: &str, seg: u32, who: &str) -> bool {
        let Some(mut entry) = self.store.transcript(meeting_id).into_iter().find(|e| e.seg == seg) else {
            return false;
        };
        entry.who = who.to_string();
        entry.who_fixed = true;
        if let Some(l) = self.inner.lock().await.live.get_mut(meeting_id) {
            if let Some(&db) = l.levels.get(&seg) {
                l.speaker.correct(db, who == "me");
            }
        }
        self.save_entry(meeting_id, &entry).await;
        true
    }

    /// `meeting_end`. The minutes are written once the last sentence is tidied.
    pub async fn end(self: &Arc<Self>, meeting_id: &str, gaps: Vec<[u64; 2]>, status: &str) -> bool {
        let live = {
            let mut inner = self.inner.lock().await;
            let l = inner.live.remove(meeting_id);
            if l.is_some() {
                self.publish_state(&inner).await;
            }
            l
        };
        let Some(mut live) = live else {
            return self.store.meta(meeting_id).is_some();
        };
        if let Some(mut meta) = self.store.meta(meeting_id) {
            meta.status = status.to_string();
            meta.ended_at = Some(chrono::Utc::now().to_rfc3339());
            meta.gaps = gaps;
            if let Err(e) = self.store.put_meta(&meta) {
                tracing::warn!(target: "meeting", meeting_id, error = %e, "cannot close the meeting");
            }
        }
        tracing::info!(target: "meeting", meeting_id, status, "meeting ended");
        self.publish(TOPIC_ENDED, json!({ "meeting_id": meeting_id, "status": status }), false).await;

        drop(live.clean_tx.take());
        let cleaner = live.cleaner.take();
        let me = self.clone();
        let id = meeting_id.to_string();
        tokio::spawn(async move {
            if let Some(c) = cleaner {
                let _ = c.await;
            }
            summary::run(&me, &id).await;
            me.inner.lock().await.devices.remove(&id);
        });
        true
    }

    /// Where to send this meeting's frames from now on (a reconnect gives a
    /// new way back).
    pub async fn attach_device(&self, meeting_id: &str, downlink: Downlink) {
        self.inner.lock().await.devices.insert(meeting_id.to_string(), downlink);
    }

    pub async fn tell_device(&self, meeting_id: &str, frame: Value) {
        let d = self.inner.lock().await.devices.get(meeting_id).cloned();
        match d {
            Some(send) => send(frame),
            None => tracing::debug!(target: "meeting", meeting_id, "no pendant to tell"),
        }
    }

    pub async fn live_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.inner.lock().await.live.keys().cloned().collect();
        v.sort();
        v
    }

    /// A meeting a crashed daemon left `live` on disk carries on if its
    /// pendant is still talking, and is abandoned on the usual clock if not.
    async fn adopt_left_live(self: &Arc<Self>) {
        let left: Vec<String> = self.store.list().into_iter().filter(|l| l.meta.status == "live").map(|l| l.meta.id).collect();
        let mut inner = self.inner.lock().await;
        for id in left {
            self.ensure_live(&mut inner, &id).await;
        }
    }

    async fn sweep(self: &Arc<Self>) {
        let abandoned: Vec<String> = {
            let mut inner = self.inner.lock().await;
            inner.segs.retain(|_, s| s.opened.elapsed() < SEGMENT_STALE);
            inner.live.iter().filter(|(_, l)| l.last_seen.elapsed() > ABANDON_AFTER).map(|(k, _)| k.clone()).collect()
        };
        for id in abandoned {
            self.end(&id, Vec::new(), "aborted").await;
        }
    }

    /// Delete a meeting and its minutes in the knowledge base.
    pub async fn delete(self: &Arc<Self>, meeting_id: &str) -> std::io::Result<bool> {
        if let Some(mut l) = self.inner.lock().await.live.remove(meeting_id) {
            drop(l.clean_tx.take());
        }
        let brain_ref = self.store.summary(meeting_id).and_then(|s| s.get("brain_ref").and_then(|v| v.as_str()).map(String::from));
        if let Some(slug) = brain_ref {
            summary::forget_in_brain(&slug).await;
        }
        self.store.delete(meeting_id)
    }
}

/// Tidies one meeting's sentences in the order they were said, each with the
/// tidied sentences before it as context.
async fn clean_worker(meetings: Arc<Meetings>, meeting_id: String, mut rx: mpsc::UnboundedReceiver<Entry>) {
    let mut context: VecDeque<String> = VecDeque::new();
    while let Some(mut entry) = rx.recv().await {
        let settings = meetings.settings();
        if !settings.clean {
            continue;
        }
        let config = crate::config::AgentConfig::load().unwrap_or_default();
        let ctx: Vec<String> = context.iter().cloned().collect();
        let result = match llm::resolve(&config, settings.clean_provider.as_deref(), settings.clean_model.as_deref()) {
            Ok(choice) => llm::clean(&choice, &ctx, &entry.raw).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(text) => {
                if !text.is_empty() {
                    context.push_back(text.clone());
                    if context.len() > llm::CLEAN_CONTEXT {
                        context.pop_front();
                    }
                }
                entry.clean = Some(text);
            }
            Err(e) => {
                tracing::debug!(target: "meeting", meeting_id, seg = entry.seg, error = %e, "sentence kept as said");
                entry.clean_failed = true;
            }
        }
        // A correction made while this was being tidied must survive it.
        if let Some(now) = meetings.store.transcript(&meeting_id).into_iter().find(|e| e.seg == entry.seg) {
            entry.who = now.who;
            entry.who_fixed = now.who_fixed;
        }
        meetings.save_entry(&meeting_id, &entry).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Vec<u8> {
        let mut p = vec![0u8; 40];
        p[0] = 31 << 3;
        p
    }

    #[tokio::test]
    async fn a_sentence_goes_from_audio_to_the_record() {
        let tmp = tempfile::tempdir().unwrap();
        let m = Arc::new(Meetings::new(Store::new(tmp.path()), None));
        let mut s = m.settings();
        s.clean = false; // no model in a unit test
        m.put_settings(&s).unwrap();

        assert!(m.start("11").await);
        assert!(m.start("11").await);
        assert!(m.begin_segment("u1", "11", 1200, false).await);
        assert!(m.is_segment("u1").await);
        for _ in 0..50 {
            m.audio("u1", frame()).await;
        }
        m.end_segment("u1", Some(-18.0)).await;
        m.finalize("u1", " 我们下周确认预算 ", true).await;
        assert!(!m.is_segment("u1").await);

        // Noise never reaches the record.
        assert!(m.begin_segment("u2", "11", 5000, false).await);
        m.finalize("u2", "", true).await;

        assert!(m.note("11", "标记一下，预算", 6000).await);

        let t = m.store().transcript("11");
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].raw.as_str(), t[0].who.as_str(), t[0].dur_ms), ("我们下周确认预算", "me", 1000));
        assert_eq!(t[1].kind, Kind::Note);
        assert!(m.store().audio_path("11", 0).is_some());

        assert!(m.set_speaker("11", 0, "other").await);
        assert_eq!(m.store().transcript("11")[0].who, "other");

        assert!(m.end("11", vec![[100, 200]], "ended").await);
        let meta = m.store().meta("11").unwrap();
        assert_eq!(meta.status, "ended");
        assert_eq!(meta.gaps, vec![[100, 200]]);
        assert!(m.live_ids().await.is_empty());
        // Ended: a late start does not reopen it.
        assert!(!m.start("11").await);
    }
}
