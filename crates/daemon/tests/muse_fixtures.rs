//! The daemon reads the same fixtures the Muse client pins (distribution
//! design §8.2). If one of these fails, the two sides disagree about the wire.

use nevoflux_daemon::remote::channel_codec;
use nevoflux_daemon::remote::elicit::{approval_schema, outcome_of, APPROVAL_TIMEOUT, CALL_META_KEY};
use nevoflux_daemon::remote::envelope::{self, InboundVerifier};
use nevoflux_daemon::remote::mcp_tools::{AgentError, Approval, REASON_DECLINED};
use nevoflux_daemon::remote::mcp_server::PROTOCOL;
use nevoflux_daemon::remote::relay_protocol::WireMessage;
use nevoflux_daemon::remote::session::Wire;
use nevoflux_daemon::remote::task::engine::{
    approval_progress, approve_result, cancel_result, progress_message, start_result,
};
use nevoflux_daemon::remote::task::record::{Conclusion, StepEvent, TaskRecord, TaskState};
use nevoflux_daemon::remote::task::tools::{task_tools, wait_result};
use nevoflux_mcp::rmcp::model::{
    CallToolResult, NumberOrString, ProgressNotificationParam, ProgressToken,
};
use nevoflux_mcp::rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use nevoflux_mcp::rmcp::RoleServer;
use nevoflux_protocol::ExecutionTier;
use serde_json::{json, Value};

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/muse");

fn json(name: &str) -> Value {
    let text = std::fs::read_to_string(format!("{DIR}/{name}")).expect("fixture exists");
    serde_json::from_str(&text).expect("fixture is JSON")
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn protocol_version_matches_the_server() {
    let text = std::fs::read_to_string(format!("{DIR}/PROTOCOL_VERSION")).unwrap();
    assert_eq!(text.trim(), PROTOCOL.to_string());
}

#[test]
fn kdf_vectors_match_the_daemon() {
    for v in json("kdf.json")["vectors"].as_array().unwrap() {
        let key = nevoflux_daemon::remote::crypto::derive_channel_key(
            v["code"].as_str().unwrap(),
            v["channel_id"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(key.to_vec(), unhex(v["key_hex"].as_str().unwrap()));
    }
}

#[test]
fn the_daemon_only_prints_canonical_codes() {
    // The daemon never normalizes because it only ever prints the canonical
    // form; this pins that, against the fixture's own alphabet.
    let f = json("pairing_code.json");
    let alphabet = f["alphabet"].as_str().unwrap();
    for _ in 0..50 {
        let code = nevoflux_daemon::share::generate_password();
        let groups: Vec<&str> = code.split('-').collect();
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            vec![1, 4, 4, 4],
            "{code}"
        );
        assert!(
            code.chars()
                .filter(|c| *c != '-')
                .all(|c| alphabet.contains(c)),
            "{code}"
        );
    }
    for v in f["valid"].as_array().unwrap() {
        let canonical = v["canonical"].as_str().unwrap();
        assert_eq!(canonical.len(), 16, "{canonical}");
    }
}

#[test]
fn the_sealed_vector_opens_to_its_plaintext() {
    let f = json("seal.json");
    let key: [u8; 32] = unhex(f["key_hex"].as_str().unwrap()).try_into().unwrap();
    let wire = Wire::Binary(unhex(f["sealed_hex"].as_str().unwrap()));
    match channel_codec::decode(Some(&key), &wire) {
        Some(WireMessage::Frame { seq: None, frame }) => {
            assert_eq!(frame, f["plaintext"]["frame"]);
        }
        other => panic!("did not open as a seq-less frame: {other:?}"),
    }
}

#[test]
fn envelope_cases_match_the_verifier() {
    let f = json("envelope.json");
    let challenge = f["challenge"].as_str().unwrap();
    for case in f["cases"].as_array().unwrap() {
        let mut v = InboundVerifier::new(challenge.to_string());
        for p in case["prior"].as_array().unwrap() {
            v.accept(p).expect("prior frames are accepted");
        }
        let got = match v.accept(&case["frame"]) {
            Ok(_) => "accept".to_string(),
            Err(r) => format!("reject:{}", r.as_str()),
        };
        assert_eq!(
            got,
            case["expect"].as_str().unwrap(),
            "case {}",
            case["name"]
        );
    }
}

#[test]
fn the_refusal_budget_matches_the_verifier() {
    assert_eq!(
        json("envelope.json")["bad_frame_limit"].as_u64(),
        Some(u64::from(envelope::BAD_FRAME_LIMIT))
    );
}

#[test]
fn example_requests_are_valid_json_rpc_for_rmcp() {
    for r in json("mcp_messages.json")["requests"].as_array().unwrap() {
        serde_json::from_value::<RxJsonRpcMessage<RoleServer>>(r.clone())
            .unwrap_or_else(|e| panic!("{r} is not a server-bound message: {e}"));
    }
}

/// Each fixture error is what the head really sends, byte for byte in
/// `structuredContent`, and its text is the same object.
#[test]
fn the_tool_error_fixture_is_what_the_head_sends() {
    let f = json("tool_errors.json");
    let cases: Vec<(&str, &str, AgentError)> = vec![
        ("unknown_tool", "bash", AgentError::UnknownTool),
        (
            "tier_denied",
            "browser_click",
            AgentError::TierDenied {
                tier: ExecutionTier::ReadOnly,
                required: ExecutionTier::BrowserAuto,
            },
        ),
        (
            "approval_denied",
            "browser_click",
            AgentError::ApprovalDenied {
                reason: REASON_DECLINED,
                message: "the person declined browser_click".into(),
            },
        ),
        ("approval_timeout", "browser_click", AgentError::ApprovalTimeout),
        ("browser_unavailable", "browser_click", AgentError::BrowserUnavailable),
        ("no_agent_tab", "browser_click", AgentError::NoAgentTab),
        ("tab_closed", "browser_click", AgentError::TabClosed { tab_id: 7 }),
        (
            "tool_failed",
            "browser_click",
            AgentError::ToolFailed {
                detail: "-1: Element not found".into(),
            },
        ),
        ("task_busy", "nevoflux_task_start", AgentError::TaskBusy { task_id: TASK.into() }),
        ("rate_limited", "nevoflux_task_start", AgentError::RateLimited { retry_after: 1234 }),
        ("session_not_found", "nevoflux_task_start", AgentError::SessionNotFound { session: SESSION.into() }),
        ("task_not_found", "nevoflux_task_wait", AgentError::TaskNotFound { task_id: TASK.into() }),
        ("approval_not_found", "nevoflux_task_approve", AgentError::ApprovalNotFound { approval_id: APPROVAL.into() }),
        ("paused", "nevoflux_task_start", AgentError::Paused),
        ("approval_desktop_only", "nevoflux_task_approve", AgentError::ApprovalDesktopOnly { approval_id: GUARDED.into() }),
    ];
    let errors = f["errors"].as_object().unwrap();
    assert_eq!(errors.len(), cases.len(), "one example per code");
    for (code, tool, error) in cases {
        let want = &errors[code];
        let got = serde_json::to_value(error.to_result(tool)).unwrap();
        assert_eq!(got["isError"], want["isError"], "{code}");
        assert_eq!(got["structuredContent"], want["structuredContent"], "{code}");
        let text: Value =
            serde_json::from_str(got["content"][0]["text"].as_str().unwrap()).unwrap();
        let want_text: Value =
            serde_json::from_str(want["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, want_text, "{code}");
        assert_eq!(
            text, want["structuredContent"],
            "{code}: text and structuredContent agree"
        );
    }
}

#[test]
fn the_approval_fixture_is_what_the_head_asks_and_understands() {
    let f = json("tool_errors.json");
    assert_eq!(f["call_meta_key"], CALL_META_KEY);
    assert_eq!(f["approval_timeout_seconds"], APPROVAL_TIMEOUT.as_secs());
    let request = &f["approval_request"];
    assert_eq!(request["method"], "elicitation/create");
    assert_eq!(request["params"]["mode"], "form");
    assert_eq!(
        request["params"]["requestedSchema"],
        serde_json::to_value(approval_schema()).unwrap()
    );
    assert!(request["params"]["_meta"][CALL_META_KEY].is_string());
    let approve: nevoflux_mcp::rmcp::model::ElicitResult =
        serde_json::from_value(f["approve"].clone()).unwrap();
    let deny: nevoflux_mcp::rmcp::model::ElicitResult =
        serde_json::from_value(f["deny"].clone()).unwrap();
    assert_eq!(outcome_of(&approve), Approval::Approved);
    assert_eq!(outcome_of(&deny), Approval::Declined);
}

#[test]
fn an_off_whitelist_call_is_answered_as_the_messages_fixture_says() {
    let messages = json("mcp_messages.json");
    let want = messages["expected"]["unknown_tool_result"].clone();
    let got = serde_json::to_value(AgentError::UnknownTool.to_result("bash")).unwrap();
    assert_eq!(got["isError"], want["isError"]);
    assert_eq!(got["structuredContent"], want["structuredContent"]);
    let text: Value = serde_json::from_str(got["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, want["structuredContent"], "text is the same object");
    for gone in ["not_allowed_error", "unavailable_error"] {
        assert!(messages["expected"].get(gone).is_none(), "{gone}");
    }
}

/// `task_busy` has a second form (contract v1.1 §1.1, R38): the desktop's ACP
/// provider is busy. The `errors` map keeps one entry per code, so this one
/// is a sibling of it.
#[test]
fn the_provider_form_of_task_busy_is_what_the_head_sends() {
    let f = json("tool_errors.json");
    let want = &f["task_busy_provider"];
    let got = serde_json::to_value(
        AgentError::ProviderBusy {
            provider: "ClaudeCode".into(),
        }
        .to_result("nevoflux_task_start"),
    )
    .unwrap();
    assert_eq!(got["isError"], want["isError"]);
    assert_eq!(got["structuredContent"], want["structuredContent"]);
    let text: Value = serde_json::from_str(got["content"][0]["text"].as_str().unwrap()).unwrap();
    let want_text: Value =
        serde_json::from_str(want["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, want_text);
    assert_eq!(text, want["structuredContent"], "text and structuredContent agree");
    assert!(want["structuredContent"]["error"].get("task_id").is_none());
}

#[test]
fn the_task_tool_fixture_is_what_tools_list_shows() {
    let f = json("task_tools.json");
    let want = f["tools"].as_array().unwrap();
    let got = task_tools();
    assert_eq!(got.len(), want.len());
    for (tool, want) in got.iter().zip(want) {
        let v = serde_json::to_value(tool).unwrap();
        assert_eq!(v["name"], want["name"]);
        assert_eq!(v["description"], want["description"], "{}", tool.name);
        assert_eq!(v["inputSchema"], want["inputSchema"], "{}", tool.name);
    }
}

#[test]
fn every_task_tool_has_a_request_example_that_fits_its_schema() {
    let f = json("task_tools.json");
    let tools: Vec<Value> = task_tools()
        .iter()
        .map(|t| serde_json::to_value(t).unwrap())
        .collect();
    let mut covered = std::collections::BTreeSet::new();
    for req in f["requests"].as_array().unwrap() {
        serde_json::from_value::<RxJsonRpcMessage<RoleServer>>(req.clone())
            .unwrap_or_else(|e| panic!("{req} is not a server-bound message: {e}"));
        assert_eq!(req["method"], "tools/call");
        let name = req["params"]["name"].as_str().unwrap();
        let tool = tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{name}"));
        let schema = &tool["inputSchema"];
        let args = req["params"]["arguments"].as_object().unwrap();
        for key in schema["required"].as_array().unwrap() {
            assert!(args.contains_key(key.as_str().unwrap()), "{name} lacks {key}");
        }
        for (key, value) in args {
            let property = schema["properties"]
                .get(key)
                .unwrap_or_else(|| panic!("{name}: unknown {key}"));
            match property["type"].as_str().unwrap() {
                "string" => assert!(value.is_string(), "{name}.{key}"),
                "integer" => {
                    let n = value.as_i64().unwrap_or_else(|| panic!("{name}.{key}"));
                    assert!(n >= property["minimum"].as_i64().unwrap(), "{name}.{key}");
                    assert!(n <= property["maximum"].as_i64().unwrap(), "{name}.{key}");
                }
                other => panic!("{name}.{key}: unexpected type {other}"),
            }
            if let Some(allowed) = property["enum"].as_array() {
                assert!(allowed.contains(value), "{name}.{key}");
            }
        }
        covered.insert(name.to_string());
    }
    assert_eq!(covered.len(), tools.len(), "one example per tool");
}

const TASK: &str = "3f2b8c1e-7d4a-4e9b-8a61-2c5d9e0f1a37";
const SESSION: &str = "9c4e1a2b-5f6d-4c3e-b8a7-1d2e3f4a5b6c";
const APPROVAL: &str = "5d0c8e2a-3b1f-4a7e-9c6d-8e2f1a0b3c4d";
const GUARDED: &str = "7a6b5c4d-3e2f-4a1b-8c9d-0e1f2a3b4c5d";
const JPEG: &str = "/9j/4AAQSkZJRgABAQ==";
/// 2026-10-14T17:46:40Z.
const T0: i64 = 1_792_000_000;

fn chunk(session: &str, event: Value) -> Value {
    json!({"type": "stream_chunk", "payload": {"session_id": session, "content": "", "done": false, "event": event}})
}

fn say(session: &str, text: &str) -> Value {
    json!({"type": "stream_chunk", "payload": {"session_id": session, "content": text, "done": false}})
}

/// One wait entry is the real `tools/call` result: `structuredContent`,
/// `content[0]` the same object as JSON, `content[1]` the image or nothing.
fn check(f: &Value, name: &str, result: CallToolResult) {
    let want = &f[name];
    assert!(want.is_object(), "{name} is in the fixture");
    let got = serde_json::to_value(&result).unwrap();
    assert_eq!(got["isError"], want["isError"], "{name}");
    assert_eq!(got["structuredContent"], want["structuredContent"], "{name}");
    let text: Value = serde_json::from_str(got["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, want["structuredContent"], "{name}: content[0] is the same object");
    assert_eq!(
        got["content"].get(1).cloned().unwrap_or(Value::Null),
        want["image"],
        "{name}"
    );
    assert!(got["content"].get(2).is_none(), "{name}");
}

fn plan_id() -> String {
    "0e9d8c7b-6a5f-4e3d-8c2b-1a0f9e8d7c6b".to_string()
}

#[test]
fn the_task_result_fixture_is_what_the_head_sends() {
    let f = json("task_results.json");
    assert_eq!(f["start"], start_result(TASK, SESSION));
    assert_eq!(f["approve"], approve_result(TaskState::Running));
    assert_eq!(f["cancel"], cancel_result(TASK, TaskState::Cancelled));

    let mut a = TaskRecord::new(TASK, "chan-1", SESSION, "Generate a calculator Canvas", "sidebar-1", T0);
    a.observe(&chunk(SESSION, json!({"type": "tool_start", "tool_id": "toolu_01", "tool_name": "skill_load", "icon": "", "summary": ""})), T0 + 5);
    a.observe(&chunk(SESSION, json!({"type": "tool_end", "tool_id": "toolu_01", "status": "success", "duration_ms": 4000, "summary": ""})), T0 + 9);
    a.observe(&chunk(SESSION, json!({"type": "tool_start", "tool_id": "toolu_02", "tool_name": "create_artifact", "icon": "", "summary": ""})), T0 + 12);
    check(&f, "wait_running", wait_result(a.wait_json(0, T0 + 15), None));

    a.observe(&json!({"type": "browser_tool_request", "payload": {"request_id": APPROVAL, "session_id": SESSION,
        "tab_id": null, "action": "ask_user", "timeout_ms": 86400000, "params": {
            "question": "AI wants to perform an action:\n\nCreate a Canvas artifact: \"Calculator\"\n\nDo you want to allow this?",
            "description": "Create a Canvas artifact: \"Calculator\"",
            "options": ["Allow", "Always allow this type of action", "Deny"],
            "allow_custom": false, "timeout_ms": 86400000, "purpose": "permission"}}}), T0 + 20);
    let approval = a.approvals[0].clone();
    check(&f, "wait_awaiting_approval", wait_result(a.wait_json(3, T0 + 80), None));

    a.observe(&json!({"type": "browser_tool_resolved", "payload": {"request_id": APPROVAL, "session_id": SESSION}}), T0 + 85);
    a.observe(&chunk(SESSION, json!({"type": "tool_end", "tool_id": "toolu_02", "status": "success", "duration_ms": 5000, "summary": ""})), T0 + 90);
    a.observe(&say(SESSION, "Here is your calculator."), T0 + 95);
    a.observe(&json!({"type": "artifact_start", "payload": {"session_id": SESSION, "id": "calc-1", "title": "Calculator", "content_type": "text/html"}}), T0 + 96);
    assert_eq!(a.conclude(true, false, T0 + 100), Conclusion::Capture);
    a.finish(Ok(JPEG.to_string()), T0 + 100);
    check(&f, "wait_done_with_image", wait_result(a.wait_json(3, T0 + 101), a.screenshot.clone()));
    check(&f, "wait_done_again", wait_result(a.wait_json(4, T0 + 102), None));

    let steps: Vec<String> = a.events_since(0).0.into_iter().map(progress_message).collect();
    assert_eq!(f["progress_messages"]["steps"], json!(steps));
    assert_eq!(f["progress_messages"]["approval"], json!(approval_progress(&approval)));
    let failed_step = StepEvent { seq: 0, id: "x".into(), at: T0, tool: "browser_click".into(), status: "failed" };
    assert_eq!(f["progress_messages"]["failed_step_example"], json!(progress_message(&failed_step)));

    // The example frame is what the head's progress sink sends for the first
    // step of a wait (mcp_server.rs `PeerProgress`), and rmcp reads it back.
    let frame = &f["progress_notification"];
    serde_json::from_value::<TxJsonRpcMessage<RoleServer>>(frame.clone())
        .unwrap_or_else(|e| panic!("{frame} is not a server-sent message: {e}"));
    assert_eq!(frame["method"], "notifications/progress");
    let first = a.events_since(0).0[0].clone();
    let sent = ProgressNotificationParam::new(ProgressToken(NumberOrString::String("wait-1".into())), 1.0)
        .with_message(progress_message(&first));
    let read: ProgressNotificationParam = serde_json::from_value(frame["params"].clone()).unwrap();
    assert_eq!(read, sent);
    assert!(read.total.is_none(), "no total (contract §2.2)");
    assert_eq!(frame["params"], serde_json::to_value(&sent).unwrap());

    let mut b = TaskRecord::new("6b7c8d9e-0f1a-4b2c-8d3e-4f5a6b7c8d9e", "chan-1", "2a3b4c5d-6e7f-4a8b-9c0d-1e2f3a4b5c6d",
                                "Ask me first", "sidebar-1", T0).with_minter(plan_id);
    b.observe(&json!({"type": "browser_tool_request", "payload": {"request_id": "8f7e6d5c-4b3a-4c2d-9e1f-0a9b8c7d6e5f",
        "session_id": "2a3b4c5d-6e7f-4a8b-9c0d-1e2f3a4b5c6d", "tab_id": null, "action": "ask_user", "timeout_ms": 30000,
        "params": {"question": "Which colour should the buttons be?", "options": ["Blue", "Green"], "allow_custom": true, "timeout_ms": 1800000}}}), T0 + 30);
    b.observe(&json!({"type": "plan_proposal", "payload": {"session_id": "2a3b4c5d-6e7f-4a8b-9c0d-1e2f3a4b5c6d",
        "summary": "Build the calculator", "steps": [{"description": "Write the HTML"}, {"description": "Render the Canvas"}]}}), T0 + 40);
    check(&f, "wait_question_and_plan", wait_result(b.wait_json(0, T0 + 60), None));

    let mut e = TaskRecord::new(TASK, "chan-1", SESSION, "Schedule it", "sidebar-1", T0);
    let rule = "A task delegated by a paired agent wants to run schedule_create. This action always needs approval.";
    e.observe(&json!({"type": "browser_tool_request", "payload": {"request_id": GUARDED,
        "session_id": SESSION, "tab_id": null, "action": "ask_user", "timeout_ms": 86400000, "params": {
            "question": rule, "description": rule, "options": ["Allow", "Deny"], "allow_custom": false,
            "timeout_ms": 86400000, "purpose": "permission", "must_approve": true}}}), T0 + 50);
    check(&f, "wait_desktop_only", wait_result(e.wait_json(0, T0 + 60), None));
    // Contract v1.2 R39: the same approval while the pairing lets the agent answer.
    check(&f, "wait_guarded_approval", wait_result(e.wait_json_for(0, T0 + 60, true), None));

    let mut c = TaskRecord::new(TASK, "chan-1", SESSION, "Say done", "sidebar-1", T0);
    c.observe(&say(SESSION, "Done."), T0 + 1);
    assert_eq!(c.conclude(true, false, T0 + 2), Conclusion::Capture);
    c.finish(Err("no_tab".to_string()), T0 + 2);
    check(&f, "wait_done_screenshot_error", wait_result(c.wait_json(0, T0 + 3), None));

    let mut d = TaskRecord::new(TASK, "chan-1", SESSION, "Fail", "sidebar-1", T0);
    d.observe(&json!({"type": "error", "payload": {"session_id": SESSION, "code": "AGENT_ERROR", "message": "Agent error: rate limited"}}), T0 + 1);
    assert_eq!(d.conclude(true, false, T0 + 2), Conclusion::Ended);
    check(&f, "wait_failed", wait_result(d.wait_json(0, T0 + 3), None));

    let mut x = TaskRecord::new(TASK, "chan-1", SESSION, "Stop", "sidebar-1", T0);
    x.cancel(T0 + 1);
    check(&f, "wait_cancelled", wait_result(x.wait_json(0, T0 + 2), None));
}
