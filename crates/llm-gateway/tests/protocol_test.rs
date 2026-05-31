//! Unit tests for [`UpstreamProtocol`] (M4-2.6).
//!
//! Covers:
//! - mapping nevoflux provider names to their canonical protocol
//! - parsing config-/env-var-supplied labels
//! - the default protocol is Anthropic (back-compat with M2)
//! - serde round-trip (snake_case rename; OpenAi → `"openai"`)

use nevoflux_llm_gateway::UpstreamProtocol;

#[test]
fn provider_name_anthropic_variants_map_to_anthropic() {
    assert_eq!(
        UpstreamProtocol::from_provider_name("anthropic"),
        UpstreamProtocol::Anthropic
    );
    assert_eq!(
        UpstreamProtocol::from_provider_name("ANTHROPIC"),
        UpstreamProtocol::Anthropic
    );
}

#[test]
fn provider_name_claude_code_maps_to_acp() {
    // Core behavior change: claude_code now drives an ACP session
    // (reusing Claude Code's own auth) instead of HTTP-proxying to
    // api.anthropic.com.
    assert_eq!(
        UpstreamProtocol::from_provider_name("claude_code"),
        UpstreamProtocol::Acp
    );
    assert_eq!(
        UpstreamProtocol::from_provider_name("claude-code"),
        UpstreamProtocol::Acp
    );
    assert_eq!(
        UpstreamProtocol::from_provider_name("CLAUDE-CODE"),
        UpstreamProtocol::Acp
    );
    assert_eq!(
        UpstreamProtocol::from_provider_name("acp"),
        UpstreamProtocol::Acp
    );
}

#[test]
fn parse_label_accepts_acp() {
    assert_eq!(UpstreamProtocol::parse_label("acp"), UpstreamProtocol::Acp);
    assert_eq!(
        UpstreamProtocol::parse_label("  ACP  "),
        UpstreamProtocol::Acp
    );
}

#[test]
fn provider_name_openai_compatible_maps_to_openai() {
    for p in [
        "openai",
        "qwen",
        "deepseek",
        "openrouter",
        "groq",
        "mistral",
    ] {
        assert_eq!(
            UpstreamProtocol::from_provider_name(p),
            UpstreamProtocol::OpenAi,
            "provider {p} should map to OpenAI"
        );
    }
}

#[test]
fn provider_name_unknown_falls_back_to_openai() {
    // The convention for unrecognized provider names is "OpenAI
    // compatible" — most third-party providers expose an OpenAI-shape
    // endpoint these days.
    assert_eq!(
        UpstreamProtocol::from_provider_name("brand-new-provider"),
        UpstreamProtocol::OpenAi
    );
}

#[test]
fn default_protocol_is_anthropic() {
    // Critical for back-compat: any pre-existing GatewayConfig
    // constructed without an explicit protocol must keep running the
    // M2 Anthropic translator path.
    assert_eq!(UpstreamProtocol::default(), UpstreamProtocol::Anthropic);
}

#[test]
fn parse_label_accepts_canonical_strings() {
    assert_eq!(
        UpstreamProtocol::parse_label("anthropic"),
        UpstreamProtocol::Anthropic
    );
    assert_eq!(
        UpstreamProtocol::parse_label("openai"),
        UpstreamProtocol::OpenAi
    );
    // case + whitespace insensitive
    assert_eq!(
        UpstreamProtocol::parse_label("  OPENAI  "),
        UpstreamProtocol::OpenAi
    );
}

#[test]
fn parse_label_garbage_falls_back_to_default() {
    // Misconfigured env vars should not panic — they fall back to
    // the default (Anthropic) so the gateway still runs.
    assert_eq!(
        UpstreamProtocol::parse_label("garbage"),
        UpstreamProtocol::Anthropic
    );
    assert_eq!(
        UpstreamProtocol::parse_label(""),
        UpstreamProtocol::Anthropic
    );
}

#[test]
fn protocol_serializes_to_snake_case() {
    let v = serde_json::to_value(UpstreamProtocol::Anthropic).unwrap();
    assert_eq!(v, serde_json::json!("anthropic"));
    let v = serde_json::to_value(UpstreamProtocol::OpenAi).unwrap();
    assert_eq!(v, serde_json::json!("openai"));
}

#[test]
fn protocol_deserializes_from_snake_case() {
    let p: UpstreamProtocol = serde_json::from_value(serde_json::json!("anthropic")).unwrap();
    assert_eq!(p, UpstreamProtocol::Anthropic);
    let p: UpstreamProtocol = serde_json::from_value(serde_json::json!("openai")).unwrap();
    assert_eq!(p, UpstreamProtocol::OpenAi);
}
