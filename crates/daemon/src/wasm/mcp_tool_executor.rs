//! MCP tool executor for ACP bridge mode.
//!
//! Routes tool calls to the appropriate NevoFlux subsystem: browser tools,
//! computer tools, memory tools, knowledge tools, skill/tool search,
//! artifact creation, and external MCP servers.

use nevoflux_computer::{
    ClickType, ComputerController, Key, KeyCombination, KeyOrChar, KeyboardController,
    MouseButton, MouseController, Point, Region, ScreenshotProvider, ScrollDirection,
};
use nevoflux_llm::providers::acp::mcp_bridge::{McpToolBridge, PendingArtifact, ToolCallRequest};
use std::sync::Arc;
use nevoflux_protocol::BrowserToolAction;
use nevoflux_storage::{CreateKnowledgeParams, KnowledgeRepository, MemoryChunk};
use tokio::sync::mpsc;

use super::services::{BrowserContext, BrowserRequest, HostServices};

/// Run the tool executor loop, dispatching each incoming request to the
/// appropriate tool category.
pub async fn run_tool_executor(
    mut rx: mpsc::Receiver<ToolCallRequest>,
    services: HostServices,
    tool_bridge: Arc<McpToolBridge>,
) {
    while let Some(req) = rx.recv().await {
        let result =
            execute_mcp_tool(&req.name, &req.arguments, &services, &tool_bridge).await;
        let _ = req.result_tx.send(result);
    }
}

// ============================================================================
// Top-level dispatcher
// ============================================================================

/// Execute a single MCP tool call, routing to the correct category.
pub async fn execute_mcp_tool(
    name: &str,
    arguments: &serde_json::Value,
    services: &HostServices,
    tool_bridge: &Arc<McpToolBridge>,
) -> Result<String, String> {
    // 1. Browser tools
    if let Some(action) = tool_name_to_browser_action(name) {
        let browser_ctx = services
            .browser_context()
            .ok_or_else(|| "browser not available".to_string())?;
        return execute_browser_tool(action, arguments, &browser_ctx).await;
    }

    // 2. Computer Use tools
    if name.starts_with("computer_") {
        return execute_computer_tool(name, arguments, services).await;
    }

    // 3. Special tools (no external deps)
    match name {
        "think" => return Ok("Thought recorded.".to_string()),
        "create_plan" => {
            return Ok("Plan submitted for review.".to_string());
        }
        "create_artifact" => {
            return execute_create_artifact(arguments, tool_bridge).await;
        }
        _ => {}
    }

    // 4. Memory tools
    match name {
        "memory_search" => return execute_memory_search(arguments, services),
        "memory_create" => return execute_memory_create(arguments, services),
        "memory_update" => return execute_memory_update(arguments, services),
        "memory_delete" => return execute_memory_delete(arguments, services),
        "memory_view" => return execute_memory_view(arguments, services),
        "knowledge_teach" => return execute_knowledge_teach(arguments, services),
        _ => {}
    }

    // 5. Skill/tool search
    match name {
        "skill_load" => return execute_skill_load(arguments, services).await,
        "tool_search" => return execute_tool_search(arguments, services).await,
        _ => {}
    }

    // 6. External MCP tools (via McpManager)
    if let Some(ref mcp_manager) = services.mcp_manager {
        if let Ok(result) = execute_mcp_manager_tool(name, arguments, mcp_manager).await {
            return Ok(result);
        }
    }

    Err(format!("unknown tool: {name}"))
}

// ============================================================================
// 1. Browser tools
// ============================================================================

/// Map tool name to BrowserToolAction.
fn tool_name_to_browser_action(name: &str) -> Option<BrowserToolAction> {
    let key = name.strip_prefix("browser_").unwrap_or(name);
    match key {
        "navigate" => Some(BrowserToolAction::Navigate),
        "go_back" => Some(BrowserToolAction::GoBack),
        "go_forward" => Some(BrowserToolAction::GoForward),
        "click" => Some(BrowserToolAction::Click),
        "click_by_id" => Some(BrowserToolAction::ClickById),
        "type" => Some(BrowserToolAction::Type),
        "type_by_id" => Some(BrowserToolAction::TypeById),
        "fill" => Some(BrowserToolAction::Fill),
        "fill_by_id" => Some(BrowserToolAction::FillById),
        "get_content" => Some(BrowserToolAction::GetContent),
        "get_markdown" => Some(BrowserToolAction::GetMarkdown),
        "screenshot" => Some(BrowserToolAction::Screenshot),
        "snapshot" => Some(BrowserToolAction::Snapshot),
        "eval_js" => Some(BrowserToolAction::EvalJs),
        "wait_for" => Some(BrowserToolAction::WaitFor),
        "wait_for_stable" => Some(BrowserToolAction::WaitForStable),
        "scroll" => Some(BrowserToolAction::Scroll),
        "get_element" => Some(BrowserToolAction::GetElement),
        "get_elements" => Some(BrowserToolAction::GetElements),
        "query_all" => Some(BrowserToolAction::QueryAll),
        "get_tabs" | "list_tabs" => Some(BrowserToolAction::ListTabs),
        "query_tabs" => Some(BrowserToolAction::QueryTabs),
        "key_press" => Some(BrowserToolAction::KeyPress),
        "read_artifact" => Some(BrowserToolAction::ReadArtifact),
        "edit_artifact" => Some(BrowserToolAction::EditArtifact),
        "ask_user" => Some(BrowserToolAction::AskUser),
        "web_search" => Some(BrowserToolAction::WebSearch),
        "fetch_page" => Some(BrowserToolAction::WebFetch),
        _ => None,
    }
}

/// Execute a browser tool via BrowserContext channel.
async fn execute_browser_tool(
    action: BrowserToolAction,
    arguments: &serde_json::Value,
    browser_ctx: &BrowserContext,
) -> Result<String, String> {
    use tokio::sync::oneshot;

    let tab_id = arguments.get("tab_id").and_then(|v| v.as_i64());

    let (response_tx, response_rx) = oneshot::channel();

    let request = BrowserRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        session_id: String::new(),
        tab_id,
        action,
        params: arguments.clone(),
        timeout_ms: 30_000,
        client_identity: browser_ctx.client_identity.clone(),
        proxy_id: browser_ctx.proxy_id.clone(),
    };

    browser_ctx
        .sender
        .send((request, response_tx))
        .await
        .map_err(|e| format!("browser_sender failed: {e}"))?;

    let response = response_rx
        .await
        .map_err(|_| "browser response channel closed".to_string())?;

    if response.success {
        Ok(response
            .result
            .map(|v| serde_json::to_string(&v).unwrap_or_default())
            .unwrap_or_default())
    } else {
        Err(response
            .error
            .map(|e| format!("{}: {}", e.code, e.message))
            .unwrap_or_else(|| "unknown tool error".to_string()))
    }
}

// ============================================================================
// 2. Computer Use tools
// ============================================================================

/// Execute a computer control tool via ComputerController.
async fn execute_computer_tool(
    name: &str,
    arguments: &serde_json::Value,
    services: &HostServices,
) -> Result<String, String> {
    let controller = services
        .computer_controller
        .as_ref()
        .ok_or_else(|| "computer controller not available".to_string())?;

    match name {
        "computer_screenshot" => {
            let screenshot = if let Some(region) = arguments.get("region") {
                let x = region.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                let y = region.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                let width = region.get("width").and_then(|v| v.as_u64()).unwrap_or(100) as u32;
                let height = region.get("height").and_then(|v| v.as_u64()).unwrap_or(100) as u32;
                controller
                    .capture_region(Region::new(x, y, width, height))
                    .await
                    .map_err(|e| format!("screenshot failed: {e}"))?
            } else if let Some(display_id) = arguments.get("display_id").and_then(|v| v.as_u64()) {
                controller
                    .capture_display(display_id as u32)
                    .await
                    .map_err(|e| format!("screenshot failed: {e}"))?
            } else {
                controller
                    .capture_screen()
                    .await
                    .map_err(|e| format!("screenshot failed: {e}"))?
            };
            // Return the serialized screenshot (contains base64 data)
            serde_json::to_string(&screenshot).map_err(|e| format!("serialize failed: {e}"))
        }
        "computer_mouse_move" => {
            let x = arguments
                .get("x")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| "missing 'x' argument".to_string())?
                as i32;
            let y = arguments
                .get("y")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| "missing 'y' argument".to_string())?
                as i32;
            controller
                .move_to(Point::new(x, y))
                .await
                .map_err(|e| format!("mouse move failed: {e}"))?;
            Ok(format!("Moved mouse to ({x}, {y})"))
        }
        "computer_mouse_click" => {
            let button = parse_mouse_button(arguments);
            let click_type = parse_click_type(arguments);

            if let (Some(x), Some(y)) = (
                arguments.get("x").and_then(|v| v.as_i64()),
                arguments.get("y").and_then(|v| v.as_i64()),
            ) {
                controller
                    .click_at(Point::new(x as i32, y as i32), button, click_type)
                    .await
                    .map_err(|e| format!("mouse click failed: {e}"))?;
                Ok(format!("Clicked {click_type:?} {button:?} at ({x}, {y})"))
            } else {
                controller
                    .click(button, click_type)
                    .await
                    .map_err(|e| format!("mouse click failed: {e}"))?;
                Ok(format!("Clicked {click_type:?} {button:?}"))
            }
        }
        "computer_mouse_scroll" => {
            let direction = arguments
                .get("direction")
                .and_then(|v| v.as_str())
                .map(|s| match s {
                    "down" => ScrollDirection::Down,
                    "left" => ScrollDirection::Left,
                    "right" => ScrollDirection::Right,
                    _ => ScrollDirection::Up,
                })
                .unwrap_or(ScrollDirection::Down);
            let amount = arguments
                .get("amount")
                .and_then(|v| v.as_u64())
                .unwrap_or(3) as u32;
            controller
                .scroll(direction, amount)
                .await
                .map_err(|e| format!("mouse scroll failed: {e}"))?;
            Ok(format!("Scrolled {direction:?} by {amount}"))
        }
        "computer_mouse_drag" => {
            let from_x = arguments
                .get("from_x")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| "missing 'from_x'".to_string())?
                as i32;
            let from_y = arguments
                .get("from_y")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| "missing 'from_y'".to_string())?
                as i32;
            let to_x = arguments
                .get("to_x")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| "missing 'to_x'".to_string())?
                as i32;
            let to_y = arguments
                .get("to_y")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| "missing 'to_y'".to_string())?
                as i32;
            let button = parse_mouse_button(arguments);
            controller
                .drag(Point::new(from_x, from_y), Point::new(to_x, to_y), button)
                .await
                .map_err(|e| format!("mouse drag failed: {e}"))?;
            Ok(format!(
                "Dragged from ({from_x}, {from_y}) to ({to_x}, {to_y})"
            ))
        }
        "computer_mouse_position" => {
            let pos = controller
                .get_position()
                .await
                .map_err(|e| format!("get position failed: {e}"))?;
            serde_json::to_string(&pos).map_err(|e| format!("serialize failed: {e}"))
        }
        "computer_type_text" => {
            let text = arguments
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "missing 'text' argument".to_string())?;
            controller
                .type_text(text)
                .await
                .map_err(|e| format!("type text failed: {e}"))?;
            Ok(format!("Typed {} characters", text.len()))
        }
        "computer_press_key" => {
            let key_or_char =
                if let Some(key_str) = arguments.get("key").and_then(|v| v.as_str()) {
                    parse_key_string(key_str)?
                } else if let Some(char_str) = arguments.get("char").and_then(|v| v.as_str()) {
                    if char_str.len() != 1 {
                        return Err("char must be a single character".to_string());
                    }
                    KeyOrChar::Char(char_str.chars().next().unwrap())
                } else {
                    return Err("missing 'key' or 'char' argument".to_string());
                };

            let mut combination = KeyCombination {
                key: key_or_char,
                modifiers: Vec::new(),
            };

            if let Some(modifiers) = arguments.get("modifiers").and_then(|v| v.as_array()) {
                for modifier in modifiers {
                    if let Some(mod_str) = modifier.as_str() {
                        match mod_str.to_lowercase().as_str() {
                            "shift" => combination = combination.with_shift(),
                            "ctrl" | "control" => combination = combination.with_ctrl(),
                            "alt" => combination = combination.with_alt(),
                            "meta" | "cmd" | "command" | "win" | "windows" => {
                                combination = combination.with_meta()
                            }
                            _ => {}
                        }
                    }
                }
            }

            controller
                .press_key(combination.clone())
                .await
                .map_err(|e| format!("press key failed: {e}"))?;
            Ok(format!("Pressed key combination: {combination:?}"))
        }
        "computer_get_displays" => {
            let displays = controller
                .get_displays()
                .await
                .map_err(|e| format!("get displays failed: {e}"))?;
            serde_json::to_string(&displays).map_err(|e| format!("serialize failed: {e}"))
        }
        _ => Err(format!("unknown computer tool: {name}")),
    }
}

/// Parse mouse button from arguments (default: left).
fn parse_mouse_button(arguments: &serde_json::Value) -> MouseButton {
    arguments
        .get("button")
        .and_then(|v| v.as_str())
        .map(|s| match s {
            "right" => MouseButton::Right,
            "middle" => MouseButton::Middle,
            _ => MouseButton::Left,
        })
        .unwrap_or(MouseButton::Left)
}

/// Parse click type from arguments (default: single).
fn parse_click_type(arguments: &serde_json::Value) -> ClickType {
    arguments
        .get("click_type")
        .and_then(|v| v.as_str())
        .map(|s| match s {
            "double" => ClickType::Double,
            "triple" => ClickType::Triple,
            _ => ClickType::Single,
        })
        .unwrap_or(ClickType::Single)
}

/// Parse a key string into KeyOrChar.
fn parse_key_string(key_str: &str) -> Result<KeyOrChar, String> {
    let key = match key_str.to_lowercase().as_str() {
        "shift" => Key::Shift,
        "ctrl" | "control" => Key::Control,
        "alt" => Key::Alt,
        "meta" | "cmd" | "command" | "win" | "windows" => Key::Meta,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        "escape" | "esc" => Key::Escape,
        "tab" => Key::Tab,
        "capslock" | "caps_lock" => Key::CapsLock,
        "space" => Key::Space,
        "enter" | "return" => Key::Enter,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "insert" | "ins" => Key::Insert,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "page_up" => Key::PageUp,
        "pagedown" | "page_down" => Key::PageDown,
        "up" | "arrowup" | "arrow_up" => Key::ArrowUp,
        "down" | "arrowdown" | "arrow_down" => Key::ArrowDown,
        "left" | "arrowleft" | "arrow_left" => Key::ArrowLeft,
        "right" | "arrowright" | "arrow_right" => Key::ArrowRight,
        "printscreen" | "print_screen" => Key::PrintScreen,
        "scrolllock" | "scroll_lock" => Key::ScrollLock,
        "pause" => Key::Pause,
        "numlock" | "num_lock" => Key::NumLock,
        s if s.len() == 1 => {
            return Ok(KeyOrChar::Char(s.chars().next().unwrap()));
        }
        _ => {
            return Err(format!("unknown key: {key_str}"));
        }
    };
    Ok(KeyOrChar::Key(key))
}

// ============================================================================
// 3. Special tools: create_artifact
// ============================================================================

/// Execute `create_artifact` by persisting to the database.
///
/// In the normal WASM agent flow, artifacts are rendered by the sidebar.
/// In ACP bridge mode we persist the artifact and return its metadata.
/// The sidebar picks up artifacts via the existing session artifact API.
async fn execute_create_artifact(
    arguments: &serde_json::Value,
    tool_bridge: &Arc<McpToolBridge>,
) -> Result<String, String> {
    let title = arguments
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("Untitled")
        .to_string();
    let content_type = arguments
        .get("content_type")
        .and_then(|v| v.as_str())
        .unwrap_or("text/html")
        .to_string();
    let content = arguments
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = arguments
        .get("description")
        .and_then(|v| v.as_str())
        .map(String::from);
    let entry = arguments
        .get("entry")
        .and_then(|v| v.as_str())
        .map(String::from);
    let files: Option<std::collections::HashMap<String, String>> =
        arguments.get("files").and_then(|f| {
            if let Some(obj) = f.as_object() {
                Some(
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect(),
                )
            } else if let Some(s) = f.as_str() {
                serde_json::from_str(s).ok()
            } else {
                None
            }
        });

    // Generate unique artifact ID
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let id = format!("art-{}-1", ts);

    // Store as pending artifact — server will pick it up after prompt completes
    // and call send_artifact_stream to push to sidebar.
    tool_bridge.push_artifact(PendingArtifact {
        id: id.clone(),
        title: title.clone(),
        content_type,
        description,
        content,
        files,
        entry,
    });

    Ok(serde_json::json!({
        "id": id,
        "title": title,
        "status": "created"
    })
    .to_string())
}

// ============================================================================
// 4. Memory tools
// ============================================================================

fn execute_memory_search(
    args: &serde_json::Value,
    services: &HostServices,
) -> Result<String, String> {
    let query = args["query"].as_str().unwrap_or("");
    let limit = args["limit"].as_u64().unwrap_or(10) as usize;
    let results = services
        .database
        .memory()
        .search_fts(query, limit)
        .map_err(|e| format!("memory search failed: {e}"))?;
    serde_json::to_string_pretty(&results).map_err(|e| format!("serialize failed: {e}"))
}

fn execute_memory_create(
    args: &serde_json::Value,
    services: &HostServices,
) -> Result<String, String> {
    let content = args["content"].as_str().unwrap_or("");
    let metadata = args
        .get("metadata")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let chunk = MemoryChunk::new(content).with_metadata(metadata);
    let id = chunk.id.clone();
    services
        .database
        .memory()
        .create(&chunk)
        .map_err(|e| format!("memory create failed: {e}"))?;
    Ok(serde_json::json!({"id": id, "status": "created"}).to_string())
}

fn execute_memory_update(
    args: &serde_json::Value,
    services: &HostServices,
) -> Result<String, String> {
    let id = args["id"]
        .as_str()
        .ok_or_else(|| "missing 'id' argument".to_string())?;
    let content = args["content"]
        .as_str()
        .ok_or_else(|| "missing 'content' argument".to_string())?;
    services
        .database
        .memory()
        .update(id, content)
        .map_err(|e| format!("memory update failed: {e}"))?;
    Ok(serde_json::json!({"id": id, "status": "updated"}).to_string())
}

fn execute_memory_delete(
    args: &serde_json::Value,
    services: &HostServices,
) -> Result<String, String> {
    let id = args["id"]
        .as_str()
        .ok_or_else(|| "missing 'id' argument".to_string())?;
    services
        .database
        .memory()
        .delete(id)
        .map_err(|e| format!("memory delete failed: {e}"))?;
    Ok(serde_json::json!({"id": id, "status": "deleted"}).to_string())
}

fn execute_memory_view(
    args: &serde_json::Value,
    services: &HostServices,
) -> Result<String, String> {
    let limit = args["limit"].as_u64().unwrap_or(20) as usize;
    let entries = services
        .database
        .memory()
        .list(Some(limit))
        .map_err(|e| format!("memory view failed: {e}"))?;
    serde_json::to_string_pretty(&entries).map_err(|e| format!("serialize failed: {e}"))
}

// ============================================================================
// Knowledge teach
// ============================================================================

fn execute_knowledge_teach(
    args: &serde_json::Value,
    services: &HostServices,
) -> Result<String, String> {
    let category = args["category"]
        .as_str()
        .unwrap_or("user_preference")
        .to_string();
    let summary = args["summary"].as_str().unwrap_or("").to_string();
    let details = args["details"].as_str().unwrap_or("").to_string();
    let domain = args.get("domain").and_then(|v| v.as_str()).map(String::from);

    let params = CreateKnowledgeParams {
        category,
        summary,
        details,
        domain,
        ..Default::default()
    };

    let repo = KnowledgeRepository::new(&services.database);
    let entry = repo
        .create(params)
        .map_err(|e| format!("knowledge teach failed: {e}"))?;

    Ok(serde_json::json!({"id": entry.id, "status": "taught"}).to_string())
}

// ============================================================================
// 5. Skill load & tool search
// ============================================================================

async fn execute_skill_load(
    args: &serde_json::Value,
    services: &HostServices,
) -> Result<String, String> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| "missing 'name' argument".to_string())?;
    let skills = services.skills.read().await;
    match skills.get(name) {
        Some(skill) => {
            let mut result = String::new();

            // Inject base_path instruction so Claude Code can read auxiliary files
            if let Some(ref file_path) = skill.file_path {
                if let Some(base_dir) = file_path.parent() {
                    let base_path = base_dir.display();
                    // List available files in skill directory
                    let available_files: Vec<String> = std::fs::read_dir(base_dir)
                        .ok()
                        .map(|entries| {
                            entries
                                .filter_map(|e| e.ok())
                                .filter(|e| {
                                    e.path().is_file()
                                        && e.file_name() != "SKILL.md"
                                })
                                .map(|e| e.file_name().to_string_lossy().to_string())
                                .collect()
                        })
                        .unwrap_or_default();

                    result.push_str(&format!(
                        "[Skill directory: {base_path}]\n\
                         [To read any file referenced below, use the Read tool with full path: {base_path}/<filename>]\n"
                    ));
                    if !available_files.is_empty() {
                        result.push_str("[Available files: ");
                        result.push_str(&available_files.join(", "));
                        result.push_str("]\n");
                    }
                    result.push('\n');
                }
            }

            result.push_str(&skill.content);
            Ok(result)
        }
        None => Err(format!("skill not found: {name}")),
    }
}

async fn execute_tool_search(
    args: &serde_json::Value,
    services: &HostServices,
) -> Result<String, String> {
    let query = args["query"].as_str().unwrap_or("");
    let max_results = args["max_results"].as_u64().unwrap_or(5) as usize;

    let index = services
        .tool_search
        .as_ref()
        .ok_or_else(|| "tool search not available".to_string())?;
    let index = index.read().await;
    let results = index.search(query);

    // Limit to max_results and serialize tool definitions
    let tools: Vec<&nevoflux_mcp::ToolDefinition> =
        results.iter().take(max_results).map(|r| &r.tool).collect();
    serde_json::to_string_pretty(&tools).map_err(|e| format!("serialize failed: {e}"))
}

// ============================================================================
// 6. External MCP tools
// ============================================================================

async fn execute_mcp_manager_tool(
    name: &str,
    arguments: &serde_json::Value,
    mcp_manager: &nevoflux_mcp::McpManager,
) -> Result<String, String> {
    let tool_result = mcp_manager
        .call_tool_any(name, arguments.clone())
        .await
        .map_err(|e| format!("MCP tool call failed: {e}"))?;

    // Concatenate text content from the ToolResult
    let mut text_parts = Vec::new();
    for content in &tool_result.content {
        match content {
            nevoflux_mcp::ToolResultContent::Text { text } => {
                text_parts.push(text.clone());
            }
            _ => {
                // Serialize non-text content as JSON
                if let Ok(json) = serde_json::to_string(content) {
                    text_parts.push(json);
                }
            }
        }
    }

    if tool_result.is_error {
        Err(text_parts.join("\n"))
    } else {
        Ok(text_parts.join("\n"))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_name_to_browser_action() {
        assert_eq!(
            tool_name_to_browser_action("browser_navigate"),
            Some(BrowserToolAction::Navigate)
        );
        assert_eq!(
            tool_name_to_browser_action("browser_snapshot"),
            Some(BrowserToolAction::Snapshot)
        );
        assert_eq!(
            tool_name_to_browser_action("web_search"),
            Some(BrowserToolAction::WebSearch)
        );
        assert_eq!(tool_name_to_browser_action("unknown_tool"), None);
    }

    #[test]
    fn test_tool_name_to_browser_action_bare_names() {
        assert_eq!(
            tool_name_to_browser_action("navigate"),
            Some(BrowserToolAction::Navigate)
        );
        assert_eq!(
            tool_name_to_browser_action("screenshot"),
            Some(BrowserToolAction::Screenshot)
        );
        assert_eq!(
            tool_name_to_browser_action("get_tabs"),
            Some(BrowserToolAction::ListTabs)
        );
        assert_eq!(
            tool_name_to_browser_action("list_tabs"),
            Some(BrowserToolAction::ListTabs)
        );
    }

    #[test]
    fn test_tool_name_to_browser_action_all_variants() {
        let cases = [
            ("browser_go_back", BrowserToolAction::GoBack),
            ("browser_go_forward", BrowserToolAction::GoForward),
            ("browser_click", BrowserToolAction::Click),
            ("browser_click_by_id", BrowserToolAction::ClickById),
            ("browser_type", BrowserToolAction::Type),
            ("browser_type_by_id", BrowserToolAction::TypeById),
            ("browser_fill", BrowserToolAction::Fill),
            ("browser_fill_by_id", BrowserToolAction::FillById),
            ("browser_get_content", BrowserToolAction::GetContent),
            ("browser_get_markdown", BrowserToolAction::GetMarkdown),
            ("browser_eval_js", BrowserToolAction::EvalJs),
            ("browser_wait_for", BrowserToolAction::WaitFor),
            ("browser_wait_for_stable", BrowserToolAction::WaitForStable),
            ("browser_scroll", BrowserToolAction::Scroll),
            ("browser_get_element", BrowserToolAction::GetElement),
            ("browser_get_elements", BrowserToolAction::GetElements),
            ("browser_query_all", BrowserToolAction::QueryAll),
            ("browser_query_tabs", BrowserToolAction::QueryTabs),
            ("browser_key_press", BrowserToolAction::KeyPress),
            ("browser_read_artifact", BrowserToolAction::ReadArtifact),
            ("browser_edit_artifact", BrowserToolAction::EditArtifact),
            ("browser_ask_user", BrowserToolAction::AskUser),
            ("fetch_page", BrowserToolAction::WebFetch),
        ];
        for (name, expected) in cases {
            assert_eq!(
                tool_name_to_browser_action(name),
                Some(expected),
                "failed for {name}"
            );
        }
    }

    #[test]
    fn test_parse_mouse_button() {
        assert_eq!(
            parse_mouse_button(&serde_json::json!({})),
            MouseButton::Left
        );
        assert_eq!(
            parse_mouse_button(&serde_json::json!({"button": "right"})),
            MouseButton::Right
        );
        assert_eq!(
            parse_mouse_button(&serde_json::json!({"button": "middle"})),
            MouseButton::Middle
        );
    }

    #[test]
    fn test_parse_click_type() {
        assert_eq!(
            parse_click_type(&serde_json::json!({})),
            ClickType::Single
        );
        assert_eq!(
            parse_click_type(&serde_json::json!({"click_type": "double"})),
            ClickType::Double
        );
        assert_eq!(
            parse_click_type(&serde_json::json!({"click_type": "triple"})),
            ClickType::Triple
        );
    }

    #[test]
    fn test_parse_key_string() {
        assert!(matches!(
            parse_key_string("enter"),
            Ok(KeyOrChar::Key(Key::Enter))
        ));
        assert!(matches!(
            parse_key_string("escape"),
            Ok(KeyOrChar::Key(Key::Escape))
        ));
        assert!(matches!(parse_key_string("a"), Ok(KeyOrChar::Char('a'))));
        assert!(parse_key_string("unknown_key").is_err());
    }

    #[test]
    fn test_dispatch_think_tool() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = std::sync::Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let services = HostServices::new(db);
        let result = rt.block_on(execute_mcp_tool("think", &serde_json::json!({}), &services));
        assert_eq!(result, Ok("Thought recorded.".to_string()));
    }

    #[test]
    fn test_dispatch_create_plan() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = std::sync::Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let services = HostServices::new(db);
        let result =
            rt.block_on(execute_mcp_tool("create_plan", &serde_json::json!({}), &services));
        assert_eq!(result, Ok("Plan submitted for review.".to_string()));
    }

    #[test]
    fn test_dispatch_unknown_tool() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = std::sync::Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let services = HostServices::new(db);
        let result = rt.block_on(execute_mcp_tool(
            "nonexistent_tool",
            &serde_json::json!({}),
            &services,
        ));
        assert_eq!(result, Err("unknown tool: nonexistent_tool".to_string()));
    }

    #[test]
    fn test_memory_roundtrip() {
        let db = std::sync::Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let services = HostServices::new(db);

        // Create
        let result = execute_memory_create(
            &serde_json::json!({"content": "test memory content"}),
            &services,
        );
        assert!(result.is_ok(), "create failed: {:?}", result);
        let created: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        let id = created["id"].as_str().unwrap().to_string();

        // Search
        let result = execute_memory_search(
            &serde_json::json!({"query": "test memory", "limit": 5}),
            &services,
        );
        assert!(result.is_ok());
        let found: Vec<serde_json::Value> = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(!found.is_empty());

        // Update
        let result = execute_memory_update(
            &serde_json::json!({"id": id, "content": "updated content"}),
            &services,
        );
        assert!(result.is_ok());

        // View
        let result = execute_memory_view(&serde_json::json!({"limit": 10}), &services);
        assert!(result.is_ok());

        // Delete
        let result = execute_memory_delete(&serde_json::json!({"id": id}), &services);
        assert!(result.is_ok());
    }

    #[test]
    fn test_knowledge_teach() {
        let db = std::sync::Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let services = HostServices::new(db);

        let result = execute_knowledge_teach(
            &serde_json::json!({
                "category": "user_preference",
                "summary": "User prefers dark mode",
                "details": "The user always wants dark theme in all applications"
            }),
            &services,
        );
        assert!(result.is_ok(), "knowledge teach failed: {:?}", result);
        let parsed: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(parsed["status"], "taught");
        assert!(parsed["id"].as_str().is_some());
    }

    #[test]
    fn test_computer_tool_no_controller() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = std::sync::Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let services = HostServices::new(db);
        let result = rt.block_on(execute_computer_tool(
            "computer_screenshot",
            &serde_json::json!({}),
            &services,
        ));
        assert_eq!(
            result,
            Err("computer controller not available".to_string())
        );
    }

    #[test]
    fn test_browser_tool_no_context() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = std::sync::Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let services = HostServices::new(db);
        let result = rt.block_on(execute_mcp_tool(
            "browser_navigate",
            &serde_json::json!({"url": "https://example.com"}),
            &services,
        ));
        assert_eq!(result, Err("browser not available".to_string()));
    }
}
