//! MCP tool definitions for NevoFlux Agent.
//!
//! This module provides all tool definitions that the NevoFlux MCP server
//! exposes to Claude Code and other MCP clients.
//!
//! # Tool Categories
//!
//! - **Browser tools**: Interact with web browsers (navigate, click, type, screenshot)
//! - **Agent tools**: Interact with the AI agent (chat)
//! - **Computer tools**: Interact with the local computer (screenshot, mouse, keyboard)

use crate::types::ToolDefinition;

/// Create all available MCP tools.
///
/// Returns a vector of `ToolDefinition` containing all browser, agent,
/// and computer tools that the NevoFlux MCP server supports.
///
/// # Example
///
/// ```rust
/// use nevoflux_mcp::tools::create_tools;
///
/// let tools = create_tools();
/// assert!(tools.iter().any(|t| t.name == "browser_navigate"));
/// ```
pub fn create_tools() -> Vec<ToolDefinition> {
    let mut tools = Vec::new();

    // Browser tools
    tools.extend(create_browser_tools());

    // Agent tools
    tools.extend(create_agent_tools());

    // Computer tools
    tools.extend(create_computer_tools());

    tools
}

/// Create browser automation tools.
fn create_browser_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "browser_navigate".to_string(),
            description: "Navigate the browser to a specified URL".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to navigate to"
                    }
                },
                "required": ["url"]
            }),
        },
        ToolDefinition {
            name: "browser_click".to_string(),
            description: "Click an element on the page identified by a CSS selector".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "selector": {
                        "type": "string",
                        "description": "CSS selector for the element to click"
                    }
                },
                "required": ["selector"]
            }),
        },
        ToolDefinition {
            name: "browser_screenshot".to_string(),
            description: "Take a screenshot of the current page".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "full_page": {
                        "type": "boolean",
                        "description": "Whether to capture the full page or just the viewport",
                        "default": false
                    }
                }
            }),
        },
        ToolDefinition {
            name: "browser_type".to_string(),
            description: "Type text into an element on the page identified by a CSS selector"
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "selector": {
                        "type": "string",
                        "description": "CSS selector for the element to type into"
                    },
                    "text": {
                        "type": "string",
                        "description": "The text to type"
                    },
                    "clear": {
                        "type": "boolean",
                        "description": "Whether to clear the element before typing",
                        "default": false
                    }
                },
                "required": ["selector", "text"]
            }),
        },
    ]
}

/// Create agent interaction tools.
fn create_agent_tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "agent_chat".to_string(),
        description: "Send a message to the AI agent and receive a response".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The message to send to the agent"
                },
                "context": {
                    "type": "object",
                    "description": "Optional context to include with the message",
                    "properties": {
                        "page_content": {
                            "type": "string",
                            "description": "Current page content or relevant text"
                        },
                        "page_url": {
                            "type": "string",
                            "description": "Current page URL"
                        }
                    }
                }
            },
            "required": ["message"]
        }),
    }]
}

/// Create computer interaction tools.
fn create_computer_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "computer_screenshot".to_string(),
            description: "Take a screenshot of the entire screen or a specific monitor".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "monitor": {
                        "type": "integer",
                        "description": "Monitor index (0-based). If not specified, captures primary monitor.",
                        "minimum": 0
                    }
                }
            }),
        },
        ToolDefinition {
            name: "computer_mouse_move".to_string(),
            description: "Move the mouse cursor to a specified position on screen".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "x": {
                        "type": "integer",
                        "description": "X coordinate in pixels"
                    },
                    "y": {
                        "type": "integer",
                        "description": "Y coordinate in pixels"
                    },
                    "click": {
                        "type": "string",
                        "description": "Optional click action after moving",
                        "enum": ["left", "right", "middle", "double"]
                    }
                },
                "required": ["x", "y"]
            }),
        },
        ToolDefinition {
            name: "computer_type_text".to_string(),
            description: "Type text using the keyboard at the current cursor position".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The text to type"
                    },
                    "delay_ms": {
                        "type": "integer",
                        "description": "Delay between keystrokes in milliseconds",
                        "default": 0,
                        "minimum": 0
                    }
                },
                "required": ["text"]
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_tools_returns_all_tools() {
        let tools = create_tools();

        // Should have 8 tools total
        assert_eq!(tools.len(), 8);

        // Verify all expected tools are present
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        // Browser tools
        assert!(tool_names.contains(&"browser_navigate"));
        assert!(tool_names.contains(&"browser_click"));
        assert!(tool_names.contains(&"browser_screenshot"));
        assert!(tool_names.contains(&"browser_type"));

        // Agent tools
        assert!(tool_names.contains(&"agent_chat"));

        // Computer tools
        assert!(tool_names.contains(&"computer_screenshot"));
        assert!(tool_names.contains(&"computer_mouse_move"));
        assert!(tool_names.contains(&"computer_type_text"));
    }

    #[test]
    fn test_browser_navigate_tool_schema() {
        let tools = create_browser_tools();
        let navigate = tools.iter().find(|t| t.name == "browser_navigate").unwrap();

        assert_eq!(
            navigate.description,
            "Navigate the browser to a specified URL"
        );

        let schema = &navigate.input_schema;
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["url"].is_object());
        assert_eq!(schema["required"], serde_json::json!(["url"]));
    }

    #[test]
    fn test_browser_click_tool_schema() {
        let tools = create_browser_tools();
        let click = tools.iter().find(|t| t.name == "browser_click").unwrap();

        assert!(click.description.contains("Click an element on the page"));

        let schema = &click.input_schema;
        assert_eq!(schema["properties"]["selector"]["type"], "string");
        assert_eq!(schema["required"], serde_json::json!(["selector"]));
    }

    #[test]
    fn test_browser_screenshot_tool_schema() {
        let tools = create_browser_tools();
        let screenshot = tools
            .iter()
            .find(|t| t.name == "browser_screenshot")
            .unwrap();

        assert!(screenshot.description.contains("screenshot"));

        let schema = &screenshot.input_schema;
        assert_eq!(schema["properties"]["full_page"]["type"], "boolean");
        // full_page is optional, so no required field or empty array
    }

    #[test]
    fn test_browser_type_tool_schema() {
        let tools = create_browser_tools();
        let type_tool = tools.iter().find(|t| t.name == "browser_type").unwrap();

        assert!(type_tool.description.contains("Type text"));

        let schema = &type_tool.input_schema;
        assert_eq!(schema["properties"]["selector"]["type"], "string");
        assert_eq!(schema["properties"]["text"]["type"], "string");
        assert_eq!(schema["required"], serde_json::json!(["selector", "text"]));
    }

    #[test]
    fn test_agent_chat_tool_schema() {
        let tools = create_agent_tools();
        let chat = tools.iter().find(|t| t.name == "agent_chat").unwrap();

        assert!(chat.description.contains("AI agent"));

        let schema = &chat.input_schema;
        assert_eq!(schema["properties"]["message"]["type"], "string");
        assert!(schema["properties"]["context"].is_object());
        assert_eq!(schema["required"], serde_json::json!(["message"]));
    }

    #[test]
    fn test_computer_screenshot_tool_schema() {
        let tools = create_computer_tools();
        let screenshot = tools
            .iter()
            .find(|t| t.name == "computer_screenshot")
            .unwrap();

        assert!(screenshot.description.contains("screenshot"));

        let schema = &screenshot.input_schema;
        assert_eq!(schema["properties"]["monitor"]["type"], "integer");
    }

    #[test]
    fn test_computer_mouse_move_tool_schema() {
        let tools = create_computer_tools();
        let mouse_move = tools
            .iter()
            .find(|t| t.name == "computer_mouse_move")
            .unwrap();

        assert!(mouse_move.description.contains("mouse cursor"));

        let schema = &mouse_move.input_schema;
        assert_eq!(schema["properties"]["x"]["type"], "integer");
        assert_eq!(schema["properties"]["y"]["type"], "integer");
        assert_eq!(schema["required"], serde_json::json!(["x", "y"]));

        // Verify click enum options
        let click_enum = &schema["properties"]["click"]["enum"];
        assert!(click_enum
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("left")));
        assert!(click_enum
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("right")));
        assert!(click_enum
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("double")));
    }

    #[test]
    fn test_computer_type_text_tool_schema() {
        let tools = create_computer_tools();
        let type_text = tools
            .iter()
            .find(|t| t.name == "computer_type_text")
            .unwrap();

        assert!(type_text.description.contains("Type text"));

        let schema = &type_text.input_schema;
        assert_eq!(schema["properties"]["text"]["type"], "string");
        assert_eq!(schema["properties"]["delay_ms"]["type"], "integer");
        assert_eq!(schema["required"], serde_json::json!(["text"]));
    }

    #[test]
    fn test_all_tools_have_valid_schemas() {
        let tools = create_tools();

        for tool in &tools {
            // Every tool should have a non-empty name
            assert!(!tool.name.is_empty(), "Tool name should not be empty");

            // Every tool should have a non-empty description
            assert!(
                !tool.description.is_empty(),
                "Tool {} should have a description",
                tool.name
            );

            // Every tool schema should be an object type
            assert_eq!(
                tool.input_schema["type"], "object",
                "Tool {} schema should be object type",
                tool.name
            );

            // Every tool should have a properties field
            assert!(
                tool.input_schema["properties"].is_object(),
                "Tool {} should have properties",
                tool.name
            );
        }
    }

    #[test]
    fn test_tool_names_are_unique() {
        let tools = create_tools();
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        let original_len = names.len();
        names.sort();
        names.dedup();

        assert_eq!(names.len(), original_len, "All tool names should be unique");
    }
}
