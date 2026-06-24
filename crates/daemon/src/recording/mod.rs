//! Lossless NDJSON recording sink (design §4.3–§4.5). Mirrors `trace/` but
//! never routes through the EventBus.

pub const RECORDING_TOPIC_PREFIX: &str = "recording:";

/// Extract `<recording_id>` from a `recording:<recording_id>` topic.
/// Returns `None` for non-recording topics or an empty id.
pub fn recording_id_from_topic(topic: &str) -> Option<&str> {
    let id = topic.strip_prefix(RECORDING_TOPIC_PREFIX)?;
    if id.is_empty() { None } else { Some(id) }
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
}
