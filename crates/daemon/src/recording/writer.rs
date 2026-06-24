//! Single-file NDJSON writer for one recording. Append + per-line `sync_all`
//! (lossless, crash-recoverable to the last complete line). Design §4.5.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use serde_json::Value;

use super::normalize_step;

pub struct RecordingWriter {
    file: File,
    next_i: u64,
}

impl RecordingWriter {
    pub fn open(dir: &Path, recording_id: &str) -> std::io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("{recording_id}.jsonl"));
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file, next_i: 1 })
    }

    pub fn write_line(&mut self, mut value: Value) -> std::io::Result<()> {
        let is_step = value.get("type").and_then(Value::as_str) == Some("step");
        if is_step {
            value["i"] = Value::from(self.next_i);
            self.next_i += 1;
            normalize_step(&mut value);
        }
        let line = serde_json::to_string(&value).map_err(std::io::Error::other)?;
        writeln!(self.file, "{line}")?;
        self.file.sync_all()?; // lossless: every step durable before we ack the next
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp() -> std::path::PathBuf {
        let mut d = std::env::temp_dir();
        // unique-ish without rand/now: PID + static atomic counter avoids
        // collisions both within a single test binary (parallel tests) and
        // across repeated `cargo test` invocations (which restart N at 0).
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        d.push(format!(
            "rec_test_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        d
    }

    #[test]
    fn writes_header_then_indexed_steps() {
        let dir = tmp();
        let mut w = RecordingWriter::open(&dir, "rec_x").unwrap();
        w.write_line(json!({"type":"header","recording_id":"rec_x"})).unwrap();
        w.write_line(json!({"type":"step","action":"click"})).unwrap();
        w.write_line(json!({"type":"step","action":"fill","value":"a"})).unwrap();

        let content = std::fs::read_to_string(dir.join("rec_x.jsonl")).unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 3);

        let h: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(h["type"], "header");
        assert!(h.get("i").is_none(), "header must not get an i");

        let s1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        let s2: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(s1["i"], json!(1));
        assert_eq!(s2["i"], json!(2));
    }

    #[test]
    fn redacted_step_persists_null_value() {
        let dir = tmp();
        let mut w = RecordingWriter::open(&dir, "rec_y").unwrap();
        w.write_line(json!({"type":"step","value":"hunter2","redacted":true})).unwrap();
        let content = std::fs::read_to_string(dir.join("rec_y.jsonl")).unwrap();
        let s: serde_json::Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert!(s["value"].is_null());
    }
}
