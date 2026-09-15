//! Task 1.6: hot-swappable llm-gateway upstream.
//!
//! Starts a real [`serve`] gateway pointed at a fake OpenAI-protocol
//! upstream A, sends a chat-completions request through it, swaps the
//! upstream to a second fake upstream B via [`GatewayHandle::set_upstream`],
//! and confirms the *next* request round-trips through B instead — while
//! the gateway's own bind address and bearer token never change.

use std::net::SocketAddr;
use std::time::Duration;

use axum::{routing::post, Json, Router};
use nevoflux_llm_gateway::{serve, GatewayConfig, UpstreamProtocol, UpstreamUpdate};
use serde_json::{json, Value};
use tokio::net::TcpListener;

const BEARER: &str = "upstream-swap-test-token";

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
        .set_upstream(UpstreamUpdate {
            base_url: format!("http://{addr_b}"),
            api_key: String::new(),
            model_override: String::new(),
            protocol: UpstreamProtocol::OpenAi,
        })
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
        .set_upstream(UpstreamUpdate {
            base_url: format!("http://{addr_b}"),
            api_key: String::new(),
            model_override: String::new(),
            protocol: UpstreamProtocol::OpenAi,
        })
        .await;

    let body_b = send_chat_request(&client, &url).await;
    assert_eq!(body_b["choices"][0]["message"]["content"], "B");

    let snapshot = control.upstream_snapshot().await;
    assert_eq!(snapshot.base_url, format!("http://{addr_b}"));

    handle.shutdown().await;
}
