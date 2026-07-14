//! Integration tests for ACP providers.
//!
//! These tests require real CLI tools installed:
//! - Claude: `npm install -g @zed-industries/claude-agent-acp`
//! - Gemini: `npm install -g @google/gemini-cli`
//!
//! Tests are skipped automatically if the required CLI is not found.

use nevoflux_llm::providers::acp::{AcpProvider, AcpUpdate, ContentBlock, TextContent};
use std::path::PathBuf;

fn claude_acp_available() -> bool {
    which::which("claude-agent-acp").is_ok()
}

fn gemini_acp_available() -> bool {
    which::which("gemini").is_ok()
}

fn antigravity_acp_available() -> bool {
    which::which("antigravity-acp").is_ok()
}

#[tokio::test]
async fn test_claude_acp_basic_prompt() {
    if !claude_acp_available() {
        eprintln!("SKIP: claude-agent-acp not installed");
        return;
    }

    let config = nevoflux_llm::providers::acp::claude::build_config(PathBuf::from("."));
    let mut provider = AcpProvider::new(config);
    provider.connect().await.expect("Failed to connect");
    assert!(provider.is_alive());

    let session_id = provider
        .new_session()
        .await
        .expect("Failed to create session");

    let content = vec![ContentBlock::Text(TextContent::new(
        "Say exactly: hello world".to_string(),
    ))];

    let mut rx = provider
        .prompt(session_id, content)
        .await
        .expect("Failed to prompt");

    let mut got_text = false;
    let mut got_complete = false;
    while let Some(update) = rx.recv().await {
        match update {
            AcpUpdate::Text(_) => got_text = true,
            AcpUpdate::Complete(_) => {
                got_complete = true;
                break;
            }
            AcpUpdate::Error(e) => panic!("ACP error: {}", e),
            _ => {}
        }
    }

    assert!(got_text, "Should have received text");
    assert!(got_complete, "Should have received completion");
    provider.shutdown().await;
}

#[tokio::test]
async fn test_gemini_acp_basic_prompt() {
    if !gemini_acp_available() {
        eprintln!("SKIP: gemini CLI not installed");
        return;
    }

    let config = nevoflux_llm::providers::acp::gemini::build_config("default", PathBuf::from("."));
    let mut provider = AcpProvider::new(config);
    provider.connect().await.expect("Failed to connect");

    let session_id = provider
        .new_session()
        .await
        .expect("Failed to create session");

    let content = vec![ContentBlock::Text(TextContent::new(
        "Say exactly: hello world".to_string(),
    ))];

    let mut rx = provider
        .prompt(session_id, content)
        .await
        .expect("Failed to prompt");

    let mut got_text = false;
    while let Some(update) = rx.recv().await {
        match update {
            AcpUpdate::Text(_) => got_text = true,
            AcpUpdate::Complete(_) => break,
            AcpUpdate::Error(e) => panic!("ACP error: {}", e),
            _ => {}
        }
    }

    assert!(got_text, "Should have received text");
    provider.shutdown().await;
}

#[tokio::test]
async fn test_antigravity_acp_basic_prompt() {
    if !antigravity_acp_available() {
        eprintln!("SKIP: antigravity-acp not installed");
        return;
    }

    // Live end-to-end through our own wrapper: antigravity::build_config ->
    // AcpProvider -> real antigravity-acp adapter -> real agy -> streamed reply.
    let config = nevoflux_llm::providers::acp::antigravity::build_config("", PathBuf::from("."));
    let mut provider = AcpProvider::new(config);
    provider.connect().await.expect("Failed to connect");
    assert!(provider.is_alive());

    let session_id = provider
        .new_session()
        .await
        .expect("Failed to create session");

    let content = vec![ContentBlock::Text(TextContent::new(
        "Say exactly: hello world".to_string(),
    ))];

    let mut rx = provider
        .prompt(session_id, content)
        .await
        .expect("Failed to prompt");

    let mut transcript = String::new();
    let mut got_complete = false;
    while let Some(update) = rx.recv().await {
        match update {
            AcpUpdate::Text(t) => transcript.push_str(&format!("{t:?}")),
            AcpUpdate::Complete(_) => {
                got_complete = true;
                break;
            }
            AcpUpdate::Error(e) => panic!("ACP error: {}", e),
            _ => {}
        }
    }

    // Guard against a false pass on the adapter's own error narration: earlier
    // the adapter emitted "agy not found — downloading … 404" AS text and the
    // test passed. A genuine run must reach a real completion and NOT carry the
    // agy-resolution failure markers (fixed by AGY_BIN in build_config).
    let lower = transcript.to_lowercase();
    assert!(
        !lower.contains("agy not found") && !lower.contains("404 not found"),
        "adapter could not resolve agy (AGY_BIN not honored?): {transcript}"
    );
    assert!(
        got_complete,
        "real agy turn must complete; got transcript: {transcript}"
    );
    provider.shutdown().await;
}
