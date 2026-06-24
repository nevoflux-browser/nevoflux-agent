//! Daemon-side normalization of recording `step` objects (design §4.4).
//! Defense-in-depth: secrets are also redacted at the source (Plan 2), but the
//! sink guarantees they never reach disk.

use serde_json::Value;

/// `e0`, `e7`, … per-snapshot ids that must never persist.
fn is_ephemeral_selector(sel: &Value) -> bool {
    let strategy = sel.get("strategy").and_then(Value::as_str).unwrap_or("");
    if strategy == "snapshot" || strategy == "data-ai-id" {
        return true;
    }
    let val = sel.get("value").and_then(Value::as_str).unwrap_or("");
    val.contains("data-ai-id")
        || (val.starts_with('e') && val.len() > 1 && val[1..].chars().all(|c| c.is_ascii_digit()))
}

/// Normalize one `step` object in place. No-op for header objects.
pub fn normalize_step(value: &mut Value) {
    // (a) redacted → null value
    if value.get("redacted").and_then(Value::as_bool) == Some(true) {
        value["value"] = Value::Null;
    }
    // (b) file input → placeholder
    let is_file = value
        .get("target")
        .and_then(|t| t.get("element_kind"))
        .and_then(Value::as_str)
        == Some("file");
    if is_file {
        value["value"] = Value::String("{{file}}".to_string());
    }
    // (c) drop ephemeral selectors
    if let Some(sels) = value
        .get_mut("target")
        .and_then(|t| t.get_mut("selectors"))
        .and_then(Value::as_array_mut)
    {
        sels.retain(|s| !is_ephemeral_selector(s));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacted_step_value_becomes_null() {
        let mut v = json!({"type":"step","value":"hunter2","redacted":true});
        normalize_step(&mut v);
        assert!(v["value"].is_null());
    }

    #[test]
    fn file_kind_value_forced_to_placeholder() {
        let mut v = json!({"type":"step","value":"C:/secret.pdf",
            "target":{"element_kind":"file"}});
        normalize_step(&mut v);
        assert_eq!(v["value"], json!("{{file}}"));
    }

    #[test]
    fn strips_ephemeral_selectors() {
        let mut v = json!({"type":"step","target":{"selectors":[
            {"type":"css","strategy":"id","value":"#email"},
            {"type":"css","strategy":"snapshot","value":"e7"},
            {"type":"attr","strategy":"data-ai-id","value":"data-ai-id=42"}
        ]}});
        normalize_step(&mut v);
        let sels = v["target"]["selectors"].as_array().unwrap();
        assert_eq!(sels.len(), 1);
        assert_eq!(sels[0]["value"], json!("#email"));
    }
}
