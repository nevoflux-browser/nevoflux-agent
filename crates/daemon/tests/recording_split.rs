// crates/daemon/tests/recording_split.rs
// Verifies the prefix decision in isolation (mirrors the server.rs branch).
use nevoflux_daemon::recording::{recording_id_from_topic, RecordingCollector};
use serde_json::json;

#[tokio::test]
async fn recording_topic_is_split_to_collector_not_bus() {
    let mut dir = std::env::temp_dir();
    dir.push("rec_split_test");
    let _ = std::fs::remove_dir_all(&dir);
    let collector = RecordingCollector::new(dir.clone());

    let topic = "recording:rec_split";
    // This is exactly the guard server.rs must run before BusEvent construction:
    if let Some(id) = recording_id_from_topic(topic) {
        collector.ingest(id.to_string(), json!({"type":"header"}));
        collector.ingest(id.to_string(), json!({"type":"step","action":"click"}));
    } else {
        panic!("should have matched recording prefix");
    }

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let content = std::fs::read_to_string(dir.join("rec_split.jsonl")).unwrap();
    assert_eq!(content.lines().count(), 2);
}
