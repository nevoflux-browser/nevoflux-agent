//! Lossless NDJSON recording sink (design §4.3–§4.5). Mirrors `trace/` but
//! never routes through the EventBus.

mod normalize;
pub use normalize::normalize_step;

mod writer;
pub use writer::RecordingWriter;

pub const RECORDING_TOPIC_PREFIX: &str = "recording:";

/// Extract `<recording_id>` from a `recording:<recording_id>` topic.
/// Returns `None` for non-recording topics or an empty id.
pub fn recording_id_from_topic(topic: &str) -> Option<&str> {
    let id = topic.strip_prefix(RECORDING_TOPIC_PREFIX)?;
    if id.is_empty() { None } else { Some(id) }
}

use std::collections::HashMap;
use std::path::PathBuf;
use serde_json::Value;
use tokio::sync::mpsc;

struct IngestMsg {
    recording_id: String,
    line: Value,
}

/// Cloneable handle to the single recording writer task.
#[derive(Clone)]
pub struct RecordingCollector {
    tx: mpsc::UnboundedSender<IngestMsg>,
}

impl RecordingCollector {
    pub fn new(recordings_dir: PathBuf) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<IngestMsg>();
        tokio::task::spawn_blocking(move || {
            // One OS thread owns all recording files → ordering + fsync isolation.
            let mut writers: HashMap<String, RecordingWriter> = HashMap::new();
            while let Some(msg) = rx.blocking_recv() {
                let writer = match writers.get_mut(&msg.recording_id) {
                    Some(w) => w,
                    None => match RecordingWriter::open(&recordings_dir, &msg.recording_id) {
                        Ok(w) => writers.entry(msg.recording_id.clone()).or_insert(w),
                        Err(e) => {
                            tracing::warn!(recording_id = %msg.recording_id, error = %e,
                                "recording: failed to open writer");
                            continue;
                        }
                    },
                };
                if let Err(e) = writer.write_line(msg.line) {
                    tracing::warn!(recording_id = %msg.recording_id, error = %e,
                        "recording: dropped a line on write error");
                }
            }
        });
        Self { tx }
    }

    /// Fire-and-forget enqueue. Never blocks the daemon's publish handler.
    pub fn ingest(&self, recording_id: String, line: Value) {
        if self.tx.send(IngestMsg { recording_id, line }).is_err() {
            tracing::warn!("recording: writer task gone, dropping line");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_recording_id_from_topic() {
        assert_eq!(recording_id_from_topic("recording:rec_abc123"), Some("rec_abc123"));
    }

    #[test]
    fn rejects_non_recording_and_empty() {
        assert_eq!(recording_id_from_topic("ui:tab:dom"), None);
        assert_eq!(recording_id_from_topic("recording:"), None);
    }

    #[tokio::test]
    async fn collector_appends_lines_for_a_recording() {
        use serde_json::json;
        let mut dir = std::env::temp_dir();
        dir.push("rec_collector_test");
        let _ = std::fs::remove_dir_all(&dir);

        let collector = RecordingCollector::new(dir.clone());
        collector.ingest("rec_z".into(), json!({"type":"header","recording_id":"rec_z"}));
        collector.ingest("rec_z".into(), json!({"type":"step","action":"click"}));

        // allow the writer task to drain
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let content = std::fs::read_to_string(dir.join("rec_z.jsonl")).unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let s: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(s["i"], json!(1));
    }
}
