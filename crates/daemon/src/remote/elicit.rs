//! Asking the agent's client for a person's approval (M2a §4).
//!
//! # Why the question is awaited where it is
//!
//! From MCP protocol 2026-07-28 (SEP-2260) a server may send a request to the
//! client only while handling one of the client's: rmcp ties the two through
//! a task-local set around the request handler's future and refuses an
//! unassociated `elicitation/create` with `invalid_request`. So the question
//! is put from inside `AgentMcpServer::call_tool`'s own future —
//! [`PeerApprover::ask`] is awaited there and must never be `tokio::spawn`ed.
//! Pinned by `tests::the_question_is_part_of_the_call_under_strict_association`.
//!
//! # Timeouts
//!
//! When [`APPROVAL_TIMEOUT`] passes, rmcp's `await_response` returns
//! `ServiceError::Timeout` and itself sends the client
//! `notifications/cancelled` for the elicitation (reason `"request timeout"`),
//! so a client that answers late is answering a request that no longer exists.

use std::time::Duration;

use async_trait::async_trait;
use nevoflux_mcp::rmcp::model::{
    ElicitRequestParams, ElicitResult, ElicitationAction, ElicitationSchema, RequestMetaObject,
};
use nevoflux_mcp::rmcp::service::{ElicitationMode, Peer};
use nevoflux_mcp::rmcp::{RoleServer, ServiceError};
use serde_json::Value;

use super::mcp_tools::{Approval, Approver};

/// How long a person has to answer.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// The `_meta` key the client stamps on every `tools/call` and the head
/// copies onto that call's elicitation, so a client running several calls
/// knows which one is asking (agreed with nevoflux-muse 0.3.0).
pub const CALL_META_KEY: &str = "io.nevoflux/call";

/// One required boolean, `approve` (M2a §4.1). Nothing else: a form the
/// person can answer in one keystroke.
pub fn approval_schema() -> ElicitationSchema {
    ElicitationSchema::builder()
        .required_bool_property("approve", |b| b)
        .build()
        .expect("a schema whose one required property exists")
}

/// What the client's answer means. Only `accept` with `approve: true` runs
/// the call; everything else — `approve: false`, no content, a non-boolean,
/// decline, cancel — is a no.
pub fn outcome_of(result: &ElicitResult) -> Approval {
    match result.action {
        ElicitationAction::Accept => {
            let yes = result
                .content
                .as_ref()
                .and_then(|c| c.get("approve"))
                .and_then(Value::as_bool);
            if yes == Some(true) {
                Approval::Approved
            } else {
                Approval::Declined
            }
        }
        _ => Approval::Declined,
    }
}

/// The call id a `tools/call` carried in its `_meta`, if any. rmcp moves a
/// request's `_meta` onto its context; the params' own copy is the fallback.
pub fn call_correlation(
    context_meta: &RequestMetaObject,
    params_meta: Option<&RequestMetaObject>,
) -> Option<Value> {
    context_meta
        .get(CALL_META_KEY)
        .or_else(|| params_meta.and_then(|m| m.get(CALL_META_KEY)))
        .cloned()
}

/// Asks the client at the other end of one MCP session.
pub struct PeerApprover {
    peer: Peer<RoleServer>,
    timeout: Duration,
    call: Option<Value>,
}

impl PeerApprover {
    pub fn new(peer: Peer<RoleServer>, timeout: Duration) -> Self {
        Self {
            peer,
            timeout,
            call: None,
        }
    }

    /// Stamp every question with the call id the `tools/call` carried.
    pub fn with_call(mut self, call: Option<Value>) -> Self {
        self.call = call;
        self
    }
}

#[async_trait]
impl Approver for PeerApprover {
    async fn ask(&self, message: String) -> Approval {
        // Form mode only: a URL-mode elicitation would send the person to a
        // page, and there is no page for "allow this click?".
        if !self
            .peer
            .supported_elicitation_modes()
            .contains(&ElicitationMode::Form)
        {
            return Approval::Unsupported;
        }
        let meta = self.call.as_ref().map(|call| {
            let mut meta = RequestMetaObject::new();
            meta.insert(CALL_META_KEY.to_string(), call.clone());
            meta
        });
        let params = ElicitRequestParams::FormElicitationParams {
            meta,
            message,
            requested_schema: approval_schema(),
        };
        // Awaited here, in the handler's future — see the module docs.
        match self
            .peer
            .create_elicitation_with_timeout(params, Some(self.timeout))
            .await
        {
            Ok(result) => outcome_of(&result),
            Err(ServiceError::Timeout { .. }) => Approval::TimedOut,
            Err(e) => {
                tracing::warn!(target: "remote", "the approval request could not be put: {e}");
                Approval::Failed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::mcp_server::AgentMcpServer;
    use crate::remote::mcp_tools::AgentToolBackend;
    use nevoflux_mcp::rmcp::model::{
        CallToolRequestParams, CallToolResult, ClientInfo, ContentBlock, ElicitationCapability,
        ProtocolVersion, Tool,
    };
    use nevoflux_mcp::rmcp::service::RequestContext;
    use nevoflux_mcp::rmcp::{ClientHandler, ErrorData, RoleClient, ServiceExt};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// A backend that only asks, and says what came of it.
    struct AskingBackend;

    #[async_trait]
    impl AgentToolBackend for AskingBackend {
        fn tools(&self) -> Vec<Tool> {
            vec![Tool::new(
                "browser_click",
                "asks first",
                serde_json::Map::new(),
            )]
        }
        async fn call(
            &self,
            _name: &str,
            _arguments: Value,
            approver: &dyn Approver,
        ) -> Result<CallToolResult, ErrorData> {
            let answer = approver.ask("Click on element 'e1'".into()).await;
            Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "{answer:?}"
            ))]))
        }
    }

    #[derive(Clone, Copy)]
    enum Answer {
        Approve(bool),
        AcceptEmpty,
        Decline,
        Cancel,
        Never,
    }

    /// What one elicitation looked like from the client's side.
    #[derive(Debug, Clone)]
    struct Seen {
        message: String,
        schema: Value,
        call: Option<Value>,
    }

    #[derive(Clone)]
    struct Muse {
        info: ClientInfo,
        answer: Answer,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl Muse {
        fn new(answer: Answer, can_elicit: bool, version: ProtocolVersion) -> Self {
            let mut info = ClientInfo::default();
            info.protocol_version = version;
            if can_elicit {
                info.capabilities.elicitation = Some(ElicitationCapability::default());
            }
            Self {
                info,
                answer,
                seen: Arc::default(),
            }
        }
    }

    impl ClientHandler for Muse {
        fn get_info(&self) -> ClientInfo {
            self.info.clone()
        }

        async fn create_elicitation(
            &self,
            request: ElicitRequestParams,
            context: RequestContext<RoleClient>,
        ) -> Result<ElicitResult, ErrorData> {
            if let ElicitRequestParams::FormElicitationParams {
                message,
                requested_schema,
                meta,
                ..
            } = &request
            {
                let call = context
                    .meta
                    .get(CALL_META_KEY)
                    .cloned()
                    .or_else(|| meta.as_ref().and_then(|m| m.get(CALL_META_KEY).cloned()));
                self.seen.lock().unwrap().push(Seen {
                    message: message.clone(),
                    schema: serde_json::to_value(requested_schema).unwrap(),
                    call,
                });
            }
            match self.answer {
                Answer::Approve(yes) => Ok(ElicitResult::new(ElicitationAction::Accept)
                    .with_content(json!({ "approve": yes }))),
                Answer::AcceptEmpty => Ok(ElicitResult::new(ElicitationAction::Accept)),
                Answer::Decline => Ok(ElicitResult::new(ElicitationAction::Decline)),
                Answer::Cancel => Ok(ElicitResult::new(ElicitationAction::Cancel)),
                Answer::Never => std::future::pending::<Result<ElicitResult, ErrorData>>().await,
            }
        }
    }

    /// Serve `AgentMcpServer(AskingBackend)` over an in-memory pipe, call
    /// `browser_click` once (stamped with a call id when `call` is given), and
    /// return what the backend reported.
    async fn ask_through_the_channel(muse: Muse, call: Option<&str>) -> String {
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            if let Ok(running) = AgentMcpServer::new(Arc::new(AskingBackend))
                .serve(server_io)
                .await
            {
                let _ = running.waiting().await;
            }
        });
        let client = muse
            .serve(client_io)
            .await
            .expect("the session initialises");
        let mut params = CallToolRequestParams::new("browser_click");
        if let Some(id) = call {
            let mut meta = RequestMetaObject::new();
            meta.insert(CALL_META_KEY.to_string(), json!(id));
            params.meta = Some(meta);
        }
        let result = client
            .peer()
            .call_tool(params)
            .await
            .expect("the call is answered");
        let text = result.content[0].as_text().expect("text").text.clone();
        let _ = client.cancel().await;
        text
    }

    const V: ProtocolVersion = ProtocolVersion::V_2025_11_25;

    #[test]
    fn the_question_is_one_required_boolean() {
        assert_eq!(
            serde_json::to_value(approval_schema()).unwrap(),
            json!({"type": "object", "properties": {"approve": {"type": "boolean"}}, "required": ["approve"]})
        );
    }

    #[test]
    fn a_question_waits_sixty_seconds() {
        assert_eq!(APPROVAL_TIMEOUT, Duration::from_secs(60));
    }

    #[test]
    fn only_an_explicit_yes_approves() {
        let accept = |content: Option<Value>| {
            let r = ElicitResult::new(ElicitationAction::Accept);
            match content {
                Some(v) => r.with_content(v),
                None => r,
            }
        };
        assert_eq!(
            outcome_of(&accept(Some(json!({"approve": true})))),
            Approval::Approved
        );
        assert_eq!(
            outcome_of(&accept(Some(json!({"approve": false})))),
            Approval::Declined
        );
        assert_eq!(
            outcome_of(&accept(Some(json!({"approve": "true"})))),
            Approval::Declined
        );
        assert_eq!(outcome_of(&accept(None)), Approval::Declined);
        assert_eq!(
            outcome_of(&ElicitResult::new(ElicitationAction::Decline)),
            Approval::Declined
        );
        assert_eq!(
            outcome_of(&ElicitResult::new(ElicitationAction::Cancel)),
            Approval::Declined
        );
    }

    #[test]
    fn the_call_id_is_read_from_the_request_meta_first() {
        let mut ctx = RequestMetaObject::new();
        ctx.insert(CALL_META_KEY.to_string(), json!("aaaaaaaa"));
        let mut params = RequestMetaObject::new();
        params.insert(CALL_META_KEY.to_string(), json!("bbbbbbbb"));
        assert_eq!(
            call_correlation(&ctx, Some(&params)),
            Some(json!("aaaaaaaa"))
        );
        assert_eq!(
            call_correlation(&RequestMetaObject::new(), Some(&params)),
            Some(json!("bbbbbbbb"))
        );
        assert_eq!(call_correlation(&RequestMetaObject::new(), None), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_yes_approves_and_the_client_saw_the_question() {
        let muse = Muse::new(Answer::Approve(true), true, V);
        let seen = muse.seen.clone();
        assert_eq!(ask_through_the_channel(muse, None).await, "Approved");
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].message, "Click on element 'e1'");
        assert_eq!(
            seen[0].schema,
            serde_json::to_value(approval_schema()).unwrap()
        );
        assert_eq!(seen[0].call, None, "no call id was sent, none is invented");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_question_names_the_call_it_belongs_to() {
        let muse = Muse::new(Answer::Approve(true), true, V);
        let seen = muse.seen.clone();
        ask_through_the_channel(muse, Some("0badc0de")).await;
        assert_eq!(seen.lock().unwrap()[0].call, Some(json!("0badc0de")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_kind_of_no_is_a_no() {
        for answer in [
            Answer::Approve(false),
            Answer::AcceptEmpty,
            Answer::Decline,
            Answer::Cancel,
        ] {
            assert_eq!(
                ask_through_the_channel(Muse::new(answer, true, V), None).await,
                "Declined"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_that_cannot_be_asked_is_never_asked() {
        let muse = Muse::new(Answer::Approve(true), false, V);
        let seen = muse.seen.clone();
        assert_eq!(ask_through_the_channel(muse, None).await, "Unsupported");
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn silence_for_sixty_seconds_is_a_timeout() {
        let muse = Muse::new(Answer::Never, true, V);
        assert_eq!(ask_through_the_channel(muse, None).await, "TimedOut");
    }

    /// SEP-2260: at protocol 2026-07-28 rmcp refuses a server→client request
    /// that is not tied to the client request being handled. This passes only
    /// because the question is awaited inside `call_tool`'s own future.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_question_is_part_of_the_call_under_strict_association() {
        let muse = Muse::new(Answer::Approve(true), true, ProtocolVersion::V_2026_07_28);
        assert_eq!(ask_through_the_channel(muse, None).await, "Approved");
    }

    /// M2b §2.2: pausing closes the channel while a question may be open. The
    /// question must end at once as `Failed` (→ `approval_denied` and a
    /// "cancelled" notice), not sit out its sixty seconds as a timeout.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_closed_mid_question_ends_the_question_as_failed() {
        struct Reports(tokio::sync::mpsc::UnboundedSender<Approval>);

        #[async_trait]
        impl AgentToolBackend for Reports {
            fn tools(&self) -> Vec<Tool> {
                vec![Tool::new(
                    "browser_click",
                    "asks first",
                    serde_json::Map::new(),
                )]
            }
            async fn call(
                &self,
                _name: &str,
                _arguments: Value,
                approver: &dyn Approver,
            ) -> Result<CallToolResult, ErrorData> {
                let answer = approver.ask("Click on element 'e1'".into()).await;
                let _ = self.0.send(answer);
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "{answer:?}"
                ))]))
            }
        }

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let session = tokio_util::sync::CancellationToken::new();
        let ct = session.clone();
        tokio::spawn(async move {
            if let Ok(running) = AgentMcpServer::new(Arc::new(Reports(tx)))
                .serve_with_ct(server_io, ct)
                .await
            {
                let _ = running.waiting().await;
            }
        });
        let muse = Muse::new(Answer::Never, true, V);
        let seen = muse.seen.clone();
        let client = muse
            .serve(client_io)
            .await
            .expect("the session initialises");
        let peer = client.peer().clone();
        tokio::spawn(async move {
            let _ = peer
                .call_tool(CallToolRequestParams::new("browser_click"))
                .await;
        });
        for _ in 0..250 {
            if !seen.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(seen.lock().unwrap().len(), 1, "the question is open");

        // What closing the channel does to the session (`McpGateway::end_session`).
        session.cancel();
        let answer = tokio::time::timeout(Duration::from_secs(15), rx.recv())
            .await
            .expect("the question ends well inside its sixty seconds")
            .expect("the backend reported");
        assert_eq!(answer, Approval::Failed);
        let _ = client.cancel().await;
    }
}
