//! Session event vocabulary — the append-only log that is the session's source
//! of truth (design spec §3).
//!
//! The 8 core event names are byte-for-byte dsh's `SessionEventMap` so a session
//! can be exported as dsh-compatible JSONL and moved between the local kernel and
//! dsh-cloud (ADR A3). NevoFlux-only events are prefixed (`pack/`, `context/`,
//! `tool/spill`) so they never collide with a future dsh core event.

use serde::{Deserialize, Serialize};

/// One section of an assembled system prompt: its stable id and a hash of its body.
///
/// Recorded instead of the body so a `system/message` event stays small while
/// still telling a replay which sections changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptSection {
    /// Stable section id (e.g. `kernel/protocol`, `base/browser`, `soul`).
    pub id: String,
    /// Hash of the section body, as produced by [`content_hash`].
    pub hash: String,
}

/// Token accounting reported by the provider for one assistant message.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Prompt tokens billed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Completion tokens billed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Prompt tokens served from the provider's prefix cache (Anthropic
    /// `cache_read_input_tokens`, DeepSeek `prompt_cache_hit_tokens`).
    /// P2 reads this to verify the prefix-caching work paid off (spec §5.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Prompt tokens written into the provider's prefix cache
    /// (Anthropic `cache_creation_input_tokens`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

/// One tool call as it appeared on an assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoggedToolCall {
    /// Provider-assigned tool call id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments as sent by the model.
    pub args: serde_json::Value,
}

/// Who initiated a tool call (spec §3.2 `origin`).
///
/// Stored as the literal spec string rather than a structured enum so the wire
/// form is stable and greppable: `model`, `canvas:<artifact_id>`, `mcp:<client>`,
/// `subagent:<id>`, `loop:<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolOrigin(String);

impl ToolOrigin {
    /// The model asked for this tool.
    pub fn model() -> Self {
        Self("model".into())
    }
    /// A Canvas panel asked for this tool.
    pub fn canvas(artifact_id: &str) -> Self {
        Self(format!("canvas:{artifact_id}"))
    }
    /// An MCP client asked for this tool.
    pub fn mcp(client: &str) -> Self {
        Self(format!("mcp:{client}"))
    }
    /// A subagent asked for this tool.
    pub fn subagent(id: &str) -> Self {
        Self(format!("subagent:{id}"))
    }
    /// A loop iteration asked for this tool.
    ///
    /// Named with a trailing underscore because `loop` is a Rust keyword.
    pub fn loop_(id: &str) -> Self {
        Self(format!("loop:{id}"))
    }
    /// Borrow the wire string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Wrap an already-formatted origin string (used when reading back a log).
    pub fn from_raw(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

/// The payload of one session event.
///
/// Internally tagged so an exported line is flat JSON:
/// `{"seq":1,"ts":...,"type":"tool/call","id":"t1",...}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionEventPayload {
    // ---- dsh core 8 ----
    /// A user turn began.
    #[serde(rename = "turn/start")]
    TurnStart {
        /// 1-based turn counter within the session.
        turn: u32,
    },
    /// A user turn ended (no pending input left).
    #[serde(rename = "turn/end")]
    TurnEnd {
        /// The turn that ended.
        turn: u32,
    },
    /// One LLM request plus its tool calls began.
    #[serde(rename = "step/start")]
    StepStart {
        /// 0-based step counter within the turn.
        step: u32,
        /// Owning turn.
        turn: u32,
    },
    /// One LLM request plus its tool calls ended.
    #[serde(rename = "step/end")]
    StepEnd {
        /// The step that ended.
        step: u32,
        /// Owning turn.
        turn: u32,
    },
    /// The system prompt was assembled for the first time, or changed.
    #[serde(rename = "system/message")]
    SystemMessage {
        /// The full assembled prompt.
        content: String,
        /// Section ids + hashes, empty until P1 makes the prompt sectioned.
        #[serde(default)]
        sections: Vec<PromptSection>,
        /// What produced it (`kernel`, `custom`, `pack:<name>`).
        origin: String,
    },
    /// A user message as it entered the request, injected prefixes included.
    #[serde(rename = "user/message")]
    UserMessage {
        /// Message body exactly as sent.
        content: String,
        /// Attachment descriptors (kind + identifier), not the bytes.
        #[serde(default)]
        attachments: Vec<String>,
        /// `user`, or a synthetic origin for loop/subagent-driven turns.
        origin: String,
    },
    /// One successful model response.
    #[serde(rename = "assistant/message")]
    AssistantMessage {
        /// Response text.
        content: String,
        /// Tool calls requested by the model.
        #[serde(default)]
        tool_calls: Vec<LoggedToolCall>,
        /// Provider-reported usage, when available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
        /// Model id that answered.
        model: String,
        /// Provider that answered.
        provider: String,
    },
    /// A tool is about to execute.
    #[serde(rename = "tool/call")]
    ToolCall {
        /// Tool call id, matching the later `tool/result`.
        id: String,
        /// Tool name.
        name: String,
        /// Arguments.
        args: serde_json::Value,
        /// Who initiated it.
        origin: ToolOrigin,
        /// URL of the tab the call targets, when the call is tab-scoped.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab_url: Option<String>,
    },
    /// A tool finished.
    #[serde(rename = "tool/result")]
    ToolResult {
        /// Tool call id.
        id: String,
        /// Full result, or a spill locator once P2 lands.
        content: String,
        /// Whether the tool reported failure.
        is_error: bool,
        /// Wall-clock duration.
        duration_ms: u64,
    },
    /// Provider / model / tool-set changed (or was set for the first time).
    #[serde(rename = "request/header")]
    RequestHeader {
        /// Provider name.
        provider: String,
        /// Model id.
        model: String,
        /// Hash of the offered tool names, from [`tools_hash`].
        tools_hash: String,
        /// Why this header was emitted (`initial`, `model_changed`, `tools_changed`).
        reason: String,
    },

    // ---- NevoFlux extensions (spec §3.2) ----
    /// A pack became active in this session.
    #[serde(rename = "pack/activate")]
    PackActivate {
        /// Pack name.
        pack: String,
    },
    /// A pack was dropped from this session.
    #[serde(rename = "pack/deactivate")]
    PackDeactivate {
        /// Pack name.
        pack: String,
    },
    /// A pack replaced the system prompt.
    #[serde(rename = "pack/prompt-replace")]
    PackPromptReplace {
        /// Pack that made the call.
        pack: String,
        /// `keep_kernel` or `full`.
        mode: String,
        /// Why the pack says it needed to.
        reason: String,
        /// Hash of the replacement body, so the log can show it changed
        /// without carrying a second copy of the prompt.
        hash: String,
    },
    /// A pack released the system prompt back to the kernel.
    #[serde(rename = "pack/prompt-restore")]
    PackPromptRestore {
        /// Pack that held it.
        pack: String,
    },
    /// Context was compacted.
    #[serde(rename = "context/compact")]
    ContextCompact {
        /// `pressure` or `overflow`.
        trigger: String,
        /// Estimated tokens before.
        before_tokens: u64,
        /// Estimated tokens after.
        after_tokens: u64,
    },
    /// A large tool result was written to disk instead of inlined.
    #[serde(rename = "tool/spill")]
    ToolSpill {
        /// Tool call id.
        id: String,
        /// Absolute path of the spill file.
        path: String,
        /// Byte length of the full result.
        bytes: u64,
    },
}

impl SessionEventPayload {
    /// The wire type string, identical to the serde tag.
    ///
    /// Stored in its own `session_events.type` column so a log can be filtered
    /// without parsing every payload.
    pub fn type_str(&self) -> &'static str {
        match self {
            Self::TurnStart { .. } => "turn/start",
            Self::TurnEnd { .. } => "turn/end",
            Self::StepStart { .. } => "step/start",
            Self::StepEnd { .. } => "step/end",
            Self::SystemMessage { .. } => "system/message",
            Self::UserMessage { .. } => "user/message",
            Self::AssistantMessage { .. } => "assistant/message",
            Self::ToolCall { .. } => "tool/call",
            Self::ToolResult { .. } => "tool/result",
            Self::RequestHeader { .. } => "request/header",
            Self::PackActivate { .. } => "pack/activate",
            Self::PackDeactivate { .. } => "pack/deactivate",
            Self::PackPromptReplace { .. } => "pack/prompt-replace",
            Self::PackPromptRestore { .. } => "pack/prompt-restore",
            Self::ContextCompact { .. } => "context/compact",
            Self::ToolSpill { .. } => "tool/spill",
        }
    }
}

/// One row of the log: sequence, timestamp, payload.
///
/// Serializes flat (`seq`/`ts` beside `type`) so an exported line is one
/// self-describing JSON object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    /// Monotonic per-session sequence, 1-based.
    pub seq: i64,
    /// Unix epoch milliseconds.
    pub ts: i64,
    /// The event itself.
    #[serde(flatten)]
    pub payload: SessionEventPayload,
}

/// Stable content hash used for prompt sections and request headers.
///
/// FNV-1a 64-bit rendered as 16 hex chars. Not cryptographic — this only has to
/// detect change, and it must not pull a hashing dependency into `protocol`.
pub fn content_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Hash of a tool set, insensitive to the order the tools were offered in.
///
/// Order-insensitivity matters: the agent builds its tool list from several
/// gated helpers whose order is not guaranteed, and an order-sensitive hash
/// would emit a spurious `request/header` on most steps.
pub fn tools_hash(names: &[String]) -> String {
    let mut sorted: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    sorted.sort_unstable();
    sorted.dedup();
    content_hash(&sorted.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dsh's SessionEventMap 8 core event strings. Aligning with them is the
    /// entire cost of ADR A3 — one typo and the session interchange it buys is
    /// gone, so the strings are pinned here.
    #[test]
    fn core_event_wire_strings_match_dsh() {
        let cases: Vec<(SessionEventPayload, &str)> = vec![
            (SessionEventPayload::TurnStart { turn: 1 }, "turn/start"),
            (SessionEventPayload::TurnEnd { turn: 1 }, "turn/end"),
            (
                SessionEventPayload::StepStart { step: 0, turn: 1 },
                "step/start",
            ),
            (
                SessionEventPayload::StepEnd { step: 0, turn: 1 },
                "step/end",
            ),
            (
                SessionEventPayload::SystemMessage {
                    content: String::new(),
                    sections: vec![],
                    origin: "kernel".into(),
                },
                "system/message",
            ),
            (
                SessionEventPayload::UserMessage {
                    content: String::new(),
                    attachments: vec![],
                    origin: "user".into(),
                },
                "user/message",
            ),
            (
                SessionEventPayload::AssistantMessage {
                    content: String::new(),
                    tool_calls: vec![],
                    usage: None,
                    model: "m".into(),
                    provider: "p".into(),
                },
                "assistant/message",
            ),
            (
                SessionEventPayload::ToolCall {
                    id: "t1".into(),
                    name: "read_file".into(),
                    args: serde_json::json!({}),
                    origin: ToolOrigin::model(),
                    tab_url: None,
                },
                "tool/call",
            ),
            (
                SessionEventPayload::ToolResult {
                    id: "t1".into(),
                    content: String::new(),
                    is_error: false,
                    duration_ms: 0,
                },
                "tool/result",
            ),
            (
                SessionEventPayload::RequestHeader {
                    provider: "p".into(),
                    model: "m".into(),
                    tools_hash: "h".into(),
                    reason: "initial".into(),
                },
                "request/header",
            ),
        ];
        for (payload, wire) in cases {
            assert_eq!(payload.type_str(), wire, "type_str mismatch");
            let json = serde_json::to_value(&payload).unwrap();
            assert_eq!(json["type"], wire, "serde tag mismatch for {wire}");
        }
    }

    #[test]
    fn event_row_flattens_seq_and_ts_next_to_type() {
        let ev = SessionEvent {
            seq: 7,
            ts: 1_757_000_000_000,
            payload: SessionEventPayload::TurnStart { turn: 2 },
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["seq"], 7);
        assert_eq!(json["ts"], 1_757_000_000_000i64);
        assert_eq!(json["type"], "turn/start");
        assert_eq!(json["turn"], 2);

        let back: SessionEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn tool_origin_renders_the_five_spec_forms() {
        assert_eq!(ToolOrigin::model().as_str(), "model");
        assert_eq!(
            ToolOrigin::canvas("okf/dashboard").as_str(),
            "canvas:okf/dashboard"
        );
        assert_eq!(ToolOrigin::mcp("filesystem").as_str(), "mcp:filesystem");
        assert_eq!(ToolOrigin::subagent("sa_1").as_str(), "subagent:sa_1");
        assert_eq!(ToolOrigin::loop_("lp_9").as_str(), "loop:lp_9");
    }

    #[test]
    fn tools_hash_is_order_insensitive_and_changes_with_membership() {
        let a = tools_hash(&["b".into(), "a".into()]);
        let b = tools_hash(&["a".into(), "b".into()]);
        assert_eq!(
            a, b,
            "the same tool set in a different order must hash the same, or every step reports a change"
        );
        let c = tools_hash(&["a".into()]);
        assert_ne!(a, c);
    }

    #[test]
    fn nevoflux_extension_events_use_distinct_prefixes() {
        for p in [
            SessionEventPayload::PackActivate { pack: "p".into() },
            SessionEventPayload::ContextCompact {
                trigger: "overflow".into(),
                before_tokens: 1,
                after_tokens: 0,
            },
            SessionEventPayload::ToolSpill {
                id: "t".into(),
                path: "p".into(),
                bytes: 1,
            },
        ] {
            let t = p.type_str();
            assert!(
                t.starts_with("pack/") || t.starts_with("context/") || t == "tool/spill",
                "unexpected extension event {t}"
            );
        }
    }
}
