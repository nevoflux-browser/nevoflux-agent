//! Tool-class vocabulary for /loop iterations (spec §6.2).
//!
//! Tools called from inside a loop iteration are filtered against the
//! loop's `allowed_tool_classes`. Default classes are read-only;
//! anything destructive (`dom-click`, `nav`, `write`, `net-post`)
//! requires explicit opt-in at loop creation time.
//!
//! Tools NOT in the static map are treated as `Write` — fail-closed
//! default for safety when new tools land before this table is updated.

use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolClass {
    Read,
    ScratchpadWrite,
    EventSubscribe,
    DomClick,
    Nav,
    Write,
    NetPost,
}

impl ToolClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::ScratchpadWrite => "scratchpad-write",
            Self::EventSubscribe => "event-subscribe",
            Self::DomClick => "dom-click",
            Self::Nav => "nav",
            Self::Write => "write",
            Self::NetPost => "net-post",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "read" => Self::Read,
            "scratchpad-write" => Self::ScratchpadWrite,
            "event-subscribe" => Self::EventSubscribe,
            "dom-click" => Self::DomClick,
            "nav" => Self::Nav,
            "write" => Self::Write,
            "net-post" => Self::NetPost,
            _ => return None,
        })
    }
}

/// Map of tool name → class. Tools NOT in the map fall through to
/// [`ToolClass::Write`] (fail-closed default).
pub fn class_for(tool_name: &str) -> ToolClass {
    match tool_name {
        // read class
        "read" | "list_files" | "fetch_page" | "dom_query" | "screenshot"
        | "loop.scratchpad.get" | "memory_search" | "web_fetch" | "web_search"
        | "browser_query" | "browser_inspect" => ToolClass::Read,

        // scratchpad-write
        "loop.scratchpad.set" => ToolClass::ScratchpadWrite,

        // event-subscribe
        "events.subscribe" => ToolClass::EventSubscribe,

        // dom-click
        "browser_click" | "browser_click_by_id" | "browser_type" | "browser_type_by_id"
        | "browser_fill" | "browser_fill_by_id" | "browser_key_press" => ToolClass::DomClick,

        // nav
        "browser_navigate" | "browser_go_back" | "browser_go_forward"
        | "browser_open_tab" | "browser_close_tab" => ToolClass::Nav,

        // write
        "write" | "edit" | "bash" | "create_artifact" | "browser_edit_artifact"
        | "memory_create" | "canvas_create_composition" | "canvas_apply_design_md"
        | "canvas_create_from_visual_identity" | "canvas_attach_asset"
        | "canvas_render_video" => ToolClass::Write,

        // net-post — left empty in MVP (no current tools fit); fall through.
        _ => ToolClass::Write,
    }
}

/// Default classes when `allowed_tool_classes` is omitted at loop creation.
pub fn default_classes() -> Vec<ToolClass> {
    vec![ToolClass::Read, ToolClass::ScratchpadWrite, ToolClass::EventSubscribe]
}

/// Tools that are forbidden inside loop iterations regardless of class.
/// `loop.create` would let an iteration spawn nested loops; `ask_user` blocks
/// on a sidebar that may be closed.
pub fn is_forbidden_in_iteration(tool_name: &str) -> bool {
    matches!(tool_name, "loop.create" | "ask_user")
}

pub fn parse_class_list(input: &[String]) -> Result<HashSet<ToolClass>, String> {
    input
        .iter()
        .map(|s| ToolClass::from_str(s).ok_or_else(|| format!("unknown class: {s}")))
        .collect()
}
