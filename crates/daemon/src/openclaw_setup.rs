//! OpenClaw automatic configuration.
//!
//! Handles first-time setup of OpenClaw gateway configuration:
//! - Registers NevoFlux MCP HTTP server
//! - Disables OpenClaw's built-in browser tool (replaced by NevoFlux browser tools)
//! - Installs nevoflux-browser skill

use std::process::Command;

/// Check if OpenClaw CLI is installed.
pub fn is_openclaw_installed() -> bool {
    which::which("openclaw").is_ok()
}

/// Check if NevoFlux MCP server is already registered in OpenClaw config.
pub fn is_mcp_configured() -> bool {
    let output = Command::new("openclaw")
        .args(["config", "get", "tools.mcpServers.nevoflux-tools"])
        .output();
    match output {
        Ok(o) => o.status.success() && !String::from_utf8_lossy(&o.stdout).contains("not found"),
        Err(_) => false,
    }
}

/// Register NevoFlux MCP HTTP server in OpenClaw gateway config.
pub fn register_mcp_server(port: u16) -> Result<(), String> {
    let url = format!("http://127.0.0.1:{}/mcp", port);
    let value = serde_json::json!({
        "type": "http",
        "url": url,
    });
    let output = Command::new("openclaw")
        .args([
            "config",
            "set",
            "tools.mcpServers.nevoflux-tools",
            &serde_json::to_string(&value).unwrap(),
            "--strict-json",
        ])
        .output()
        .map_err(|e| format!("Failed to run openclaw config set: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("openclaw config set failed: {}", stderr));
    }
    tracing::info!("Registered NevoFlux MCP server in OpenClaw config: {}", url);
    Ok(())
}

/// Disable OpenClaw's built-in browser tool by adding "browser" to tools.deny.
/// Merges with existing deny list to avoid overwriting user config.
pub fn disable_openclaw_browser() -> Result<(), String> {
    // Read existing deny list
    let existing = Command::new("openclaw")
        .args(["config", "get", "tools.deny"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                serde_json::from_str::<Vec<String>>(&s).ok()
            } else {
                None
            }
        })
        .unwrap_or_default();

    // Add "browser" if not already present
    let mut deny_list = existing;
    if !deny_list.iter().any(|s| s == "browser") {
        deny_list.push("browser".to_string());
    }

    let output = Command::new("openclaw")
        .args([
            "config",
            "set",
            "tools.deny",
            &serde_json::to_string(&deny_list).unwrap(),
            "--strict-json",
        ])
        .output()
        .map_err(|e| format!("Failed to set tools.deny: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("tools.deny set failed: {}", stderr));
    }
    tracing::info!("Disabled OpenClaw built-in browser tool");
    Ok(())
}

/// Install the nevoflux-browser skill into OpenClaw workspace.
pub fn install_nevoflux_skill() -> Result<(), String> {
    let skill_dir = dirs::home_dir()
        .ok_or_else(|| "Cannot determine home directory".to_string())?
        .join(".openclaw/workspace/skills/nevoflux-browser");

    if skill_dir.join("SKILL.md").exists() {
        tracing::info!("NevoFlux browser skill already installed");
        return Ok(());
    }

    std::fs::create_dir_all(&skill_dir)
        .map_err(|e| format!("Failed to create skill directory: {}", e))?;

    let skill_content = r#"---
name: nevoflux-browser
description: Control NevoFlux browser for web browsing, page reading, and interaction. Use when user asks to navigate, read pages, click elements, or take screenshots. Prefer NevoFlux MCP tools over built-in browser tool.
metadata: { "openclaw": { "requires": { "config": ["tools.mcpServers.nevoflux-tools"] } } }
---

# NevoFlux Browser Control

NevoFlux provides browser tools through MCP server "nevoflux-tools".
Use these tools for all browser operations.

## Available Tools
- `browser_get_markdown` — Read current page as markdown
- `browser_navigate` — Navigate to URL
- `browser_click_by_id` — Click page element by snapshot ID
- `browser_snapshot` — Get page element structure (accessibility tree)
- `browser_screenshot` — Take screenshot of current page
- `browser_scroll` — Scroll page up or down
- `browser_get_tabs` — List all open browser tabs
- `browser_get_content` — Get page HTML source
- `browser_eval_js` — Execute JavaScript on current page
- `web_search` — Search the web
- `fetch_page` — Fetch URL content as markdown
- `create_artifact` — Create interactive Canvas app (HTML/React/SVG/Mermaid/Markdown)

## Usage Notes
- These are MCP tools provided by the NevoFlux browser engine
- Use `browser_get_markdown` to read page content before answering questions about a page
- Use `browser_snapshot` to get interactive element IDs before clicking
- Use `create_artifact` to build visual apps, dashboards, and tools
"#;

    std::fs::write(skill_dir.join("SKILL.md"), skill_content)
        .map_err(|e| format!("Failed to write SKILL.md: {}", e))?;

    tracing::info!("Installed NevoFlux browser skill to {:?}", skill_dir);
    Ok(())
}

/// Run the complete first-time OpenClaw setup.
/// Returns Ok(true) if setup was performed, Ok(false) if already configured.
pub fn ensure_openclaw_configured(_mcp_port: u16) -> Result<bool, String> {
    if !is_openclaw_installed() {
        return Err(
            "OpenClaw is not installed. Install with: npm install -g openclaw@latest && openclaw onboard"
                .to_string(),
        );
    }

    // Check if skill is already installed as indicator of setup completion
    let skill_dir = dirs::home_dir()
        .ok_or_else(|| "Cannot determine home directory".to_string())?
        .join(".openclaw/workspace/skills/nevoflux-browser");
    if skill_dir.join("SKILL.md").exists() {
        return Ok(false); // Already configured
    }

    // First-time setup
    // Note: OpenClaw doesn't support HTTP MCP servers via config.
    // NevoFlux tools are exposed through <tool_call> XML in system prompt
    // and the nevoflux-browser skill teaches OpenClaw how to use them.
    // register_mcp_server is skipped — tools defined in system prompt instead.
    let _ = disable_openclaw_browser(); // Best effort — may fail if tools.deny path is invalid
    install_nevoflux_skill()?;

    tracing::info!(
        "OpenClaw first-time setup complete. Please restart OpenClaw gateway: openclaw gateway restart"
    );
    Ok(true)
}
