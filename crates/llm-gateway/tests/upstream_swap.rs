//! Task 1.6: hot-swappable llm-gateway upstream.
//!
//! Starts a real [`serve`] gateway pointed at a fake OpenAI-protocol
//! upstream A, sends a chat-completions request through it, swaps the
//! upstream to a second fake upstream B via [`GatewayHandle::set_upstream`],
//! and confirms the *next* request round-trips through B instead — while
//! the gateway's own bind address and bearer token never change.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use axum::{routing::post, Json, Router};
use nevoflux_llm_gateway::{
    serve, AcpProviderConfig, GatewayConfig, UpstreamProtocol, UpstreamUpdate,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

const BEARER: &str = "upstream-swap-test-token";

/// Build an [`UpstreamUpdate`] with every field explicit but the common
/// case (no ACP, available) defaulted, so each test only spells out what
/// it's actually exercising.
fn upstream_update(base_url: String, protocol: UpstreamProtocol) -> UpstreamUpdate {
    UpstreamUpdate {
        base_url,
        api_key: String::new(),
        model_override: String::new(),
        protocol,
        acp_config: None,
        unavailable: false,
    }
}

/// An `AcpProviderConfig` that will never successfully spawn (bogus
/// command), for tests that only need to prove a request gets *past* the
/// "no acp_config" gateway guard — not that a live ACP session succeeds.
fn unreachable_acp_config() -> AcpProviderConfig {
    AcpProviderConfig {
        command: PathBuf::from("definitely-not-a-real-binary-xyz"),
        args: vec![],
        env: vec![],
        env_remove: vec![],
        work_dir: std::env::temp_dir(),
        session_mode: "code".into(),
        use_mcp_bridge: false,
        inject_mcp_url: false,
        gate_tool_calls: false,
        config_options: vec![],
    }
}

/// Spin up a tiny OpenAI-protocol fake upstream that always answers
/// `/v1/chat/completions` with the same canned `content` string, wrapped
/// in an OpenAI chat-completion response shape (the gateway's OpenAI
/// passthrough path forwards the upstream body verbatim, so the test
/// client sees this shape too).
async fn spawn_fake_upstream(content: &'static str) -> SocketAddr {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || async move {
            Json(json!({
                "id": "cmpl-test",
                "object": "chat.completion",
                "created": 0,
                "model": "fake",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": content},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            }))
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake upstream");
    let addr = listener.local_addr().expect("fake upstream local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("fake upstream serve");
    });
    addr
}

fn base_config(upstream_base_url: String) -> GatewayConfig {
    GatewayConfig {
        bind_addr: "127.0.0.1:0".parse().expect("loopback addr"),
        bearer_token: BEARER.to_string(),
        upstream_base_url,
        upstream_api_key: String::new(),
        upstream_model_remap: None,
        anthropic_version: "2023-06-01".to_string(),
        upstream_request_timeout: Duration::from_secs(10),
        upstream_connect_timeout: Duration::from_secs(5),
        upstream_stream_idle_timeout: Duration::from_secs(10),
        upstream_retry_max_wait: Duration::from_secs(1),
        advertised_models: vec![],
        upstream_protocol: UpstreamProtocol::OpenAi,
        acp_config: None,
    }
}

async fn send_chat_request(client: &reqwest::Client, url: &str) -> Value {
    let resp = client
        .post(url)
        .bearer_auth(BEARER)
        .json(&json!({
            "model": "placeholder-model",
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .expect("request to gateway should succeed");
    assert!(
        resp.status().is_success(),
        "gateway response should be 2xx, got {}",
        resp.status()
    );
    resp.json().await.expect("gateway response should be JSON")
}

#[tokio::test]
async fn set_upstream_swaps_which_backend_serves_the_next_request() {
    let addr_a = spawn_fake_upstream("A").await;
    let addr_b = spawn_fake_upstream("B").await;

    let handle = serve(base_config(format!("http://{addr_a}")))
        .await
        .expect("gateway should start");
    let original_bind_addr = handle.bind_addr;
    let original_bearer = handle.bearer_token.clone();

    let client = reqwest::Client::new();
    let url = format!("{}/v1/chat/completions", handle.url());

    // Before any swap: requests go to fake upstream A.
    let body_a = send_chat_request(&client, &url).await;
    assert_eq!(body_a["choices"][0]["message"]["content"], "A");

    // Hot-swap to fake upstream B.
    handle
        .set_upstream(upstream_update(
            format!("http://{addr_b}"),
            UpstreamProtocol::OpenAi,
        ))
        .await;

    // The very next request goes to B instead, with no restart / re-bind.
    let body_b = send_chat_request(&client, &url).await;
    assert_eq!(body_b["choices"][0]["message"]["content"], "B");

    // set_upstream must never touch the bind address or the bearer token.
    assert_eq!(handle.bind_addr, original_bind_addr);
    assert_eq!(handle.bearer_token, original_bearer);

    let snapshot = handle.upstream_snapshot().await;
    assert_eq!(snapshot.base_url, format!("http://{addr_b}"));
    assert_eq!(snapshot.protocol, UpstreamProtocol::OpenAi);

    handle.shutdown().await;
}

#[tokio::test]
async fn control_handle_can_swap_upstream_without_the_owning_gateway_handle() {
    // Task 1.6 / controller ruling: the daemon's config-change handlers and
    // the future engine supervisor don't hold a `&GatewayHandle` (it owns
    // non-Clone shutdown machinery) — they get a cheap `GatewayControl`
    // clone instead. Confirm that clone can drive the same swap.
    let addr_a = spawn_fake_upstream("A").await;
    let addr_b = spawn_fake_upstream("B").await;

    let handle = serve(base_config(format!("http://{addr_a}")))
        .await
        .expect("gateway should start");
    let control = handle.control();

    let client = reqwest::Client::new();
    let url = format!("{}/v1/chat/completions", handle.url());

    let body_a = send_chat_request(&client, &url).await;
    assert_eq!(body_a["choices"][0]["message"]["content"], "A");

    control
        .set_upstream(upstream_update(
            format!("http://{addr_b}"),
            UpstreamProtocol::OpenAi,
        ))
        .await;

    let body_b = send_chat_request(&client, &url).await;
    assert_eq!(body_b["choices"][0]["message"]["content"], "B");

    let snapshot = control.upstream_snapshot().await;
    assert_eq!(snapshot.base_url, format!("http://{addr_b}"));

    handle.shutdown().await;
}

#[tokio::test]
async fn set_upstream_to_acp_builds_a_client_on_demand_when_boot_was_not_acp() {
    // Fix round 1, item 1: the gateway boots on a plain OpenAI upstream
    // (no `acp_config`, so `AppState::new` never builds an `AcpUpstream`).
    // Switching the active provider to an ACP one at runtime must not
    // permanently 500 with "no acp_config was supplied" — the gateway
    // should build the client from the `UpstreamUpdate`'s `acp_config` on
    // demand.
    let addr_a = spawn_fake_upstream("A").await;
    let handle = serve(base_config(format!("http://{addr_a}")))
        .await
        .expect("gateway should start");

    handle
        .set_upstream(UpstreamUpdate {
            base_url: String::new(),
            api_key: String::new(),
            model_override: String::new(),
            protocol: UpstreamProtocol::Acp,
            acp_config: Some(unreachable_acp_config()),
            unavailable: false,
        })
        .await;

    let client = reqwest::Client::new();
    let url = format!("{}/v1/chat/completions", handle.url());
    let resp = tokio::time::timeout(
        Duration::from_secs(15),
        client
            .post(&url)
            .bearer_auth(BEARER)
            .json(&json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]}))
            .send(),
    )
    .await
    .expect("gateway must respond, not hang")
    .expect("request to gateway should succeed at the HTTP layer");

    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    // The bogus binary will fail to spawn — some OTHER error is expected —
    // but it must not be the "no acp_config was supplied" guard, which
    // would mean the on-demand client was never built.
    assert!(
        !body_text.contains("no acp_config was supplied"),
        "expected to get past the missing-acp_config guard; got {status} {body_text}"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn acp_then_openai_then_acp_reuses_the_same_acp_client() {
    // Fix round 1, item 1: boot WITH acp_config, so `AppState::new`
    // already builds one `AcpUpstream`. Swapping away to OpenAI and back
    // to Acp — this time with `acp_config: None` in the update — must
    // still get past the guard, proving the ORIGINAL client (built once,
    // at boot) survived the round trip rather than being dropped and
    // needing to be re-supplied.
    let mut config = base_config("https://unused.example".to_string());
    config.upstream_protocol = UpstreamProtocol::Acp;
    config.acp_config = Some(unreachable_acp_config());
    let handle = serve(config).await.expect("gateway should start");

    let addr_openai = spawn_fake_upstream("openai-leg").await;
    handle
        .set_upstream(upstream_update(
            format!("http://{addr_openai}"),
            UpstreamProtocol::OpenAi,
        ))
        .await;

    let client = reqwest::Client::new();
    let url = format!("{}/v1/chat/completions", handle.url());
    let body = send_chat_request(&client, &url).await;
    assert_eq!(body["choices"][0]["message"]["content"], "openai-leg");

    // Swap back to Acp WITHOUT re-supplying acp_config.
    handle
        .set_upstream(UpstreamUpdate {
            base_url: String::new(),
            api_key: String::new(),
            model_override: String::new(),
            protocol: UpstreamProtocol::Acp,
            acp_config: None,
            unavailable: false,
        })
        .await;

    let resp = tokio::time::timeout(
        Duration::from_secs(15),
        client
            .post(&url)
            .bearer_auth(BEARER)
            .json(&json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]}))
            .send(),
    )
    .await
    .expect("gateway must respond, not hang")
    .expect("request to gateway should succeed at the HTTP layer");
    let body_text = resp.text().await.unwrap_or_default();
    assert!(
        !body_text.contains("no acp_config was supplied"),
        "the boot-time ACP client must have been reused, got: {body_text}"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn set_upstream_unavailable_short_circuits_with_503_and_no_network_call() {
    // Fix round 1, item 5: latched with no on-device engine published yet
    // must fail immediately with 503, without ever attempting a network
    // call — proven here by pointing `base_url` at a real fake upstream
    // that would otherwise happily answer, and confirming it's never hit.
    let addr = spawn_fake_upstream("should-never-be-reached").await;
    let handle = serve(base_config(format!("http://{addr}")))
        .await
        .expect("gateway should start");

    handle
        .set_upstream(UpstreamUpdate {
            base_url: format!("http://{addr}"),
            api_key: String::new(),
            model_override: String::new(),
            protocol: UpstreamProtocol::OpenAi,
            acp_config: None,
            unavailable: true,
        })
        .await;

    let client = reqwest::Client::new();
    let url = format!("{}/v1/chat/completions", handle.url());
    let resp = client
        .post(&url)
        .bearer_auth(BEARER)
        .json(&json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("request to gateway should succeed at the HTTP layer");
    assert_eq!(resp.status().as_u16(), 503);
    let body: Value = resp.json().await.expect("error body should be JSON");
    assert_eq!(body["error"]["type"], "local_engine_unavailable");

    handle.shutdown().await;
}
