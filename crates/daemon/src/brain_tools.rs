//! Full gbrain tool surface exposed to the nevoflux agent (M4-B).
//!
//! This module is the single source of truth for which gbrain MCP tools
//! the daemon advertises to the agent. M3-4 shipped a curated 12-tool
//! read-only subset; M4-B expands this to the **complete** gbrain MCP
//! surface (all tools the live `gbrain serve` advertises via
//! `tools/list`), per the user's explicit decision to expose the full
//! knowledge-base toolset to the sidebar agent.
//!
//! Source of truth
//!
//! Rather than hand-transcribe ~80 tool schemas, the catalog is loaded
//! at startup from a committed JSON resource
//! (`resources/gbrain-tools.json`) captured from the live gbrain
//! `tools/list` response during the M4 spike. Each entry becomes a
//! [`BrainToolDef`]:
//!
//!   - `nevoflux_name = format!("brain_{}", gbrain_name)` — the
//!     agent-visible name, prefixed to avoid collisions with native
//!     nevoflux tools.
//!   - `gbrain_name` — the bare name gbrain advertises.
//!   - `description` — gbrain's description, prefixed with
//!     "[Knowledge Base] " so the agent + user can distinguish brain
//!     tools from native ones in the tool list.
//!   - `input_schema` — gbrain's `inputSchema` verbatim.
//!
//! Safety note
//!
//! The full surface now includes destructive tools (`brain_delete_page`,
//! `brain_purge_deleted_pages`, `brain_forget_fact`, `brain_sources_remove`)
//! and token-billing tools (`brain_search`, `brain_query`, `brain_think`,
//! `brain_submit_job` / `brain_submit_agent`). This is the user's
//! explicit decision; there is intentionally **no** per-tool gating in
//! this module — a permission-gate pass is deferred to a future task.
//!
//! Routing layout
//!
//! Each tool here has a `nevoflux_name` prefixed with `brain_` and a
//! `gbrain_name`. [`invoke_brain_tool`] is the single dispatch entry
//! point used by the daemon's tool routers
//! (`mcp_tool_executor::execute_mcp_tool` for ACP/MCP-HTTP, and
//! `agent_host::tool_call_dynamic` for WASM-direct). Dispatch is
//! name-agnostic: any `brain_<name>` resolved by
//! [`lookup_by_nevoflux_name`] is forwarded to gbrain.

use std::sync::{Arc, LazyLock};

use serde_json::Value;
use tracing::warn;

use crate::gbrain::GbrainSupervisor;

/// Prefix applied to every gbrain description so the agent and the user
/// can tell knowledge-base tools apart from native nevoflux tools.
const KB_DESCRIPTION_PREFIX: &str = "[Knowledge Base] ";

/// Embedded gbrain `tools/list` response, captured from the live gbrain
/// server during the M4 spike. This is a committed product artifact.
const GBRAIN_TOOLS_JSON: &str = include_str!("resources/gbrain-tools.json");

/// Description of one gbrain-backed tool exposed to nevoflux's agent.
///
/// Tools are pure metadata — the actual dispatch lives in
/// [`invoke_brain_tool`]. Unlike the M3-4 version (which used
/// `&'static str` + a schema-builder `fn`), the fields are now owned
/// because the catalog is parsed from JSON at runtime.
#[derive(Debug, Clone)]
pub struct BrainToolDef {
    /// nevoflux-side tool name (with `brain_` prefix, agent-visible).
    pub nevoflux_name: String,
    /// gbrain-side tool name (no prefix, what gbrain advertises).
    pub gbrain_name: String,
    /// Human description shown to the agent (with `[Knowledge Base] `
    /// prefix applied).
    pub description: String,
    /// JSON Schema for inputs, verbatim from gbrain's `inputSchema`.
    pub input_schema: Value,
}

/// Shape of the embedded JSON: the raw MCP `tools/list` JSON-RPC
/// envelope, `{ "result": { "tools": [ {name, description, inputSchema} ] } }`.
#[derive(serde::Deserialize)]
struct ToolsListEnvelope {
    result: ToolsListResult,
}

#[derive(serde::Deserialize)]
struct ToolsListResult {
    tools: Vec<RawTool>,
}

#[derive(serde::Deserialize)]
struct RawTool {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(rename = "inputSchema", default)]
    input_schema: Value,
}

/// Parse the embedded gbrain tool catalog into [`BrainToolDef`]s.
///
/// Panics at first use if the embedded JSON is malformed — that is a
/// build-time/resource invariant, not a runtime condition, so a panic
/// (surfacing the exact parse error) is the correct failure mode.
fn load_catalog() -> Vec<BrainToolDef> {
    let envelope: ToolsListEnvelope = serde_json::from_str(GBRAIN_TOOLS_JSON)
        .expect("embedded resources/gbrain-tools.json must be a valid tools/list envelope");

    envelope
        .result
        .tools
        .into_iter()
        .map(|raw| {
            let input_schema = if raw.input_schema.is_null() {
                // Defensive: gbrain always sends inputSchema, but never
                // expose a tool with a missing schema — fall back to an
                // empty object schema so the agent gets valid JSON Schema.
                serde_json::json!({ "type": "object", "properties": {} })
            } else {
                raw.input_schema
            };
            BrainToolDef {
                nevoflux_name: format!("brain_{}", raw.name),
                description: format!("{KB_DESCRIPTION_PREFIX}{}", raw.description),
                gbrain_name: raw.name,
                input_schema,
            }
        })
        .collect()
}

/// The complete gbrain tool catalog exposed to the agent, parsed once
/// from the embedded resource. This replaces M3-4's hardcoded 12-entry
/// `DEFAULT_TOOLS` array.
pub static BRAIN_TOOLS: LazyLock<Vec<BrainToolDef>> = LazyLock::new(load_catalog);

/// All brain tools as MCP-shaped tool definitions, for indexing into the
/// agent's `tool_search` discovery index. Returns `(nevoflux_name,
/// description, input_schema)` tuples; the caller constructs whatever
/// concrete `ToolDefinition` type its index requires.
pub fn tool_catalog() -> &'static [BrainToolDef] {
    &BRAIN_TOOLS
}

/// Look up a brain tool definition by its nevoflux-side name (the
/// `brain_*` prefixed name the agent calls). Returns [`None`] if the
/// name is not in the catalog, which the dispatcher treats as "not a
/// brain tool — fall through to the next router arm".
pub fn lookup_by_nevoflux_name(name: &str) -> Option<&'static BrainToolDef> {
    BRAIN_TOOLS.iter().find(|t| t.nevoflux_name == name)
}

/// Translate an `arguments` object from the agent into a gbrain
/// tools/call invocation, then route the response back. Returns a
/// JSON-string tool result that the agent can ingest, OR an error
/// string suitable for surfacing to the LLM.
///
/// The returned `Ok(String)` is the raw JSON of `result.content[0].text`
/// when present (matching how external MCP tools surface results
/// elsewhere in the daemon); when the gbrain response doesn't have a
/// content array we fall back to the stringified envelope.
pub async fn invoke_brain_tool(
    supervisor: &Arc<GbrainSupervisor>,
    gbrain_name: &str,
    arguments: Value,
) -> Result<String, String> {
    let response = supervisor
        .call_tool(gbrain_name, arguments)
        .await
        .map_err(|e| {
            warn!(tool = gbrain_name, error = %e, "brain tool call failed");
            format!("gbrain tool {gbrain_name} failed: {e}")
        })?;

    // gbrain returns a JSON-RPC envelope; `result` is the MCP
    // `tools/call` response shape: { content: [{ type: "text", text }],
    // isError?: bool }. Pull the user-facing payload out so the agent
    // doesn't have to peel the envelope itself.
    if let Some(result) = response.get("result") {
        if let Some(is_error) = result.get("isError").and_then(|v| v.as_bool()) {
            if is_error {
                let msg = result
                    .get("content")
                    .and_then(|c| c.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|c| c.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("brain tool reported an error");
                return Err(format!("gbrain tool {gbrain_name} error: {msg}"));
            }
        }
        if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
            // Join text parts; image/resource parts are stringified as a
            // fallback.
            let joined = content
                .iter()
                .filter_map(|part| {
                    let kind = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match kind {
                        "text" => part
                            .get("text")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string()),
                        _ => Some(part.to_string()),
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !joined.is_empty() {
                return Ok(joined);
            }
        }
        return Ok(result.to_string());
    }

    // Unexpected shape: return the whole envelope so the agent can at
    // least see something rather than a generic error.
    Ok(response.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded catalog must parse and contain the full gbrain
    /// surface. We assert against the count actually present in the
    /// JSON (computed at parse time) rather than a magic number, so the
    /// test tracks the resource if it is ever refreshed.
    #[test]
    fn catalog_loads_full_surface() {
        // Re-parse the raw envelope independently to derive the expected
        // count, then confirm BRAIN_TOOLS matches it 1:1.
        let envelope: ToolsListEnvelope =
            serde_json::from_str(GBRAIN_TOOLS_JSON).expect("embedded JSON must parse");
        let expected = envelope.result.tools.len();
        assert!(
            expected >= 80,
            "expected the full gbrain surface (~83 tools), got {expected}"
        );
        assert_eq!(BRAIN_TOOLS.len(), expected);
    }

    #[test]
    fn every_name_is_brain_prefixed() {
        for t in tool_catalog() {
            assert!(
                t.nevoflux_name.starts_with("brain_"),
                "tool {} must start with brain_ prefix",
                t.nevoflux_name
            );
            // gbrain_name must be the prefix-stripped form.
            assert_eq!(t.nevoflux_name, format!("brain_{}", t.gbrain_name));
        }
    }

    #[test]
    fn nevoflux_names_are_unique() {
        let mut names: Vec<&str> = tool_catalog()
            .iter()
            .map(|t| t.nevoflux_name.as_str())
            .collect();
        names.sort_unstable();
        let count_before = names.len();
        names.dedup();
        assert_eq!(count_before, names.len(), "duplicate nevoflux_name in catalog");
    }

    #[test]
    fn every_input_schema_is_non_empty_object() {
        for t in tool_catalog() {
            assert_eq!(
                t.input_schema["type"], "object",
                "tool {} input_schema must be a JSON object schema",
                t.nevoflux_name
            );
            assert!(
                t.input_schema.get("properties").is_some(),
                "tool {} input_schema must have a properties field",
                t.nevoflux_name
            );
            // Non-empty in the sense that it is a real object value, not
            // null/string — the well-formedness invariant the agent loop
            // relies on when presenting the schema to the LLM.
            assert!(
                t.input_schema.is_object(),
                "tool {} input_schema must be a JSON object",
                t.nevoflux_name
            );
        }
    }

    #[test]
    fn descriptions_are_kb_prefixed_and_non_empty() {
        for t in tool_catalog() {
            assert!(
                t.description.starts_with(KB_DESCRIPTION_PREFIX),
                "tool {} description must carry the [Knowledge Base] prefix",
                t.nevoflux_name
            );
            // After stripping the prefix there must still be real text
            // (the agent surfaces this to the LLM for tool selection).
            let body = &t.description[KB_DESCRIPTION_PREFIX.len()..];
            assert!(
                !body.trim().is_empty(),
                "tool {} has an empty description body",
                t.nevoflux_name
            );
        }
    }

    /// M3-4 had `no_destructive_tools_in_default`; M4-B intentionally
    /// INVERTS that — the full surface MUST now include the destructive
    /// tools, proving the expansion worked.
    #[test]
    fn catalog_includes_destructive_tools() {
        let destructive = [
            "delete_page",
            "purge_deleted_pages",
            "sources_remove",
            "forget_fact",
        ];
        for name in destructive {
            assert!(
                lookup_by_nevoflux_name(&format!("brain_{name}")).is_some(),
                "expected destructive tool brain_{name} in the full catalog"
            );
        }
    }

    /// M3-4 had `no_llm_billing_tools_in_default`; M4-B INVERTS it — the
    /// full surface MUST now include the LLM/token-billing tools.
    #[test]
    fn catalog_includes_llm_billing_tools() {
        let billable = ["search", "query", "think", "submit_job", "submit_agent"];
        for name in billable {
            assert!(
                lookup_by_nevoflux_name(&format!("brain_{name}")).is_some(),
                "expected LLM-billing tool brain_{name} in the full catalog"
            );
        }
    }

    #[test]
    fn read_only_subset_still_present() {
        // The original M3-4 read-only tools must still resolve.
        for name in [
            "get_page",
            "list_pages",
            "get_stats",
            "get_health",
            "get_brain_identity",
            "get_chunks",
            "resolve_slugs",
            "get_links",
            "get_backlinks",
            "get_tags",
            "get_timeline",
            "whoami",
        ] {
            assert!(
                lookup_by_nevoflux_name(&format!("brain_{name}")).is_some(),
                "read-only tool brain_{name} must remain in the catalog"
            );
        }
    }

    #[test]
    fn lookup_by_nevoflux_name_finds_known_tools() {
        let def = lookup_by_nevoflux_name("brain_get_page")
            .expect("brain_get_page must be present in the catalog");
        assert_eq!(def.gbrain_name, "get_page");

        let def = lookup_by_nevoflux_name("brain_search")
            .expect("brain_search must be present in the catalog");
        assert_eq!(def.gbrain_name, "search");
    }

    #[test]
    fn lookup_rejects_unknown_and_unprefixed_names() {
        assert!(lookup_by_nevoflux_name("brain_nonexistent").is_none());
        // Unprefixed gbrain names must NOT resolve — agents must call via
        // the brain_ prefix, never via the bare gbrain name.
        assert!(lookup_by_nevoflux_name("get_page").is_none());
        assert!(lookup_by_nevoflux_name("").is_none());
    }
}
