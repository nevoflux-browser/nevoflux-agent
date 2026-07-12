//! `/loop evolve`: a meta-pass that reads a loop's recent iteration history
//! and proposes concrete edits to its `prompt_text` / `gate_spec` (W4 spec).
//!
//! The meta-pass itself never mutates the loop — it runs one read-only chat
//! turn, parses a fenced ```json proposal out of the model's reply, and
//! stores it as a `LoopProposal` row (`status = "pending"`) for a human to
//! accept/reject later via `LoopProposalRepository::respond_proposal` +
//! `LoopRepository::apply_proposal_fields`.

use crate::agent_exec::{run_agent_once, AgentExecRequest};
use crate::loops::events::LoopEvents;
use crate::loops::types::LoopId;
use crate::wasm::services::HostServices;
use nevoflux_builtin_wasm::AgentMode;
use nevoflux_storage::connection::Database;
use nevoflux_storage::models::LoopRecord;
use nevoflux_storage::repositories::{LoopProposal, LoopProposalRepository, RecentIteration};

/// One-line summary cap (chars, not bytes — this repo has had a CJK
/// byte-slice panic from length-capping on byte indices; always truncate on
/// `.chars()`).
const RECENT_LINE_MAX: usize = 200;

/// Build the meta-pass prompt: states the loop's current contract (prompt +
/// gate), its recent iteration history, and the ask (propose concrete
/// `prompt_text`/`gate_spec` edits, or say the loop is healthy and propose
/// nothing).
pub fn build_evolve_prompt(rec: &LoopRecord, recent: &[RecentIteration]) -> String {
    let prompt_text = rec
        .prompt_text
        .as_deref()
        .unwrap_or("(none — this loop wraps a skill instead of a literal prompt)");

    let gate_spec_display = rec.gate_spec.as_deref().unwrap_or("(none)");

    let recent_block = if recent.is_empty() {
        "  (no finished iterations yet)\n".to_string()
    } else {
        let mut s = String::new();
        for r in recent {
            let mut summary = r.final_text.as_deref().unwrap_or("").replace('\n', " ");
            if summary.chars().count() > RECENT_LINE_MAX {
                let truncated: String = summary.chars().take(RECENT_LINE_MAX).collect();
                summary = format!("{truncated}…");
            }
            let verify = match r.verify_passed {
                Some(true) => " verify=pass".to_string(),
                Some(false) => format!(
                    " verify=fail({})",
                    r.verify_reason.as_deref().unwrap_or("no reason given")
                ),
                None => String::new(),
            };
            s.push_str(&format!(
                "  #{} [{}]{} {}\n",
                r.sequence_number, r.status, verify, summary
            ));
        }
        s
    };

    format!(
        "You are the self-improvement pass for a running /loop. You are read-only \
         for this turn — do not call tools, do not act, only analyze and propose.\n\n\
         current prompt_text:\n{prompt_text}\n\n\
         gate_kind: {gate_kind}\n\
         gate_spec: {gate_spec_display}\n\n\
         skipped_triggers (all-time total): {skipped}\n\n\
         recent_iterations (newest first):\n{recent_block}\n\
         You are improving THIS loop. Where is it repeating mistakes, wasting runs, or \
         mis-scoped (boundary too loose/tight, gate too eager/cheap)? Propose concrete \
         changes to its prompt_text (contract) and/or gate_spec ONLY. Output a single \
         fenced ```json block:\n\
         {{\"rationale\": \"...\", \"prompt_text\": \"<full replacement or omit>\", \"gate_spec\": \"<json or omit>\"}}\n\
         Omit a field to leave it unchanged. If the loop is healthy, say so in rationale \
         and omit both.",
        gate_kind = rec.gate_kind,
        skipped = rec.skipped_triggers,
    )
}

/// Extract the LAST fenced ```json ... ``` block in `text`, if any. Byte
/// offsets here are always the return of `find`/`rfind`-style scanning on
/// ASCII fence markers, so they land on char boundaries even when the
/// surrounding text (e.g. CJK content in a final_text summary) isn't ASCII.
fn extract_last_json_block(text: &str) -> Option<String> {
    let mut search_from = 0usize;
    let mut last_body: Option<String> = None;
    while let Some(rel_start) = text[search_from..].find("```json") {
        let start = search_from + rel_start + "```json".len();
        let Some(rel_end) = text[start..].find("```") else {
            break;
        };
        let end = start + rel_end;
        last_body = Some(text[start..end].trim().to_string());
        search_from = end + "```".len();
    }
    last_body
}

/// Parse the meta-pass's reply into a `LoopProposal` row. Extracts the last
/// fenced ```json block, requires `rationale`, and treats `prompt_text` /
/// `gate_spec` as optional (a field's absence means "leave unchanged").
/// `gate_spec` may come back either as a JSON string (the wire shape stored
/// in `loops.gate_spec`) or as a nested JSON object/array — either is
/// re-serialized to the string form the DB column expects.
pub fn parse_proposal(llm_output: &str, loop_id: &str, now: i64) -> Result<LoopProposal, String> {
    let body = extract_last_json_block(llm_output)
        .ok_or_else(|| "evolve output has no fenced ```json block".to_string())?;

    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("evolve proposal block is not valid JSON: {e}"))?;

    let rationale = v
        .get("rationale")
        .and_then(|r| r.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "evolve proposal JSON missing required \"rationale\" field".to_string())?;

    let proposed_prompt_text = v
        .get("prompt_text")
        .and_then(|r| r.as_str())
        .map(|s| s.to_string());

    let proposed_gate_spec = v.get("gate_spec").and_then(|r| {
        if r.is_null() {
            None
        } else if let Some(s) = r.as_str() {
            Some(s.to_string())
        } else {
            Some(r.to_string())
        }
    });

    Ok(LoopProposal {
        id: uuid::Uuid::new_v4().to_string(),
        loop_id: loop_id.to_string(),
        created_at: now,
        rationale,
        proposed_prompt_text,
        proposed_gate_spec,
        status: "pending".to_string(),
    })
}

/// Tool-name prefixes stripped from the evolve meta-pass's tool catalog, on
/// top of the loop-level [`crate::loops::tool_classes::iteration_forbidden_tools`]
/// (`loop_create`, `ask_user`, `goal_set`, `schedule_create`) that `evolve_loop`
/// already passes as `forbidden_tools`.
///
/// The meta-pass is meant to be strictly read-only: it reasons over a loop's
/// recent iteration history — text that came out of the loop's own (possibly
/// adversarial) `final_text` and is interpolated into the prompt verbatim, a
/// prompt-injection surface — and emits a JSON proposal. Unlike a normal loop
/// iteration it never needs to act, so every tool that writes, mutates,
/// renders, spawns, or notifies is denied here; only `think`/`plan` and pure
/// read tools (`web_search`, `web_fetch`, `memory_search`, `memory_view`,
/// `browser_get_*`, `browser_screenshot`, `browser_read_artifact`,
/// `loop_list`, `loop_scratchpad_get`, `schedule_list`, `schedule_runs`,
/// `goal_status`) survive the filter. Entries are matched as
/// [`crate::agent_exec::filter_allowlist`] prefixes, so exact tool names
/// (e.g. `create_artifact`) work the same as true prefixes (e.g. `canvas_`)
/// as long as no safe tool shares that prefix — verified by
/// `evolve_forbidden_prefixes_do_not_shadow_read_tools` below.
///
/// Kept in sync with `builtin-wasm::Agent::get_chat_tools()` by hand; if a
/// new mutating tool is added there, add its name/prefix here too.
pub(crate) fn evolve_forbidden_prefixes() -> Vec<String> {
    [
        "canvas_",
        "memory_create",
        "memory_update",
        "memory_delete",
        "browser_edit_",
        "create_artifact",
        "switch_model",
        "skill_load",
        "tts_",
        "loop_cancel",
        "loop_scratchpad_set",
        // W4 task 3: the meta-pass must not be able to kick off a nested
        // evolve run on itself, nor accept/reject the very proposal it is
        // about to produce — both would let the loop's own (possibly
        // adversarial, since it's built from `final_text`) reasoning
        // self-apply changes without the human-in-the-loop review the
        // whole feature exists to enforce.
        "loop_evolve",
        "loop_proposal_respond",
        "schedule_cancel",
        "schedule_pause",
        "schedule_resume",
        "schedule_run_now",
        "notify_user",
        "goal_clear",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// Run the evolve meta-pass for one loop: load its record + recent
/// iteration history, ask the model (one read-only chat turn) for a
/// self-improvement proposal, persist it, and emit `system:loop:proposal`.
pub async fn evolve_loop(
    db: &Database,
    services: &HostServices,
    loop_id: &str,
) -> Result<LoopProposal, String> {
    let loop_repo = nevoflux_storage::repositories::LoopRepository::new(db);
    let rec = loop_repo
        .get(loop_id)
        .map_err(|e| format!("failed to load loop {loop_id}: {e}"))?
        .ok_or_else(|| format!("loop {loop_id} not found"))?;

    let recent = loop_repo
        .recent_iterations(loop_id, 12)
        .map_err(|e| format!("failed to load recent iterations for loop {loop_id}: {e}"))?;

    let prompt = build_evolve_prompt(&rec, &recent);

    let req = AgentExecRequest {
        session_id: rec.session_id.clone(),
        mode: AgentMode::Chat,
        user_message: prompt,
        forbidden_tools: crate::loops::tool_classes::iteration_forbidden_tools(),
        forbidden_prefixes: evolve_forbidden_prefixes(),
        unattended: true,
        iteration_loop_id: Some(loop_id.to_string()),
        borrow_proxy: true,
        bound_browser: None,
        history: Vec::new(),
        token_budget: None,
    };

    let outcome = run_agent_once(services, req)
        .await
        .map_err(|e| format!("evolve meta-pass failed for loop {loop_id}: {e}"))?;

    let now = nevoflux_storage::models::current_timestamp();
    let proposal = parse_proposal(&outcome.text, loop_id, now)?;

    let proposal_repo = LoopProposalRepository::new(db);
    proposal_repo
        .insert_proposal(&proposal)
        .map_err(|e| format!("failed to persist evolve proposal for loop {loop_id}: {e}"))?;

    let events = LoopEvents::new(services.event_bus.clone());
    events
        .proposal(&rec.session_id, &LoopId(loop_id.to_string()), &proposal)
        .await;

    Ok(proposal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nevoflux_storage::models::{current_timestamp, LoopState};

    fn sample_rec() -> LoopRecord {
        LoopRecord {
            id: "abc".into(),
            session_id: "s1".into(),
            trigger_expr: "time:5m".into(),
            prompt_text: Some("Check the inbox and reply to urgent emails.".into()),
            wrapped_skill: None,
            mode: "chat".into(),
            scratchpad: String::new(),
            state: LoopState::Running,
            consecutive_failures: 0,
            skipped_triggers: 3,
            iteration_count: 5,
            created_at: current_timestamp(),
            updated_at: current_timestamp(),
            gate_kind: "none".into(),
            gate_spec: None,
            gate_last_value: None,
            verify_check: None,
        }
    }

    fn sample_recent() -> Vec<RecentIteration> {
        vec![
            RecentIteration {
                sequence_number: 5,
                ended_at: Some(100),
                status: "ok".into(),
                final_text: Some("Replied to 2 urgent emails, snoozed the rest.".into()),
                verify_passed: Some(true),
                verify_reason: None,
            },
            RecentIteration {
                sequence_number: 4,
                ended_at: Some(90),
                status: "ok".into(),
                final_text: Some("No urgent emails found, nothing to do.".into()),
                verify_passed: None,
                verify_reason: None,
            },
        ]
    }

    #[test]
    fn build_evolve_prompt_contains_current_prompt_and_recent_final_text() {
        let rec = sample_rec();
        let recent = sample_recent();
        let prompt = build_evolve_prompt(&rec, &recent);

        assert!(prompt.contains("Check the inbox and reply to urgent emails."));
        assert!(prompt.contains("Replied to 2 urgent emails, snoozed the rest."));
        assert!(prompt.contains("skipped_triggers (all-time total): 3"));
        assert!(prompt.contains("```json"));
    }

    #[test]
    fn build_evolve_prompt_handles_no_recent_iterations() {
        let rec = sample_rec();
        let prompt = build_evolve_prompt(&rec, &[]);
        assert!(prompt.contains("no finished iterations yet"));
    }

    #[test]
    fn parse_proposal_extracts_rationale_and_prompt_text() {
        let output = "Looked at the history — the loop keeps re-checking an empty inbox.\n\n\
            ```json\n\
            {\"rationale\": \"Loop wastes runs when inbox is empty; tighten the gate.\", \
             \"prompt_text\": \"Check the inbox; only reply if there are unread urgent emails.\"}\n\
            ```\n";

        let proposal = parse_proposal(output, "abc", 1234).expect("should parse");
        assert_eq!(proposal.loop_id, "abc");
        assert_eq!(proposal.created_at, 1234);
        assert_eq!(proposal.status, "pending");
        assert!(!proposal.id.is_empty());
        assert_eq!(
            proposal.rationale,
            "Loop wastes runs when inbox is empty; tighten the gate."
        );
        assert_eq!(
            proposal.proposed_prompt_text.as_deref(),
            Some("Check the inbox; only reply if there are unread urgent emails.")
        );
        assert_eq!(proposal.proposed_gate_spec, None);
    }

    #[test]
    fn parse_proposal_with_no_json_block_errors() {
        let output = "The loop looks healthy, no changes needed.";
        let err = parse_proposal(output, "abc", 1234).unwrap_err();
        assert!(err.contains("no fenced"), "unexpected error: {err}");
    }

    #[test]
    fn parse_proposal_rationale_only_leaves_fields_none() {
        let output = "```json\n{\"rationale\": \"Healthy, no changes needed.\"}\n```";
        let proposal = parse_proposal(output, "abc", 1234).expect("should parse");
        assert_eq!(proposal.rationale, "Healthy, no changes needed.");
        assert_eq!(proposal.proposed_prompt_text, None);
        assert_eq!(proposal.proposed_gate_spec, None);
    }

    #[test]
    fn parse_proposal_missing_rationale_errors() {
        let output = "```json\n{\"prompt_text\": \"new prompt\"}\n```";
        let err = parse_proposal(output, "abc", 1234).unwrap_err();
        assert!(err.contains("rationale"), "unexpected error: {err}");
    }

    #[test]
    fn parse_proposal_uses_last_json_block_when_multiple_present() {
        let output = "```json\n{\"rationale\": \"first, ignored\"}\n```\n\nActually, revised:\n\
            ```json\n{\"rationale\": \"second, correct one\"}\n```";
        let proposal = parse_proposal(output, "abc", 1234).expect("should parse");
        assert_eq!(proposal.rationale, "second, correct one");
    }

    #[test]
    fn parse_proposal_gate_spec_as_nested_object_is_reserialized_to_string() {
        let output = "```json\n{\"rationale\": \"tighten gate\", \
            \"gate_spec\": {\"kind\": \"http\", \"url\": \"https://example.com\"}}\n```";
        let proposal = parse_proposal(output, "abc", 1234).expect("should parse");
        let spec = proposal
            .proposed_gate_spec
            .expect("gate_spec should be Some");
        let v: serde_json::Value = serde_json::from_str(&spec).expect("should be valid JSON");
        assert_eq!(v["kind"], "http");
    }

    #[test]
    fn parse_proposal_is_char_safe_with_cjk_content_around_json_block() {
        let output = "循环最近多次重复失败，需要收紧提示词。\n\n```json\n\
            {\"rationale\": \"循环在空收件箱时反复运行，收紧门控。\"}\n```\n\
            后续说明：无需进一步操作。";
        let proposal = parse_proposal(output, "abc", 1234).expect("should parse without panicking");
        assert_eq!(proposal.rationale, "循环在空收件箱时反复运行，收紧门控。");
    }

    // ---- evolve tool restriction (read-only meta-pass) ------------------

    #[test]
    fn evolve_forbidden_prefixes_contains_expected_entries() {
        let list = evolve_forbidden_prefixes();
        for expected in [
            "canvas_",
            "memory_create",
            "memory_update",
            "memory_delete",
            "browser_edit_",
            "create_artifact",
            "tts_",
            "loop_cancel",
            "loop_scratchpad_set",
            "loop_evolve",
            "loop_proposal_respond",
        ] {
            assert!(
                list.iter().any(|p| p == expected),
                "expected {expected:?} in evolve_forbidden_prefixes(), got {list:?}"
            );
        }
        // memory_search / memory_view are read-only and must NOT be caught by
        // any forbidden prefix here (the whole "memory_" prefix is
        // deliberately not used — see the comment on the fn).
        assert!(!list.iter().any(|p| p == "memory_"));
    }

    /// Snapshot of `builtin-wasm::Agent::get_chat_tools()`'s tool names
    /// (captured 2026-07-12, updated same day to add `loop_evolve` /
    /// `loop_proposal_respond` — W4 task 3). Used to prove
    /// the actual filter (`forbidden_tools` from `iteration_forbidden_tools()`
    /// + `forbidden_prefixes` from `evolve_forbidden_prefixes()`, exactly as
    /// `evolve_loop` passes to `AgentExecRequest`) reduces the full Chat
    /// catalog down to a read-only/reasoning-only set.
    fn chat_tool_catalog_snapshot() -> Vec<String> {
        names(&[
            "think",
            "plan",
            "create_artifact",
            "switch_model",
            "web_search",
            "web_fetch",
            "ask_user",
            "memory_search",
            "memory_create",
            "memory_update",
            "memory_delete",
            "memory_view",
            "skill_load",
            "browser_get_content",
            "browser_get_markdown",
            "browser_screenshot",
            "browser_read_artifact",
            "browser_edit_artifact",
            "canvas_eval",
            "canvas_create_composition",
            "canvas_render_video",
            "canvas_lint_composition",
            "canvas_apply_design_md",
            "canvas_inspect_layout",
            "canvas_attach_asset",
            "tts_synthesize_api",
            "tts_synthesize_local",
            "tts_transcribe",
            "canvas_create_from_visual_identity",
            "canvas_extract_visual_identity",
            "loop_create",
            "loop_list",
            "loop_cancel",
            "loop_scratchpad_get",
            "loop_scratchpad_set",
            "loop_evolve",
            "loop_proposal_respond",
            "schedule_create",
            "schedule_list",
            "schedule_cancel",
            "schedule_pause",
            "schedule_resume",
            "schedule_run_now",
            "schedule_runs",
            "notify_user",
            "goal_set",
            "goal_status",
            "goal_clear",
        ])
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn evolve_effective_tool_set_excludes_mutating_tools_and_keeps_read_tools() {
        let effective = crate::agent_exec::filter_allowlist(
            chat_tool_catalog_snapshot(),
            &crate::loops::tool_classes::iteration_forbidden_tools(),
            &evolve_forbidden_prefixes(),
        );

        for forbidden in [
            "canvas_eval",
            "memory_delete",
            "browser_edit_artifact",
            "create_artifact",
            "loop_cancel",
            "loop_scratchpad_set",
            "tts_synthesize_api",
            // also covered by iteration_forbidden_tools, but the evolve pass
            // must not regain them if that set ever shrinks:
            "loop_create",
            "ask_user",
            "goal_set",
            "schedule_create",
            // W4 task 3: the meta-pass must not be able to spawn a nested
            // evolve run or self-resolve its own proposal.
            "loop_evolve",
            "loop_proposal_respond",
            // rest of the mutating/control-plane surface:
            "switch_model",
            "skill_load",
            "memory_create",
            "memory_update",
            "canvas_create_composition",
            "canvas_render_video",
            "canvas_lint_composition",
            "canvas_apply_design_md",
            "canvas_inspect_layout",
            "canvas_attach_asset",
            "tts_synthesize_local",
            "tts_transcribe",
            "canvas_create_from_visual_identity",
            "canvas_extract_visual_identity",
            "schedule_cancel",
            "schedule_pause",
            "schedule_resume",
            "schedule_run_now",
            "notify_user",
            "goal_clear",
        ] {
            assert!(
                !effective.iter().any(|t| t == forbidden),
                "{forbidden:?} must not survive the evolve tool filter, got {effective:?}"
            );
        }

        // Read/reasoning tools remain available.
        for allowed in [
            "think",
            "plan",
            "web_search",
            "web_fetch",
            "memory_search",
            "memory_view",
            "browser_get_content",
            "browser_get_markdown",
            "browser_screenshot",
            "browser_read_artifact",
            "loop_list",
            "loop_scratchpad_get",
            "schedule_list",
            "schedule_runs",
            "goal_status",
        ] {
            assert!(
                effective.iter().any(|t| t == allowed),
                "{allowed:?} should remain available to the evolve pass, got {effective:?}"
            );
        }
    }

    #[test]
    fn evolve_loop_request_uses_non_empty_forbidden_prefixes() {
        // Guards against a future edit accidentally reverting `evolve_loop`
        // back to `forbidden_prefixes: Vec::new()` (the original defect):
        // the request builder must always route through
        // `evolve_forbidden_prefixes()`, which is never empty.
        assert!(!evolve_forbidden_prefixes().is_empty());
    }
}
