//! gbrain tool subset exposed to the nevoflux agent (M3-4).
//!
//! This module is the single source of truth for which gbrain MCP tools
//! the daemon advertises to the agent. M3-4 ships the curated read-only
//! / no-LLM subset (12 tools); future tasks may extend the surface via
//! explicit user opt-in.
//!
//! See `spike/notes/tools-classification.md` for the canonical
//! `read_only=yes, needs_llm=no` list — every tool here is in that set,
//! which makes the default exposure safe:
//!
//!   - Safe-default: no token billing, no mutations, no destructive ops.
//!   - Cheap: ~all are simple DB reads (`get_*`, `list_*`, `whoami`).
//!   - Opt-in only: any mutation / LLM-billing / destructive tool must
//!     be added in a future M3-5/M4 task with explicit user consent.
//!
//! Routing layout
//!
//! Each tool here has a `nevoflux_name` prefixed with `brain_` (to avoid
//! collisions with same-named native tools) and a `gbrain_name` (the
//! actual MCP tool name gbrain advertises). [`invoke_brain_tool`] is the
//! single dispatch entry point used by the daemon's tool routers
//! (`mcp_tool_executor::execute_mcp_tool` for ACP/MCP-HTTP, and
//! `agent_host::tool_call_dynamic` for WASM-direct).

use std::sync::Arc;

use serde_json::{json, Value};
use tracing::warn;

use crate::gbrain::GbrainSupervisor;

/// Static description of one gbrain-backed tool exposed to nevoflux's
/// agent. Tools are pure metadata — the actual dispatch lives in
/// [`invoke_brain_tool`].
pub struct BrainToolDef {
    /// nevoflux-side tool name (with `brain_` prefix, agent-visible).
    pub nevoflux_name: &'static str,
    /// gbrain-side tool name (no prefix, what gbrain advertises).
    pub gbrain_name: &'static str,
    /// Short human description shown to the agent.
    pub description: &'static str,
    /// JSON Schema for inputs. Mirrors what gbrain advertises (the
    /// agent could also fetch via tools/list, but caching here keeps
    /// the agent-loop fast and avoids an extra round-trip per turn).
    pub input_schema: fn() -> Value,
}

/// The 12 default read-only / no-LLM tools to expose by default.
///
/// All 12 are `read_only=yes, needs_llm=no` per
/// `spike/notes/tools-classification.md`. This list is intentionally
/// short and curated; adding a tool here requires confirming it does
/// not mutate the brain and does not trigger LLM billing.
pub const DEFAULT_TOOLS: &[BrainToolDef] = &[
    BrainToolDef {
        nevoflux_name: "brain_get_page",
        gbrain_name: "get_page",
        description: "Fetch a single page by slug from the knowledge base. Read-only, no LLM call.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "slug": { "type": "string", "description": "The page slug" },
                    "fuzzy": {
                        "type": "boolean",
                        "description": "Allow fuzzy slug matching",
                        "default": false
                    }
                },
                "required": ["slug"]
            })
        },
    },
    BrainToolDef {
        nevoflux_name: "brain_list_pages",
        gbrain_name: "list_pages",
        description: "List pages in the knowledge base with optional filters (tag, type, recency). Read-only, no LLM call.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "default": 50, "maximum": 500 },
                    "sort": {
                        "type": "string",
                        "enum": ["updated_desc", "slug_asc"],
                        "default": "updated_desc"
                    },
                    "tag": { "type": "string" },
                    "type": { "type": "string" },
                    "updated_after": { "type": "string", "format": "date-time" }
                }
            })
        },
    },
    BrainToolDef {
        nevoflux_name: "brain_get_stats",
        gbrain_name: "get_stats",
        description: "Get aggregate counters for the knowledge base (page count, chunk count, embedding count). Read-only.",
        input_schema: || json!({ "type": "object", "properties": {} }),
    },
    BrainToolDef {
        nevoflux_name: "brain_get_health",
        gbrain_name: "get_health",
        description: "Get a dashboard-style health summary of the knowledge base (last sync time, error counts, etc). Read-only.",
        input_schema: || json!({ "type": "object", "properties": {} }),
    },
    BrainToolDef {
        nevoflux_name: "brain_identity",
        gbrain_name: "get_brain_identity",
        description: "Get the brain's identity banner (name, model, schema pack version). Read-only.",
        input_schema: || json!({ "type": "object", "properties": {} }),
    },
    BrainToolDef {
        nevoflux_name: "brain_get_chunks",
        gbrain_name: "get_chunks",
        description: "Fetch the chunks for a specific page (text + embedding metadata). Read-only.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "slug": { "type": "string" }
                },
                "required": ["slug"]
            })
        },
    },
    BrainToolDef {
        nevoflux_name: "brain_resolve_slugs",
        gbrain_name: "resolve_slugs",
        description: "Fuzzy-match candidate slug strings to real page slugs (pg_trgm-backed). Read-only.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "candidates": {
                        "type": "array",
                        "items": { "type": "string" }
                    }
                },
                "required": ["candidates"]
            })
        },
    },
    BrainToolDef {
        nevoflux_name: "brain_get_links",
        gbrain_name: "get_links",
        description: "Get the outbound links from a page (slug -> [slug...]). Read-only.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "slug": { "type": "string" }
                },
                "required": ["slug"]
            })
        },
    },
    BrainToolDef {
        nevoflux_name: "brain_get_backlinks",
        gbrain_name: "get_backlinks",
        description: "Get the inbound backlinks to a page (which pages link here). Read-only.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "slug": { "type": "string" }
                },
                "required": ["slug"]
            })
        },
    },
    BrainToolDef {
        nevoflux_name: "brain_get_tags",
        gbrain_name: "get_tags",
        description: "List all tags in the brain, optionally filtered. Read-only.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "prefix": { "type": "string" }
                }
            })
        },
    },
    BrainToolDef {
        nevoflux_name: "brain_get_timeline",
        gbrain_name: "get_timeline",
        description: "Fetch the append-only timeline entries for a page. Read-only.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "slug": { "type": "string" }
                },
                "required": ["slug"]
            })
        },
    },
    BrainToolDef {
        nevoflux_name: "brain_whoami",
        gbrain_name: "whoami",
        description: "Brain identity introspection - returns the active user / agent identity. Read-only.",
        input_schema: || json!({ "type": "object", "properties": {} }),
    },
];

/// Look up a brain tool definition by its nevoflux-side name (the
/// `brain_*` prefixed name the agent calls). Returns [`None`] if the
/// name is not in the default subset, which the dispatcher should
/// treat as "not a brain tool — fall through to the next router arm".
pub fn lookup_by_nevoflux_name(name: &str) -> Option<&'static BrainToolDef> {
    DEFAULT_TOOLS.iter().find(|t| t.nevoflux_name == name)
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
            // Join text parts; image/resource parts are rare for the
            // 12 read-only tools but stringified as a fallback.
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

    #[test]
    fn default_tools_count_is_twelve() {
        assert_eq!(DEFAULT_TOOLS.len(), 12);
    }

    #[test]
    fn nevoflux_names_all_brain_prefixed() {
        for t in DEFAULT_TOOLS {
            assert!(
                t.nevoflux_name.starts_with("brain_"),
                "tool {} must start with brain_ prefix",
                t.nevoflux_name
            );
        }
    }

    #[test]
    fn nevoflux_names_are_unique() {
        let mut names: Vec<&str> = DEFAULT_TOOLS.iter().map(|t| t.nevoflux_name).collect();
        names.sort();
        let count_before = names.len();
        names.dedup();
        assert_eq!(
            count_before,
            names.len(),
            "duplicate nevoflux_name in DEFAULT_TOOLS"
        );
    }

    #[test]
    fn gbrain_names_match_classification_list() {
        let expected = [
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
        ];
        let actual: Vec<&str> = DEFAULT_TOOLS.iter().map(|t| t.gbrain_name).collect();
        for name in expected {
            assert!(
                actual.contains(&name),
                "missing gbrain tool {name} from DEFAULT_TOOLS"
            );
        }
        assert_eq!(
            actual.len(),
            expected.len(),
            "DEFAULT_TOOLS contains tools outside the classification list"
        );
    }

    #[test]
    fn input_schemas_are_well_formed_json_schema() {
        for t in DEFAULT_TOOLS {
            let schema = (t.input_schema)();
            assert_eq!(
                schema["type"], "object",
                "tool {} input_schema must be object",
                t.nevoflux_name
            );
            assert!(
                schema["properties"].is_object(),
                "tool {} must have properties",
                t.nevoflux_name
            );
        }
    }

    #[test]
    fn no_destructive_tools_in_default() {
        let destructive = [
            "delete_page",
            "purge_deleted_pages",
            "sources_remove",
            "forget_fact",
            "put_page",
            "add_tag",
            "remove_tag",
        ];
        for t in DEFAULT_TOOLS {
            assert!(
                !destructive.contains(&t.gbrain_name),
                "destructive tool {} must NOT be in default subset",
                t.gbrain_name
            );
        }
    }

    #[test]
    fn no_llm_billing_tools_in_default() {
        let billable = [
            "search",
            "query",
            "think",
            "extract_facts",
            "search_by_image",
            "run_doctor",
            "sync_brain",
            "submit_job",
            "submit_agent",
            "replay_job",
        ];
        for t in DEFAULT_TOOLS {
            assert!(
                !billable.contains(&t.gbrain_name),
                "LLM-billing tool {} must NOT be in default subset",
                t.gbrain_name
            );
        }
    }

    #[test]
    fn lookup_by_nevoflux_name_finds_known_tools() {
        let def = lookup_by_nevoflux_name("brain_get_page")
            .expect("brain_get_page must be present in DEFAULT_TOOLS");
        assert_eq!(def.gbrain_name, "get_page");

        let def = lookup_by_nevoflux_name("brain_whoami")
            .expect("brain_whoami must be present in DEFAULT_TOOLS");
        assert_eq!(def.gbrain_name, "whoami");
    }

    #[test]
    fn lookup_rejects_unknown_and_unprefixed_names() {
        assert!(lookup_by_nevoflux_name("brain_nonexistent").is_none());
        // Unprefixed gbrain names must NOT resolve — agents must call
        // via the brain_ prefix, never via the bare gbrain name.
        assert!(lookup_by_nevoflux_name("get_page").is_none());
        // Empty string is the canonical not-found case.
        assert!(lookup_by_nevoflux_name("").is_none());
    }

    #[test]
    fn descriptions_are_non_empty() {
        // The agent surfaces these to the LLM as tool-selection guidance;
        // an empty description would silently degrade tool routing.
        for t in DEFAULT_TOOLS {
            assert!(
                !t.description.trim().is_empty(),
                "tool {} has empty description",
                t.nevoflux_name
            );
        }
    }
}
