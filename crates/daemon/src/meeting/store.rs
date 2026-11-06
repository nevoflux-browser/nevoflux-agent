//! What a meeting leaves on disk, under `<data dir>/meetings/`:
//!
//! ```text
//! 2026-11-06T14-30-05_<meeting id>/
//!   meta.json         Meta
//!   transcript.jsonl  one Entry per line; a later line for the same seg replaces it
//!   audio/<seg>.ogg   that segment's Opus, as the pendant encoded it
//!   summary.json      written by the summary pass
//! ```
//!
//! The transcript is append-only so a crash loses at most the line being
//! written; a sentence's clean text and a speaker correction arrive after the
//! sentence itself and are appended as its new state, folded on read.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    /// RFC 3339.
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// `live`, `ended`, or `aborted` (the pendant never came back to end it).
    pub status: String,
    /// The pendant's buffer overflowed during these spans `[from_ms, to_ms]`.
    #[serde(default)]
    pub gaps: Vec<[u64; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_purged_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Something said in the meeting.
    Speech,
    /// The wearer's own annotation ("标记一下…").
    Note,
    /// The wearer asked something; `raw` is the question, `clean` the answer.
    Qa,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub seg: u32,
    /// From the meeting's start, on the pendant's clock.
    pub offset_ms: u64,
    #[serde(default)]
    pub dur_ms: u64,
    pub kind: Kind,
    /// `me`, `other`, or (after diarization) `spk1`, `spk2`, …
    pub who: String,
    pub raw: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clean: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clean_failed: bool,
    /// Sent up from the pendant's offline buffer after a reconnect.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub late: bool,
    /// The wearer corrected `who`; the speaker model learns from it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub who_fixed: bool,
}

/// One row of the history list.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Listing {
    #[serde(flatten)]
    pub meta: Meta,
    pub sentences: usize,
    pub has_summary: bool,
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<data dir>/meetings`, the daemon's real store.
    pub fn default_root() -> PathBuf {
        crate::paths::resolve_from_daemon().data_dir.join("meetings")
    }

    /// The meeting's directory, if it exists.
    pub fn dir(&self, id: &str) -> Option<PathBuf> {
        if !valid_id(id) {
            return None;
        }
        let suffix = format!("_{id}");
        std::fs::read_dir(&self.root)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(&suffix)))
    }

    /// Make the directory for a new meeting; an existing one is kept as is,
    /// so a pendant re-sending `meeting_start` after a reconnect is harmless.
    pub fn create(&self, meta: &Meta) -> std::io::Result<PathBuf> {
        if !valid_id(&meta.id) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad meeting id"));
        }
        if let Some(d) = self.dir(&meta.id) {
            return Ok(d);
        }
        let stamp = chrono::DateTime::parse_from_rfc3339(&meta.started_at)
            .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%dT%H-%M-%S").to_string())
            .unwrap_or_else(|_| chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string());
        let dir = self.root.join(format!("{stamp}_{}", meta.id));
        std::fs::create_dir_all(dir.join("audio"))?;
        write_json(&dir.join("meta.json"), meta)?;
        Ok(dir)
    }

    pub fn meta(&self, id: &str) -> Option<Meta> {
        let dir = self.dir(id)?;
        serde_json::from_slice(&std::fs::read(dir.join("meta.json")).ok()?).ok()
    }

    pub fn put_meta(&self, meta: &Meta) -> std::io::Result<()> {
        let dir = self.dir(&meta.id).ok_or_else(not_found)?;
        write_json(&dir.join("meta.json"), meta)
    }

    /// Append an entry's current state.
    pub fn put_entry(&self, id: &str, entry: &Entry) -> std::io::Result<()> {
        let dir = self.dir(id).ok_or_else(not_found)?;
        let mut line = serde_json::to_vec(entry).map_err(std::io::Error::other)?;
        line.push(b'\n');
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("transcript.jsonl"))?
            .write_all(&line)
    }

    /// The transcript in time order, each seg at its latest state. A torn last
    /// line (the daemon died writing it) is skipped.
    pub fn transcript(&self, id: &str) -> Vec<Entry> {
        let Some(dir) = self.dir(id) else { return Vec::new() };
        let Ok(text) = std::fs::read_to_string(dir.join("transcript.jsonl")) else {
            return Vec::new();
        };
        let mut by_seg = BTreeMap::new();
        for line in text.lines() {
            if let Ok(e) = serde_json::from_str::<Entry>(line) {
                by_seg.insert(e.seg, e);
            }
        }
        let mut out: Vec<Entry> = by_seg.into_values().collect();
        out.sort_by_key(|e| (e.offset_ms, e.seg));
        out
    }

    pub fn put_audio(&self, id: &str, seg: u32, ogg: &[u8]) -> std::io::Result<()> {
        let dir = self.dir(id).ok_or_else(not_found)?.join("audio");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("{seg}.ogg")), ogg)
    }

    pub fn audio_path(&self, id: &str, seg: u32) -> Option<PathBuf> {
        let p = self.dir(id)?.join("audio").join(format!("{seg}.ogg"));
        p.is_file().then_some(p)
    }

    pub fn summary(&self, id: &str) -> Option<serde_json::Value> {
        let dir = self.dir(id)?;
        serde_json::from_slice(&std::fs::read(dir.join("summary.json")).ok()?).ok()
    }

    pub fn put_summary(&self, id: &str, summary: &serde_json::Value) -> std::io::Result<()> {
        let dir = self.dir(id).ok_or_else(not_found)?;
        write_json(&dir.join("summary.json"), summary)
    }

    /// Newest first.
    pub fn list(&self) -> Vec<Listing> {
        let Ok(rd) = std::fs::read_dir(&self.root) else { return Vec::new() };
        let mut out: Vec<Listing> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let meta: Meta = serde_json::from_slice(&std::fs::read(e.path().join("meta.json")).ok()?).ok()?;
                let sentences = self.transcript(&meta.id).iter().filter(|x| x.kind == Kind::Speech).count();
                let has_summary = e.path().join("summary.json").is_file();
                Some(Listing { meta, sentences, has_summary })
            })
            .collect();
        out.sort_by(|a, b| b.meta.started_at.cmp(&a.meta.started_at));
        out
    }

    pub fn delete(&self, id: &str) -> std::io::Result<bool> {
        match self.dir(id) {
            Some(d) => std::fs::remove_dir_all(d).map(|_| true),
            None => Ok(false),
        }
    }

    /// Remove the audio of meetings that ended more than `days` ago; the text
    /// stays. Returns the ids purged.
    pub fn purge_audio(&self, days: u32, now: chrono::DateTime<chrono::Utc>) -> Vec<String> {
        let cutoff = now - chrono::Duration::days(i64::from(days));
        let mut purged = Vec::new();
        for l in self.list() {
            let mut meta = l.meta;
            if meta.audio_purged_at.is_some() || meta.status == "live" {
                continue;
            }
            let ended = meta
                .ended_at
                .as_deref()
                .unwrap_or(&meta.started_at)
                .parse::<chrono::DateTime<chrono::Utc>>();
            let Ok(ended) = ended else { continue };
            if ended > cutoff {
                continue;
            }
            let Some(dir) = self.dir(&meta.id) else { continue };
            let _ = std::fs::remove_dir_all(dir.join("audio"));
            meta.audio_purged_at = Some(now.to_rfc3339());
            if self.put_meta(&meta).is_ok() {
                purged.push(meta.id);
            }
        }
        purged
    }
}

/// Ids are the pendant's u64 in decimal; anything else could walk out of
/// the store through the directory name.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn not_found() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, "no such meeting")
}

/// Written to a temporary file and renamed, so a reader never sees half.
fn write_json(path: &Path, v: &impl Serialize) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str, started: &str) -> Meta {
        Meta { id: id.into(), started_at: started.into(), status: "live".into(), ..Default::default() }
    }

    fn speech(seg: u32, offset_ms: u64, raw: &str) -> Entry {
        Entry {
            seg,
            offset_ms,
            dur_ms: 1000,
            kind: Kind::Speech,
            who: "other".into(),
            raw: raw.into(),
            clean: None,
            clean_failed: false,
            late: false,
            who_fixed: false,
        }
    }

    #[test]
    fn create_is_idempotent_and_ids_are_checked() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::new(tmp.path());
        let a = s.create(&meta("42", "2026-11-06T06:30:05Z")).unwrap();
        let b = s.create(&meta("42", "2026-11-06T07:00:00Z")).unwrap();
        assert_eq!(a, b);
        assert!(s.create(&meta("../x", "2026-11-06T06:30:05Z")).is_err());
        assert!(s.dir("../x").is_none());
        assert_eq!(s.meta("42").unwrap().started_at, "2026-11-06T06:30:05Z");
    }

    #[test]
    fn transcript_folds_later_states_and_orders_by_time() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::new(tmp.path());
        s.create(&meta("7", "2026-11-06T06:30:05Z")).unwrap();
        s.put_entry("7", &speech(1, 5000, "嗯那个预算")).unwrap();
        s.put_entry("7", &speech(0, 1000, "开始吧")).unwrap();
        let mut cleaned = speech(1, 5000, "嗯那个预算");
        cleaned.clean = Some("预算".into());
        s.put_entry("7", &cleaned).unwrap();
        // A torn line from a crash.
        let dir = s.dir("7").unwrap();
        let mut f = std::fs::OpenOptions::new().append(true).open(dir.join("transcript.jsonl")).unwrap();
        f.write_all(b"{\"seg\":2,\"offs").unwrap();

        let t = s.transcript("7");
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].raw, "开始吧");
        assert_eq!(t[1].clean.as_deref(), Some("预算"));
        assert_eq!(s.list()[0].sentences, 2);
    }

    #[test]
    fn purge_keeps_text_and_recent_audio() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::new(tmp.path());
        for (id, ended) in [("1", "2026-09-01T00:00:00Z"), ("2", "2026-11-05T00:00:00Z")] {
            let mut m = meta(id, ended);
            m.status = "ended".into();
            m.ended_at = Some(ended.into());
            s.create(&m).unwrap();
            s.put_audio(id, 0, b"OggS").unwrap();
            s.put_entry(id, &speech(0, 0, "hi")).unwrap();
        }
        let now = "2026-11-06T00:00:00Z".parse().unwrap();
        assert_eq!(s.purge_audio(30, now), vec!["1".to_string()]);
        assert!(s.audio_path("1", 0).is_none());
        assert!(s.audio_path("2", 0).is_some());
        assert_eq!(s.transcript("1").len(), 1);
        assert!(s.meta("1").unwrap().audio_purged_at.is_some());
        assert!(s.purge_audio(30, now).is_empty());
    }

    #[test]
    fn delete_removes_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::new(tmp.path());
        s.create(&meta("9", "2026-11-06T06:30:05Z")).unwrap();
        assert!(s.delete("9").unwrap());
        assert!(!s.delete("9").unwrap());
        assert!(s.list().is_empty());
    }
}
