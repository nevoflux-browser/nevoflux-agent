//! A delegated task, answered or stopped from the desktop status bar (M3
//! contract §8, R23). Local only: neither command is a portal query or an
//! agent-channel tool, and Canvas pages are refused every `remote.` command.

use serde_json::{json, Value};

use super::answer::Choice;
use super::engine::{Answerer, ApproveError, TaskEngine};

/// Answer `remote.task_answer` / `remote.task_cancel` with a `system_response`.
pub async fn handle(engine: &TaskEngine, command: &str, params: &Value, request_id: &str) -> Value {
    let field = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let reply = |result: Result<Value, (&'static str, String)>| match result {
        Ok(data) => json!({"type": "system_response", "payload": {
            "request_id": request_id, "command": command, "success": true, "data": data }}),
        Err((code, message)) => json!({"type": "system_response", "payload": {
            "request_id": request_id, "command": command, "success": false,
            "error": { "code": code, "message": message } }}),
    };
    let task_id = field("task_id");
    match command {
        "remote.task_answer" => {
            let approval_id = field("approval_id");
            let Some(choice) = Choice::parse_desktop(&field("choice")) else {
                return reply(Err((
                    "INVALID_CHOICE",
                    "choice must be allow, allow_always or deny".to_string(),
                )));
            };
            reply(
                match engine
                    .approve(Answerer::Desktop, &task_id, &approval_id, choice, None)
                    .await
                {
                    Ok(state) => Ok(json!({ "state": state.as_str() })),
                    Err(ApproveError::TaskNotFound) => {
                        Err(("TASK_NOT_FOUND", format!("no task {task_id}")))
                    }
                    Err(ApproveError::ApprovalNotFound) => Err((
                        "APPROVAL_NOT_FOUND",
                        format!("approval {approval_id} is not waiting for an answer"),
                    )),
                    Err(ApproveError::Invalid(why)) => Err(("INVALID_CHOICE", why.to_string())),
                    // Only an agent's answer is ever desktop-only (R39); the
                    // arm keeps the match total all the same.
                    Err(ApproveError::DesktopOnly) => {
                        Err(("INVALID_CHOICE", super::answer::NOT_OFFERED.to_string()))
                    }
                },
            )
        }
        "remote.task_cancel" => reply(
            engine
                .cancel(None, &task_id)
                .await
                .map_err(|_| ("TASK_NOT_FOUND", format!("no task {task_id}"))),
        ),
        _ => reply(Err((
            "UNKNOWN_COMMAND",
            format!("{command} is not a task command"),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::task::engine::testing::*;

    #[tokio::test]
    async fn the_status_bar_answers_and_stops_a_task() {
        let r = rig();
        let (id, s) = r.start().await;
        let mut rx = waiting_request(&r, "req-1", &s);
        r.engine.observe(&ask(
            &s,
            "req-1",
            Some("permission"),
            &["Allow", "Always allow this type of action", "Deny"],
        ));
        let ok = handle(
            &r.engine,
            "remote.task_answer",
            &json!({"task_id": id, "approval_id": "req-1", "choice": "allow_always"}),
            "r1",
        )
        .await;
        assert_eq!(
            ok,
            json!({"type": "system_response", "payload": {"request_id": "r1", "command": "remote.task_answer",
                                                          "success": true, "data": {"state": "running"}}})
        );
        assert_eq!(
            rx.try_recv().unwrap().result.unwrap()["answer"],
            "Always allow this type of action"
        );
        let code = |v: Value| v["payload"]["error"]["code"].as_str().unwrap().to_string();
        assert_eq!(
            code(
                handle(
                    &r.engine,
                    "remote.task_answer",
                    &json!({"task_id": id, "approval_id": "req-1", "choice": "allow"}),
                    "r2"
                )
                .await
            ),
            "APPROVAL_NOT_FOUND"
        );
        assert_eq!(
            code(
                handle(
                    &r.engine,
                    "remote.task_answer",
                    &json!({"task_id": "nope", "approval_id": "req-1", "choice": "allow"}),
                    "r3"
                )
                .await
            ),
            "TASK_NOT_FOUND"
        );
        assert_eq!(
            code(
                handle(
                    &r.engine,
                    "remote.task_answer",
                    &json!({"task_id": id, "approval_id": "req-1", "choice": "maybe"}),
                    "r4"
                )
                .await
            ),
            "INVALID_CHOICE"
        );
        let stopped = handle(
            &r.engine,
            "remote.task_cancel",
            &json!({"task_id": id}),
            "r5",
        )
        .await;
        assert_eq!(
            stopped["payload"]["data"],
            json!({"task_id": id, "state": "cancelled"})
        );
        assert_eq!(
            code(
                handle(
                    &r.engine,
                    "remote.task_cancel",
                    &json!({"task_id": "nope"}),
                    "r6"
                )
                .await
            ),
            "TASK_NOT_FOUND"
        );
    }

    #[tokio::test]
    async fn the_desktop_cannot_answer_a_question_with_allow() {
        let r = rig();
        let (id, s) = r.start().await;
        let _waiting = waiting_request(&r, "req-q", &s);
        r.engine.observe(&ask(&s, "req-q", None, &["Blue"]));
        let v = handle(
            &r.engine,
            "remote.task_answer",
            &json!({"task_id": id, "approval_id": "req-q", "choice": "allow"}),
            "r1",
        )
        .await;
        assert_eq!(v["payload"]["error"]["code"], "INVALID_CHOICE");
    }
}
