//! Canvas Tool protocol message types.
//!
//! Defines wire types for canvas tool invocation requests/responses,
//! tool listing, and streaming execution events.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Request to invoke a canvas tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanvasToolInvokeRequest {
    pub tool_name: String,
    #[serde(default)]
    pub params: HashMap<String, String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    pub session_id: String,
}

/// Response from a canvas tool invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanvasToolInvokeResponse {
    pub tool_name: String,
    pub success: bool,
    #[serde(default)]
    pub stdout: Option<String>,
    #[serde(default)]
    pub stderr: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub error: Option<String>,
    pub duration_ms: u64,
    pub invocation_id: String,
}

/// Request to list available tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanvasToolListRequest {
    #[serde(default)]
    pub include_disabled: bool,
}

/// A tool summary in a list response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanvasToolSummary {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub kind: String,
    pub args_mode: Option<String>,
    pub enabled: bool,
    pub source: String,
}

/// Response listing available tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanvasToolListResponse {
    pub tools: Vec<CanvasToolSummary>,
}

/// Canvas tool event (streamed during execution).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CanvasToolEvent {
    Started {
        invocation_id: String,
        tool_name: String,
    },
    Output {
        invocation_id: String,
        stream: String,
        data: String,
    },
    Completed {
        invocation_id: String,
        success: bool,
        duration_ms: u64,
    },
    Error {
        invocation_id: String,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    #[test]
    fn test_invoke_request_roundtrip() {
        let mut params = HashMap::new();
        params.insert("file".to_string(), "main.rs".to_string());
        params.insert("line".to_string(), "42".to_string());

        let req = CanvasToolInvokeRequest {
            tool_name: "run_tests".to_string(),
            params,
            args: Some(vec!["--verbose".to_string(), "--nocapture".to_string()]),
            session_id: "sess-abc123".to_string(),
        };

        let json = serde_json::to_string(&req).unwrap();
        let decoded: CanvasToolInvokeRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_invoke_request_defaults() {
        let json = r#"{"tool_name":"lint","session_id":"s1"}"#;
        let req: CanvasToolInvokeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.tool_name, "lint");
        assert!(req.params.is_empty());
        assert!(req.args.is_none());
        assert_eq!(req.session_id, "s1");
    }

    #[test]
    fn test_invoke_response_roundtrip() {
        let resp = CanvasToolInvokeResponse {
            tool_name: "cargo_test".to_string(),
            success: true,
            stdout: Some("test result: ok".to_string()),
            stderr: None,
            exit_code: Some(0),
            error: None,
            duration_ms: 1500,
            invocation_id: "inv-001".to_string(),
        };

        let json = serde_json::to_string(&resp).unwrap();
        let decoded: CanvasToolInvokeResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, decoded);
    }

    #[test]
    fn test_invoke_response_failure() {
        let resp = CanvasToolInvokeResponse {
            tool_name: "build".to_string(),
            success: false,
            stdout: None,
            stderr: Some("error[E0308]: mismatched types".to_string()),
            exit_code: Some(1),
            error: Some("Compilation failed".to_string()),
            duration_ms: 3200,
            invocation_id: "inv-002".to_string(),
        };

        let json = serde_json::to_string(&resp).unwrap();
        let decoded: CanvasToolInvokeResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.success, false);
        assert_eq!(decoded.error.as_deref(), Some("Compilation failed"));
        assert_eq!(decoded.exit_code, Some(1));
    }

    #[test]
    fn test_list_request_roundtrip() {
        let req = CanvasToolListRequest {
            include_disabled: true,
        };

        let json = serde_json::to_string(&req).unwrap();
        let decoded: CanvasToolListRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_list_request_default() {
        let json = r#"{}"#;
        let req: CanvasToolListRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.include_disabled, false);
    }

    #[test]
    fn test_list_response_roundtrip() {
        let resp = CanvasToolListResponse {
            tools: vec![
                CanvasToolSummary {
                    name: "cargo_test".to_string(),
                    description: Some("Run cargo tests".to_string()),
                    kind: "shell".to_string(),
                    args_mode: Some("params".to_string()),
                    enabled: true,
                    source: "builtin".to_string(),
                },
                CanvasToolSummary {
                    name: "eslint".to_string(),
                    description: None,
                    kind: "shell".to_string(),
                    args_mode: None,
                    enabled: false,
                    source: "user".to_string(),
                },
            ],
        };

        let json = serde_json::to_string(&resp).unwrap();
        let decoded: CanvasToolListResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, decoded);
        assert_eq!(decoded.tools.len(), 2);
        assert_eq!(decoded.tools[0].name, "cargo_test");
        assert!(decoded.tools[0].description.is_some());
        assert_eq!(decoded.tools[1].enabled, false);
    }

    #[test]
    fn test_event_started_roundtrip() {
        let event = CanvasToolEvent::Started {
            invocation_id: "inv-100".to_string(),
            tool_name: "build".to_string(),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""event":"started"#));
        let decoded: CanvasToolEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, decoded);
    }

    #[test]
    fn test_event_output_roundtrip() {
        let event = CanvasToolEvent::Output {
            invocation_id: "inv-100".to_string(),
            stream: "stdout".to_string(),
            data: "Compiling nevoflux v0.1.0\n".to_string(),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""event":"output"#));
        let decoded: CanvasToolEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, decoded);
    }

    #[test]
    fn test_event_completed_roundtrip() {
        let event = CanvasToolEvent::Completed {
            invocation_id: "inv-100".to_string(),
            success: true,
            duration_ms: 5432,
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""event":"completed"#));
        let decoded: CanvasToolEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, decoded);
    }

    #[test]
    fn test_event_error_roundtrip() {
        let event = CanvasToolEvent::Error {
            invocation_id: "inv-100".to_string(),
            message: "Process killed by signal 9".to_string(),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""event":"error"#));
        let decoded: CanvasToolEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, decoded);
    }

    #[test]
    fn test_event_tag_based_deserialization() {
        // Verify that the serde tag-based dispatch works correctly
        let started_json = r#"{"event":"started","invocation_id":"x","tool_name":"t"}"#;
        let output_json =
            r#"{"event":"output","invocation_id":"x","stream":"stderr","data":"warn"}"#;
        let completed_json =
            r#"{"event":"completed","invocation_id":"x","success":false,"duration_ms":0}"#;
        let error_json = r#"{"event":"error","invocation_id":"x","message":"boom"}"#;

        let started: CanvasToolEvent = serde_json::from_str(started_json).unwrap();
        let output: CanvasToolEvent = serde_json::from_str(output_json).unwrap();
        let completed: CanvasToolEvent = serde_json::from_str(completed_json).unwrap();
        let error: CanvasToolEvent = serde_json::from_str(error_json).unwrap();

        assert!(matches!(started, CanvasToolEvent::Started { .. }));
        assert!(matches!(output, CanvasToolEvent::Output { .. }));
        assert!(matches!(completed, CanvasToolEvent::Completed { .. }));
        assert!(matches!(error, CanvasToolEvent::Error { .. }));
    }
}
