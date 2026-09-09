//! Daemon-side writer for the session event log (design spec §3.4).
//!
//! # Where this is called from
//!
//! `agent_host.rs` converts an `LlmRequest` into a daemon request through *two*
//! branches — `convert_request_with_messages` when compaction rewrote the
//! messages, `convert_request_to_daemon` otherwise — and the uncompressed branch
//! is the common one. The writer is therefore called **after** the two branches
//! join, on the `daemon_request` binding, so invariant I1 ("model-visible ⟺
//! logged") holds for every request rather than only for compacted ones.
//!
//! Every write is best-effort: a logging failure is traced and swallowed, never
//! propagated into a live request. An audit trail that can take down the thing
//! it audits is worse than one with a gap.

use nevoflux_protocol::session_event::{
    content_hash, tools_hash, LoggedToolCall, SessionEventPayload, TokenUsage,
};
use std::sync::Arc;

use nevoflux_storage::connection::Database;
use nevoflux_storage::repositories::SessionEventRepository;

use crate::wasm::llm::LlmChatRequest;

/// Appends session events for one session.
pub struct SessionEventWriter {
    db: Arc<Database>,
    session_id: String,
}

impl SessionEventWriter {
    /// Create a writer bound to one session.
    ///
    /// Takes the shared handle `HostServices` already holds rather than a fresh
    /// connection, so events land in the same database the rest of the run uses.
    pub fn new(db: Arc<Database>, session_id: String) -> Self {
        Self { db, session_id }
    }

    /// Log everything that entered one LLM request.
    ///
    /// Emits `system/message` only when the prompt differs from the last one
    /// logged, `user/message` for the last user turn in the request, and
    /// `request/header` when provider, model or tool set changed.
    pub fn record_request(&self, req: &LlmChatRequest, provider: &str, model: &str) {
        if self.session_id.is_empty() {
            return;
        }
        self.record_system(req);
        self.record_user(req);
        self.record_header(req, provider, model);
        self.assert_derivable(req);
    }

    /// Log one successful model response.
    pub fn record_assistant(
        &self,
        content: &str,
        tool_calls: Vec<LoggedToolCall>,
        usage: Option<TokenUsage>,
        provider: &str,
        model: &str,
    ) {
        self.append(SessionEventPayload::AssistantMessage {
            content: content.to_string(),
            tool_calls,
            usage,
            model: model.to_string(),
            provider: provider.to_string(),
        });
    }

    /// Append one already-built event.
    ///
    /// Public so other subsystems (tool recording, compaction, pack activation)
    /// can log without rebuilding this type's private logic.
    pub fn append(&self, payload: SessionEventPayload) {
        if self.session_id.is_empty() {
            return;
        }
        let repo = SessionEventRepository::new(&self.db);
        if let Err(e) = repo.append(&self.session_id, &payload) {
            tracing::warn!(
                session = %self.session_id,
                event = payload.type_str(),
                error = %e,
                "session event append failed"
            );
        }
    }

    fn record_system(&self, req: &LlmChatRequest) {
        let Some(system) = req.system.as_deref() else {
            return;
        };
        let repo = SessionEventRepository::new(&self.db);
        let unchanged = match repo.last_of_type(&self.session_id, "system/message") {
            Ok(Some(ev)) => match &ev.payload {
                SessionEventPayload::SystemMessage { content, .. } => {
                    content_hash(content) == content_hash(system)
                }
                _ => false,
            },
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(error = %e, "session event: system dedup lookup failed");
                false
            }
        };
        if unchanged {
            return;
        }
        self.append(SessionEventPayload::SystemMessage {
            content: system.to_string(),
            // P1 makes the prompt sectioned; until then there is nothing
            // truthful to put here, and inventing ids would make the log lie.
            sections: Vec::new(),
            origin: "kernel".into(),
        });
    }

    fn record_user(&self, req: &LlmChatRequest) {
        let Some(last_user) = req.messages.iter().rev().find(|m| m.role == "user") else {
            return;
        };
        // Descriptors, never the bytes: a log that inlined base64 image data
        // would be unreadable and would balloon the database.
        let attachments = last_user
            .attachments
            .iter()
            .map(|a| format!("{} ({})", a.name, a.mime_type))
            .collect();
        self.append(SessionEventPayload::UserMessage {
            content: last_user.content.clone(),
            attachments,
            origin: "user".into(),
        });
    }

    fn record_header(&self, req: &LlmChatRequest, provider: &str, model: &str) {
        let names: Vec<String> = req
            .tools
            .as_ref()
            .map(|ts| ts.iter().map(|t| t.name.clone()).collect())
            .unwrap_or_default();
        let hash = tools_hash(&names);

        let repo = SessionEventRepository::new(&self.db);
        let reason = match repo.last_of_type(&self.session_id, "request/header") {
            Ok(Some(ev)) => match &ev.payload {
                SessionEventPayload::RequestHeader {
                    provider: p,
                    model: m,
                    tools_hash: h,
                    ..
                } => {
                    if p == provider && m == model && *h == hash {
                        return; // unchanged — nothing to log
                    } else if m != model || p != provider {
                        "model_changed"
                    } else {
                        "tools_changed"
                    }
                }
                _ => "initial",
            },
            Ok(None) => "initial",
            Err(e) => {
                tracing::warn!(error = %e, "session event: header dedup lookup failed");
                "initial"
            }
        };

        self.append(SessionEventPayload::RequestHeader {
            provider: provider.to_string(),
            model: model.to_string(),
            tools_hash: hash,
            reason: reason.to_string(),
        });
    }

    /// Debug-only check that what was just logged derives back to what is about
    /// to be sent (invariant I1).
    ///
    /// Traces a warning rather than panicking: a live request must not die
    /// because its audit trail disagreed. In debug builds the warning is loud
    /// enough to catch in development, which is where the drift would start.
    #[cfg(debug_assertions)]
    fn assert_derivable(&self, req: &LlmChatRequest) {
        let repo = SessionEventRepository::new(&self.db);

        // Two indexed lookups rather than reading the whole log: the check only
        // needs the most recent system and user events, and a full `list()` on
        // every request would grow linearly with the session.
        match repo.last_of_type(&self.session_id, "system/message") {
            Ok(Some(ev)) => {
                if let SessionEventPayload::SystemMessage { content, .. } = &ev.payload {
                    if Some(content.as_str()) != req.system.as_deref() {
                        tracing::warn!(
                            session = %self.session_id,
                            "I1 drift: system prompt differs from log"
                        );
                    }
                }
            }
            Ok(None) if req.system.is_some() => {
                tracing::warn!(
                    session = %self.session_id,
                    "I1 drift: a system prompt was sent but never logged"
                );
            }
            _ => {}
        }

        let sent_user = req.messages.iter().rev().find(|m| m.role == "user");
        match (
            repo.last_of_type(&self.session_id, "user/message"),
            sent_user,
        ) {
            (Ok(Some(ev)), Some(m)) => {
                if let SessionEventPayload::UserMessage { content, .. } = &ev.payload {
                    if content != &m.content {
                        tracing::warn!(
                            session = %self.session_id,
                            "I1 drift: user message differs from log"
                        );
                    }
                }
            }
            (Ok(None), Some(_)) => {
                tracing::warn!(
                    session = %self.session_id,
                    "I1 drift: a user message was sent but never logged"
                );
            }
            _ => {}
        }
    }

    /// Release builds skip the derivability check entirely.
    #[cfg(not(debug_assertions))]
    fn assert_derivable(&self, _req: &LlmChatRequest) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm::llm::{LlmChatRequest, LlmMessage};
    use nevoflux_storage::Storage;

    fn msg(role: &str, content: &str) -> LlmMessage {
        LlmMessage {
            role: role.into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            attachments: vec![],
            reasoning: None,
        }
    }

    fn req(system: Option<&str>, user: &str) -> LlmChatRequest {
        LlmChatRequest {
            messages: vec![msg("user", user)],
            system: system.map(|s| s.to_string()),
            temperature: None,
            max_tokens: None,
            tools: None,
        }
    }

    fn writer(storage: &Storage, session: &str) -> SessionEventWriter {
        // `Database` clones share one connection, so the writer and the test's
        // reads see the same in-memory database.
        SessionEventWriter::new(Arc::new(storage.database().clone()), session.to_string())
    }

    #[test]
    fn a_first_request_logs_system_user_and_header() {
        let storage = Storage::open_in_memory().unwrap();
        let w = writer(&storage, "s1");
        w.record_request(&req(Some("SYS"), "hello"), "anthropic", "claude-opus-5");

        let types: Vec<&str> = storage
            .session_events()
            .list("s1")
            .unwrap()
            .iter()
            .map(|e| e.payload.type_str())
            .collect();
        assert_eq!(
            types,
            vec!["system/message", "user/message", "request/header"]
        );
    }

    #[test]
    fn an_unchanged_system_prompt_is_not_logged_twice() {
        let storage = Storage::open_in_memory().unwrap();
        let w = writer(&storage, "s1");
        w.record_request(&req(Some("SYS"), "one"), "anthropic", "m");
        w.record_request(&req(Some("SYS"), "two"), "anthropic", "m");

        let evs = storage.session_events().list("s1").unwrap();
        let sys = evs
            .iter()
            .filter(|e| e.payload.type_str() == "system/message")
            .count();
        assert_eq!(sys, 1, "an unchanged system prompt must not be re-logged");
        let users = evs
            .iter()
            .filter(|e| e.payload.type_str() == "user/message")
            .count();
        assert_eq!(users, 2, "every request's user message is logged");
    }

    #[test]
    fn a_changed_system_prompt_is_logged_again() {
        let storage = Storage::open_in_memory().unwrap();
        let w = writer(&storage, "s1");
        w.record_request(&req(Some("SYS-A"), "one"), "anthropic", "m");
        w.record_request(&req(Some("SYS-B"), "two"), "anthropic", "m");

        let sys: Vec<String> = storage
            .session_events()
            .list("s1")
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.payload {
                SessionEventPayload::SystemMessage { content, .. } => Some(content),
                _ => None,
            })
            .collect();
        assert_eq!(sys, vec!["SYS-A".to_string(), "SYS-B".to_string()]);
    }

    #[test]
    fn an_unchanged_header_is_not_logged_twice_but_a_model_change_is() {
        let storage = Storage::open_in_memory().unwrap();
        let w = writer(&storage, "s1");
        w.record_request(&req(Some("SYS"), "one"), "anthropic", "m1");
        w.record_request(&req(Some("SYS"), "two"), "anthropic", "m1");
        w.record_request(&req(Some("SYS"), "three"), "anthropic", "m2");

        let headers: Vec<(String, String)> = storage
            .session_events()
            .list("s1")
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.payload {
                SessionEventPayload::RequestHeader { model, reason, .. } => Some((model, reason)),
                _ => None,
            })
            .collect();
        assert_eq!(
            headers,
            vec![
                ("m1".to_string(), "initial".to_string()),
                ("m2".to_string(), "model_changed".to_string()),
            ]
        );
    }

    #[test]
    fn the_logged_user_message_is_the_last_user_message_actually_sent() {
        // With several user messages in history, the one that entered this
        // request is the last — including whatever prefixes were injected.
        let storage = Storage::open_in_memory().unwrap();
        let w = writer(&storage, "s1");
        let mut r = req(Some("SYS"), "old");
        r.messages.push(msg("assistant", "ack"));
        r.messages
            .push(msg("user", "NEW with injected tab context"));
        w.record_request(&r, "anthropic", "m");

        let found = storage
            .session_events()
            .list("s1")
            .unwrap()
            .into_iter()
            .find_map(|e| match e.payload {
                SessionEventPayload::UserMessage { content, .. } => Some(content),
                _ => None,
            })
            .unwrap();
        assert_eq!(found, "NEW with injected tab context");
    }

    #[test]
    fn a_write_for_an_empty_session_id_is_dropped_rather_than_failing() {
        // I1 wants everything logged, but logging must never take down a live
        // request — and an empty session id has nowhere to log to.
        let storage = Storage::open_in_memory().unwrap();
        let w = writer(&storage, "");
        w.record_request(&req(Some("SYS"), "hello"), "anthropic", "m");
        assert!(storage.session_events().list("").unwrap().is_empty());
    }

    #[test]
    fn a_changed_tool_set_logs_a_header_with_the_tools_changed_reason() {
        use crate::wasm::llm::LlmToolDefinition;
        let storage = Storage::open_in_memory().unwrap();
        let w = writer(&storage, "s1");

        let mut r = req(Some("SYS"), "one");
        r.tools = Some(vec![LlmToolDefinition {
            name: "read_file".into(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }]);
        w.record_request(&r, "anthropic", "m");

        r.tools = Some(vec![
            LlmToolDefinition {
                name: "read_file".into(),
                description: String::new(),
                parameters: serde_json::json!({}),
            },
            LlmToolDefinition {
                name: "write_file".into(),
                description: String::new(),
                parameters: serde_json::json!({}),
            },
        ]);
        w.record_request(&r, "anthropic", "m");

        let reasons: Vec<String> = storage
            .session_events()
            .list("s1")
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.payload {
                SessionEventPayload::RequestHeader { reason, .. } => Some(reason),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasons,
            vec!["initial".to_string(), "tools_changed".to_string()]
        );
    }

    #[test]
    fn an_assistant_message_records_usage_including_cache_fields() {
        let storage = Storage::open_in_memory().unwrap();
        let w = writer(&storage, "s1");
        w.record_assistant(
            "done",
            vec![LoggedToolCall {
                id: "t1".into(),
                name: "read_file".into(),
                args: serde_json::json!({}),
            }],
            Some(TokenUsage {
                input_tokens: Some(100),
                output_tokens: Some(20),
                cache_read_tokens: Some(80),
                cache_write_tokens: None,
            }),
            "anthropic",
            "claude-opus-5",
        );

        let ev = storage.session_events().list("s1").unwrap().pop().unwrap();
        match ev.payload {
            SessionEventPayload::AssistantMessage {
                content,
                tool_calls,
                usage,
                model,
                provider,
            } => {
                assert_eq!(content, "done");
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].name, "read_file");
                assert_eq!(usage.unwrap().cache_read_tokens, Some(80));
                assert_eq!(model, "claude-opus-5");
                assert_eq!(provider, "anthropic");
            }
            other => panic!("wrong payload: {other:?}"),
        }
    }

    #[test]
    fn a_logged_request_derives_back_to_the_same_system_and_user_content() {
        // Invariant I1 in its checkable form (spec §3.6).
        let storage = Storage::open_in_memory().unwrap();
        let w = writer(&storage, "s1");
        let r = req(Some("SYS"), "hello");
        w.record_request(&r, "anthropic", "m");

        let events = storage.session_events().list("s1").unwrap();
        let derived = crate::replay::derive_agent_input(&events).unwrap();
        assert_eq!(derived.system_prompt.as_deref(), r.system.as_deref());
        assert_eq!(derived.user_message, "hello");
    }
}
