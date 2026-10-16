//! What a task delegated by a paired agent may do only with the desktop's
//! say-so (M3 §4.2, contract v1.1 R25).
//!
//! First stage of the shared pipeline for a delegated session, so both tool
//! entry points (the native loop's `tool_pre`, `execute_mcp_tool`'s
//! `gate_pipeline`) judge a call before any other stage. Above the pairing's
//! tier: refused, nobody asked (R30) — for any tool, because a call can reach
//! `execute_mcp_tool` without the ACP bridge's permission request having
//! judged it. Within the tier: the ten must-approve tools are always asked —
//! the always-allow cache and a full-auto tier do not reach this stage — and
//! the question is marked must-approve (Task 13b), so the paired agent cannot
//! approve its own request unless the person turned the pairing's R39 toggle
//! on at the desktop. By tool name: a future tool that can schedule work,
//! change a pack, the model, a setting, a pairing, an account or an MCP server
//! has to be added here, and `every_model_tool_is_judged_for_delegated_turns`
//! fails until it is classified one way or the other.

use nevoflux_builtin_wasm::{ToolCall, ToolContext, ToolDenial};

use super::{Stage, Verdict};
use crate::remote::task::ceiling::Ceiling;

/// The refusal code above the pairing's tier (contract v1.1 R25/R30).
pub const ABOVE_TIER_CODE: &str = "DELEGATED_ABOVE_TIER";

/// Model tools a delegated turn may run only after the person at the desktop
/// allows that very call (contract v1.1 R25). Scheduling outlives the turn
/// (and the pairing); packs, the model, the system prompt and goals change
/// what later turns do; a screencast shares the screen.
pub const MUST_APPROVE: &[&str] = &[
    "schedule_create",
    "loop_create",
    "pack_activate",
    "pack_deactivate",
    "switch_model",
    "screencast_start",
    "schedule_run_now",
    "schedule_resume",
    "goal_set",
    "system_prompt_replace",
];

/// Whether `tool` is one of the ten that always needs the desktop's approval.
pub fn must_approve(tool: &str) -> bool {
    MUST_APPROVE.contains(&tool)
}

/// The question put to the desktop (offered `Allow` / `Deny` only).
pub fn prompt_for(tool: &str) -> String {
    format!("A task delegated by a paired agent wants to run {tool}. This action always needs approval.")
}

/// The pairing's ceiling and the must-approve list as a pipeline stage. Only
/// ever built for a delegated session (`default_pipeline_for(.., Some(ceiling))`).
///
/// Above the ceiling: any tool is refused without asking (R30). Within it:
/// the ten [`MUST_APPROVE`] tools are asked, everything else is left to the
/// later stages.
pub struct DelegatedTaskStage {
    /// The pairing's tier, read at the call (Task 4).
    pub ceiling: Ceiling,
}

impl Stage for DelegatedTaskStage {
    fn name(&self) -> &'static str {
        "delegated_task"
    }

    fn check(&self, call: &ToolCall, _ctx: &ToolContext) -> Verdict {
        // `tool_call_dynamic` names the tool it will run; judge that one.
        // On the ACP path the inner call is gated again on its own, but the
        // native loop's `tool_call_dynamic` is not.
        let name = if call.name == "tool_call_dynamic" {
            call.arguments
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
        } else {
            call.name.as_str()
        };
        if !(self.ceiling)(name) {
            return Verdict::Deny(ToolDenial {
                code: ABOVE_TIER_CODE.into(),
                message: crate::remote::task::ceiling::refusal(name),
                rule: None,
                pack: None,
            });
        }
        if must_approve(name) {
            return Verdict::Ask {
                prompt: prompt_for(name),
            };
        }
        Verdict::Allow
    }
}

/// Model tools judged safe to leave to the tier in a delegated turn, as of
/// `bbaa7b0` (every `name:` in `crates/builtin-wasm/src` minus test fixtures
/// and the ten on `MUST_APPROVE`). Only the catalogue test reads it.
#[cfg(test)]
const REVIEWED_ALLOWED: &[&str] = &[
    "act",
    "ask_user",
    "bash",
    "brain_search",
    "browser_activate_tab",
    "browser_click",
    "browser_click_by_id",
    "browser_console_messages",
    "browser_edit_artifact",
    "browser_element_info",
    "browser_eval_js",
    "browser_fill",
    "browser_fill_by_id",
    "browser_find_elements",
    "browser_get_content",
    "browser_get_elements",
    "browser_get_markdown",
    "browser_get_tabs",
    "browser_go_back",
    "browser_go_forward",
    "browser_input",
    "browser_navigate",
    "browser_network_capture_stop",
    "browser_network_requests",
    "browser_probe",
    "browser_read_artifact",
    "browser_screenshot",
    "browser_scroll",
    "browser_type",
    "browser_type_by_id",
    "browser_upload_file",
    "browser_wait_for",
    "canvas_apply_design_md",
    "canvas_attach_asset",
    "canvas_create_composition",
    "canvas_create_from_visual_identity",
    "canvas_eval",
    "canvas_extract_visual_identity",
    "canvas_inspect_layout",
    "canvas_lint_composition",
    "canvas_render_video",
    "computer_click",
    "computer_cursor_position",
    "computer_drag",
    "computer_hold_key",
    "computer_key",
    "computer_mouse_down",
    "computer_mouse_move",
    "computer_mouse_up",
    "computer_screenshot",
    "computer_scroll",
    "computer_type_text",
    "computer_wait",
    "create_artifact",
    "edit",
    "glob",
    "goal_clear",
    "goal_status",
    "grep",
    "list_agents",
    "list_flows",
    "load_computer_use_tools",
    "load_selector_tools",
    "loop_cancel",
    "loop_evolve",
    "loop_list",
    "loop_proposal_respond",
    "loop_scratchpad_get",
    "loop_scratchpad_set",
    "memory_create",
    "memory_delete",
    "memory_search",
    "memory_update",
    "memory_view",
    "notify_user",
    "orchestrate",
    "pack_list_active",
    "plan",
    "play_local_file",
    "read",
    "read_file",
    "recall",
    "report_flow_repair",
    "run_flow",
    "schedule_cancel",
    "schedule_list",
    "schedule_pause",
    "schedule_runs",
    "screencast_stop",
    "skill_load",
    "start_recording",
    "stop_recording",
    "subagent_kill",
    "subagent_list",
    "subagent_spawn",
    "subagent_status",
    "subagent_wait",
    "subagent_wait_all",
    "system_prompt_restore",
    "think",
    "tool_call_dynamic",
    "tool_search",
    "tts_synthesize_api",
    "tts_synthesize_local",
    "tts_transcribe",
    "tts_voices",
    "web_fetch",
    "web_search",
    "write",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn the_must_approve_list_is_the_contracts() {
        assert_eq!(
            MUST_APPROVE,
            &[
                "schedule_create",
                "loop_create",
                "pack_activate",
                "pack_deactivate",
                "switch_model",
                "screencast_start",
                "schedule_run_now",
                "schedule_resume",
                "goal_set",
                "system_prompt_replace"
            ]
        );
        for tool in [
            "schedule_list",
            "loop_list",
            "pack_list_active",
            "browser_click",
            "goal_status",
        ] {
            assert!(!must_approve(tool), "{tool}");
        }
        assert_eq!(
            prompt_for("schedule_create"),
            "A task delegated by a paired agent wants to run schedule_create. This action always needs approval."
        );
    }

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            call_id: None,
            name: name.into(),
            arguments,
            signature: None,
        }
    }

    fn ctx() -> ToolContext {
        ToolContext {
            session_id: "s-del".into(),
            origin: "model".into(),
            mode: nevoflux_builtin_wasm::AgentMode::Agent,
            is_unattended: false,
            tab_url: None,
            allowed_tools: None,
        }
    }

    fn stage(within: bool) -> DelegatedTaskStage {
        DelegatedTaskStage {
            ceiling: Arc::new(move |_: &str| within),
        }
    }

    #[test]
    fn within_the_tier_every_listed_tool_is_asked() {
        for tool in MUST_APPROVE {
            match stage(true).check(&call(tool, serde_json::json!({})), &ctx()) {
                Verdict::Ask { prompt } => assert_eq!(prompt, prompt_for(tool)),
                _ => panic!("{tool} was not asked"),
            }
        }
        assert!(matches!(
            stage(true).check(&call("browser_click", serde_json::json!({})), &ctx()),
            Verdict::Allow
        ));
    }

    #[test]
    fn above_the_tier_a_listed_tool_is_refused_without_asking() {
        for tool in MUST_APPROVE {
            match stage(false).check(&call(tool, serde_json::json!({})), &ctx()) {
                Verdict::Deny(d) => {
                    assert_eq!(d.code, ABOVE_TIER_CODE, "{tool}");
                    assert_eq!(d.message, crate::remote::task::ceiling::refusal(tool));
                }
                _ => panic!("{tool} was not refused"),
            }
        }
    }

    #[test]
    fn above_the_tier_any_tool_is_refused_not_only_the_listed() {
        assert!(matches!(
            stage(false).check(&call("write_file", serde_json::json!({})), &ctx()),
            Verdict::Deny(d) if d.code == ABOVE_TIER_CODE
        ));
        let wrapped = call(
            "tool_call_dynamic",
            serde_json::json!({"tool_name": "write_file"}),
        );
        assert!(matches!(
            stage(false).check(&wrapped, &ctx()),
            Verdict::Deny(_)
        ));
    }

    #[test]
    fn a_wrapped_call_is_judged_by_what_it_wraps() {
        let wrapped = call(
            "tool_call_dynamic",
            serde_json::json!({"tool_name": "loop_create", "arguments": "{}"}),
        );
        assert!(matches!(
            stage(true).check(&wrapped, &ctx()),
            Verdict::Ask { .. }
        ));
        assert!(matches!(
            stage(false).check(&wrapped, &ctx()),
            Verdict::Deny(_)
        ));
        let harmless = call(
            "tool_call_dynamic",
            serde_json::json!({"tool_name": "brain_search"}),
        );
        assert!(matches!(
            stage(true).check(&harmless, &ctx()),
            Verdict::Allow
        ));
    }

    /// Resolution (c): every tool the model can be offered is on one side or
    /// the other. A name this fails on is a tool nobody has judged: read what
    /// it does, then add it to `MUST_APPROVE` if it can create schedules or
    /// loops, run or resume one, change a pack, the model, the system prompt,
    /// a goal, a setting (`config:*`), a pairing, an account or an MCP server,
    /// or share the screen — otherwise to `REVIEWED_ALLOWED` — and change
    /// contract R25 to match.
    #[test]
    fn every_model_tool_is_judged_for_delegated_turns() {
        use nevoflux_builtin_wasm::AgentMode;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = std::sync::Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let host = crate::agent_host::DaemonHostFunctions::new(
            std::sync::Arc::new(crate::config::AgentConfig::default()),
            rt.handle().clone(),
        )
        .with_services(crate::wasm::services::HostServices::new(db));
        let agent = nevoflux_builtin_wasm::Agent::new(host);
        let mut seen = std::collections::BTreeSet::new();
        // `AgentMode::Code` is deprecated and maps to Agent: nothing more to list.
        for mode in [AgentMode::Chat, AgentMode::Browser, AgentMode::Agent] {
            seen.extend(agent.get_tools_for_mode(mode).into_iter().map(|t| t.name));
        }
        let unjudged: Vec<&String> = seen
            .iter()
            .filter(|n| !must_approve(n) && !REVIEWED_ALLOWED.contains(&n.as_str()))
            .collect();
        assert!(
            unjudged.is_empty(),
            "model tools nobody classified for delegated turns: {unjudged:?}"
        );
        for name in MUST_APPROVE {
            assert!(
                seen.contains(*name),
                "{name} must be approved but is no longer a model tool: a rename would slip past the list"
            );
            assert!(!REVIEWED_ALLOWED.contains(name), "{name} is on both lists");
        }
    }
}
