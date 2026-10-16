//! The four delegation tools on the agent channel (M3 §3, contract §1).
//!
//! Argument checks, schemas and result shapes live here; the work is the
//! engine's. A result never exceeds `MCP_RESULT_BUDGET`: the relay drops a
//! socket whose message is too large without a word, so the image is given up
//! first (`screenshot_error: "too_large"`), then the reply is cut.

use std::time::Duration;

use nevoflux_mcp::rmcp::model::{CallToolResult, ContentBlock, Tool};
use serde_json::{json, Value};

use super::answer::Choice;
use super::engine::{approve_result, Answerer, ApproveError, ProgressSink, TaskEngine};
use super::limits::{
    ANSWER_MAX_CHARS, MCP_RESULT_BUDGET, PROMPT_MAX_CHARS, WAIT_DEFAULT_SECS, WAIT_MAX_SECS,
};
use crate::remote::mcp_tools::AgentError;

pub const TASK_START: &str = "nevoflux_task_start";
pub const TASK_WAIT: &str = "nevoflux_task_wait";
pub const TASK_APPROVE: &str = "nevoflux_task_approve";
pub const TASK_CANCEL: &str = "nevoflux_task_cancel";

fn schema(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

fn tool(
    name: &'static str,
    description: &'static str,
    properties: Value,
    required: &[&str],
) -> Tool {
    Tool::new(
        name,
        description,
        schema(json!({ "type": "object", "properties": properties, "required": required })),
    )
}

/// The schemas `tools/list` shows (contract §1, `fixtures/muse/task_tools.json`).
pub fn task_tools() -> Vec<Tool> {
    vec![
        tool(
            TASK_START,
            "Hand NevoFlux a whole task to run on the desktop: a full agent turn that may use skills, Canvas and every tool this pairing's tier allows. Returns at once with a task_id; follow it with nevoflux_task_wait. Pass session to continue a task session this agent started before.",
            json!({
                "prompt": { "type": "string", "description": "What to do, in plain words (1 to 8000 characters)" },
                "session": { "type": "string", "description": "A session from an earlier nevoflux_task_start of yours, to continue that conversation" }
            }),
            &["prompt"],
        ),
        tool(
            TASK_WAIT,
            "Wait for a delegated task. Returns its state, the tool steps since your last wait, the approvals it is waiting on, and once it is done its reply and one screenshot of what it produced. Returns early when an approval appears or the task ends.",
            json!({
                "task_id": { "type": "string", "description": "From nevoflux_task_start" },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 60, "default": 30, "description": "The longest to wait, in seconds" }
            }),
            &["task_id"],
        ),
        tool(
            TASK_APPROVE,
            "Answer an approval a delegated task is waiting on: allow or deny. For a question, choose allow and put the text in answer. The person at the desktop may answer first; then this answers approval_not_found.",
            json!({
                "task_id": { "type": "string", "description": "The task" },
                "approval_id": { "type": "string", "description": "From the approvals of nevoflux_task_wait" },
                "choice": { "type": "string", "enum": ["allow", "deny"] },
                "answer": { "type": "string", "description": "The answer to a question (at most 4000 characters)" }
            }),
            &["task_id", "approval_id", "choice"],
        ),
        tool(
            TASK_CANCEL,
            "Stop a delegated task. Safe to repeat: a task that already ended keeps its final state.",
            json!({ "task_id": { "type": "string", "description": "The task" } }),
            &["task_id"],
        ),
    ]
}

fn failed(detail: &str) -> AgentError {
    AgentError::ToolFailed {
        detail: detail.to_string(),
    }
}

fn required<'a>(args: &'a Value, field: &str) -> Result<&'a str, AgentError> {
    args.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| failed(&format!("{field} is required")))
}

/// Run one task tool for the paired agent on control channel `pairing`.
/// `mcp_session` names the MCP session asking (step cursor, one image).
pub async fn call(
    engine: &TaskEngine,
    pairing: &str,
    mcp_session: &str,
    name: &str,
    args: Value,
    progress: Option<&dyn ProgressSink>,
) -> CallToolResult {
    let outcome = match name {
        TASK_START => start(engine, pairing, &args)
            .await
            .map(CallToolResult::structured),
        TASK_WAIT => wait(engine, pairing, mcp_session, &args, progress).await,
        TASK_APPROVE => approve(engine, pairing, &args)
            .await
            .map(CallToolResult::structured),
        TASK_CANCEL => match required(&args, "task_id") {
            Ok(task_id) => engine
                .cancel(Some(pairing), task_id)
                .await
                .map(CallToolResult::structured),
            Err(e) => Err(e),
        },
        _ => Err(AgentError::UnknownTool),
    };
    outcome.unwrap_or_else(|e| e.to_result(name))
}

async fn start(engine: &TaskEngine, pairing: &str, args: &Value) -> Result<Value, AgentError> {
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| failed("prompt is required"))?
        .trim();
    let length = prompt.chars().count();
    if length == 0 || length > PROMPT_MAX_CHARS {
        return Err(failed("prompt must be 1 to 8000 characters"));
    }
    let session = args
        .get("session")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    engine.start(pairing, prompt, session).await
}

async fn wait(
    engine: &TaskEngine,
    pairing: &str,
    mcp_session: &str,
    args: &Value,
    progress: Option<&dyn ProgressSink>,
) -> Result<CallToolResult, AgentError> {
    let task_id = required(args, "task_id")?;
    let secs = match args.get("timeout_secs") {
        None | Some(Value::Null) => WAIT_DEFAULT_SECS,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=WAIT_MAX_SECS).contains(n))
            .ok_or_else(|| failed("timeout_secs must be an integer from 1 to 60"))?,
    };
    let waited = engine
        .wait(
            pairing,
            mcp_session,
            task_id,
            Duration::from_secs(secs),
            progress,
        )
        .await?;
    Ok(wait_result(waited.state, waited.image))
}

async fn approve(engine: &TaskEngine, pairing: &str, args: &Value) -> Result<Value, AgentError> {
    let task_id = required(args, "task_id")?;
    let approval_id = required(args, "approval_id")?;
    let choice = Choice::parse_agent(required(args, "choice")?)
        .ok_or_else(|| failed("choice must be allow or deny"))?;
    let answer = match args.get("answer") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| failed("answer must be a string"))?,
        ),
    };
    if answer.is_some_and(|a| a.chars().count() > ANSWER_MAX_CHARS) {
        return Err(failed("answer must be at most 4000 characters"));
    }
    // Contract v1.1 R5: an answer with `deny` is refused, not silently dropped.
    if answer.is_some() && matches!(choice, Choice::Deny) {
        return Err(failed("answer is only allowed with allow"));
    }
    match engine
        .approve(
            Answerer::Agent(pairing.to_string()),
            task_id,
            approval_id,
            choice,
            answer,
        )
        .await
    {
        Ok(state) => Ok(approve_result(state)),
        Err(ApproveError::TaskNotFound) => Err(AgentError::TaskNotFound {
            task_id: task_id.to_string(),
        }),
        Err(ApproveError::ApprovalNotFound) => Err(AgentError::ApprovalNotFound {
            approval_id: approval_id.to_string(),
        }),
        Err(ApproveError::Invalid(why)) => Err(failed(why)),
    }
}

fn fits(result: &CallToolResult) -> bool {
    serde_json::to_vec(result).is_ok_and(|bytes| bytes.len() <= MCP_RESULT_BUDGET)
}

/// A wait's result: the state as text and `structuredContent`, then the
/// image (R12) — within `MCP_RESULT_BUDGET`, giving up the image first and
/// then halving the reply until it fits (Review Focus 4).
pub fn wait_result(mut state: Value, image: Option<String>) -> CallToolResult {
    if let Some(data) = image {
        let mut with_image = CallToolResult::structured(state.clone());
        with_image
            .content
            .push(ContentBlock::image(data, "image/jpeg"));
        if fits(&with_image) {
            return with_image;
        }
        state["screenshot_error"] = json!("too_large");
    }
    loop {
        let result = CallToolResult::structured(state.clone());
        if fits(&result) {
            return result;
        }
        let Some(reply) = state
            .get("reply")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        else {
            return result;
        };
        let keep = reply.chars().count() / 2;
        state["reply"] = json!(reply.chars().take(keep).collect::<String>());
        state["reply_truncated"] = json!(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::task::engine::testing::*;

    fn structured(r: &CallToolResult) -> Value {
        r.structured_content.clone().expect("structured")
    }

    #[test]
    fn the_task_tools_are_named_and_shaped_as_the_contract_says() {
        let tools = task_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(names, [TASK_START, TASK_WAIT, TASK_APPROVE, TASK_CANCEL]);
        let schema = |i: usize| Value::Object((*tools[i].input_schema).clone());
        assert_eq!(schema(0)["required"], json!(["prompt"]));
        assert_eq!(
            schema(1)["properties"]["timeout_secs"],
            json!({"type": "integer", "minimum": 1, "maximum": 60, "default": 30, "description": "The longest to wait, in seconds"})
        );
        assert_eq!(
            schema(2)["properties"]["choice"]["enum"],
            json!(["allow", "deny"])
        );
        assert_eq!(
            schema(2)["required"],
            json!(["task_id", "approval_id", "choice"])
        );
        assert_eq!(schema(3)["required"], json!(["task_id"]));
    }

    #[tokio::test]
    async fn malformed_arguments_answer_tool_failed_with_the_contracts_detail() {
        let r = rig();
        let long = "x".repeat(8001);
        let cases = [
            (TASK_START, json!({}), "prompt is required"),
            (
                TASK_START,
                json!({"prompt": "   "}),
                "prompt must be 1 to 8000 characters",
            ),
            (
                TASK_START,
                json!({"prompt": long}),
                "prompt must be 1 to 8000 characters",
            ),
            (TASK_WAIT, json!({}), "task_id is required"),
            (
                TASK_WAIT,
                json!({"task_id": "t", "timeout_secs": 0}),
                "timeout_secs must be an integer from 1 to 60",
            ),
            (
                TASK_WAIT,
                json!({"task_id": "t", "timeout_secs": 61}),
                "timeout_secs must be an integer from 1 to 60",
            ),
            (
                TASK_WAIT,
                json!({"task_id": "t", "timeout_secs": 1.5}),
                "timeout_secs must be an integer from 1 to 60",
            ),
            (
                TASK_APPROVE,
                json!({"task_id": "t", "approval_id": "a"}),
                "choice is required",
            ),
            (
                TASK_APPROVE,
                json!({"task_id": "t", "approval_id": "a", "choice": "allow_always"}),
                "choice must be allow or deny",
            ),
            (
                TASK_APPROVE,
                json!({"task_id": "t", "approval_id": "a", "choice": "maybe"}),
                "choice must be allow or deny",
            ),
            (
                TASK_APPROVE,
                json!({"task_id": "t", "approval_id": "a", "choice": "allow", "answer": "y".repeat(4001)}),
                "answer must be at most 4000 characters",
            ),
            (
                TASK_APPROVE,
                json!({"task_id": "t", "approval_id": "a", "choice": "allow", "answer": 7}),
                "answer must be a string",
            ),
            (
                TASK_APPROVE,
                json!({"task_id": "t", "approval_id": "a", "choice": "deny", "answer": "no"}),
                "answer is only allowed with allow",
            ),
            (TASK_CANCEL, json!({}), "task_id is required"),
        ];
        for (tool, args, detail) in cases {
            let out = call(&r.engine, CHANNEL, "m1", tool, args.clone(), None).await;
            assert_eq!(out.is_error, Some(true), "{tool} {args}");
            let e = &structured(&out)["error"];
            assert_eq!(e["code"], "tool_failed", "{tool} {args}");
            assert_eq!(e["detail"], detail, "{tool} {args}");
        }
    }

    #[tokio::test]
    async fn a_question_needs_an_answer_from_the_agent() {
        let r = rig();
        let (id, s) = r.start().await;
        let _waiting = waiting_request(&r, "req-q", &s);
        r.engine.observe(&ask(&s, "req-q", None, &["Blue"]));
        let out = call(
            &r.engine,
            CHANNEL,
            "m1",
            TASK_APPROVE,
            json!({"task_id": id, "approval_id": "req-q", "choice": "allow"}),
            None,
        )
        .await;
        assert_eq!(
            structured(&out)["error"]["detail"],
            "answer is required for a question"
        );
    }

    #[tokio::test]
    async fn start_wait_approve_and_cancel_answer_in_the_contracts_shapes() {
        let r = rig();
        let started = call(
            &r.engine,
            CHANNEL,
            "m1",
            TASK_START,
            json!({"prompt": "  Generate a calculator Canvas "}),
            None,
        )
        .await;
        assert_eq!(started.is_error, Some(false));
        let v = structured(&started);
        assert_eq!(v["state"], "running");
        let text: Value =
            serde_json::from_str(started.content[0].as_text().unwrap().text.as_str()).unwrap();
        assert_eq!(text, v, "content[0] is the same object as text");
        let id = v["task_id"].as_str().unwrap().to_string();
        let s = v["session"].as_str().unwrap().to_string();
        let mut rx = waiting_request(&r, "req-1", &s);
        r.engine
            .observe(&ask(&s, "req-1", Some("permission"), &["Allow", "Deny"]));
        let waited = call(
            &r.engine,
            CHANNEL,
            "m1",
            TASK_WAIT,
            json!({"task_id": id, "timeout_secs": 1}),
            None,
        )
        .await;
        assert_eq!(structured(&waited)["state"], "awaiting_approval");
        let approved = call(
            &r.engine,
            CHANNEL,
            "m1",
            TASK_APPROVE,
            json!({"task_id": id, "approval_id": "req-1", "choice": "deny"}),
            None,
        )
        .await;
        assert_eq!(
            structured(&approved),
            json!({"ok": true, "state": "running"})
        );
        assert_eq!(rx.try_recv().unwrap().result.unwrap()["answer"], "Deny");
        let again = call(
            &r.engine,
            CHANNEL,
            "m1",
            TASK_APPROVE,
            json!({"task_id": id, "approval_id": "req-1", "choice": "allow"}),
            None,
        )
        .await;
        assert_eq!(structured(&again)["error"]["code"], "approval_not_found");
        let cancelled = call(
            &r.engine,
            CHANNEL,
            "m1",
            TASK_CANCEL,
            json!({"task_id": id}),
            None,
        )
        .await;
        assert_eq!(
            structured(&cancelled),
            json!({"task_id": id, "state": "cancelled"})
        );
        let unknown = call(
            &r.engine,
            CHANNEL,
            "m1",
            TASK_WAIT,
            json!({"task_id": "nope"}),
            None,
        )
        .await;
        assert_eq!(
            structured(&unknown)["error"],
            json!({"code": "task_not_found", "message": "no task nope for this agent", "task_id": "nope"})
        );
    }

    #[test]
    fn a_wait_result_carries_the_image_after_the_text() {
        let state =
            json!({"task_id": "t", "session": "s", "state": "done", "steps": [], "reply": "ok"});
        let r = wait_result(state.clone(), Some("/9j/4AAQ".into()));
        assert_eq!(r.structured_content, Some(state.clone()));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v["content"][1],
            json!({"type": "image", "data": "/9j/4AAQ", "mimeType": "image/jpeg"})
        );
        let text: Value = serde_json::from_str(v["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, state);
    }

    #[test]
    fn an_image_that_breaks_the_relay_budget_is_dropped_as_too_large() {
        let state = json!({"task_id": "t", "session": "s", "state": "done", "steps": [], "reply": "x".repeat(300_000)});
        let r = wait_result(state, Some("A".repeat(409_600)));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["content"].as_array().unwrap().len(), 1, "no image");
        assert_eq!(v["structuredContent"]["screenshot_error"], "too_large");
        assert!(serde_json::to_vec(&r).unwrap().len() <= MCP_RESULT_BUDGET);
    }

    #[test]
    fn an_oversized_reply_is_cut_to_fit() {
        let state = json!({"task_id": "t", "session": "s", "state": "done", "steps": [], "reply": "\"".repeat(1_000_000)});
        let r = wait_result(state, None);
        assert!(serde_json::to_vec(&r).unwrap().len() <= MCP_RESULT_BUDGET);
        let v = r.structured_content.unwrap();
        assert_eq!(v["reply_truncated"], true);
        assert!(v["reply"].as_str().unwrap().len() < 1_000_000);
    }
}
