//! Tests for the `GET /v1/models` discovery endpoint (M2-1).
//!
//! These tests drive the gateway router directly via
//! `tower::ServiceExt::oneshot` — no TCP listener is bound, so they're
//! fast and reliable on every platform. The `serve_test_router` helper
//! is gated behind the `test-util` feature, which is turned on for this
//! integration test binary by the crate's own `dev-dependencies` entry
//! re-pulling itself with that feature.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use nevoflux_llm_gateway::{serve_test_router, GatewayConfig};
use serde_json::Value;
use tower::ServiceExt;

#[tokio::test]
async fn models_returns_list_with_explicit_advertised() {
    let mut config = GatewayConfig::test_default();
    config.advertised_models = vec!["claude-haiku-4-5".into(), "claude-sonnet-4-6".into()];
    let app = serve_test_router(config).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("authorization", "Bearer test-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["object"], "list");
    let data = body["data"].as_array().expect("data must be an array");
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["id"], "claude-haiku-4-5");
    assert_eq!(data[0]["object"], "model");
    assert_eq!(data[0]["owned_by"], "nevoflux-gateway");
    assert!(
        data[0]["created"].as_u64().is_some(),
        "`created` must be a u64"
    );
    assert_eq!(data[1]["id"], "claude-sonnet-4-6");
    // Stable, 1-second-per-index offset from the fixed epoch.
    assert_eq!(
        data[1]["created"].as_u64().unwrap(),
        data[0]["created"].as_u64().unwrap() + 1
    );
}

#[tokio::test]
async fn models_falls_back_to_remap_when_advertised_empty() {
    let mut config = GatewayConfig::test_default();
    config.advertised_models = Vec::new();
    config.upstream_model_remap = Some("claude-sonnet-4-6".into());
    let app = serve_test_router(config).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("authorization", "Bearer test-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 1, "single fallback entry expected");
    assert_eq!(data[0]["id"], "claude-sonnet-4-6");
    assert_eq!(data[0]["owned_by"], "nevoflux-gateway");
}

#[tokio::test]
async fn models_returns_default_sentinel_when_no_config() {
    let mut config = GatewayConfig::test_default();
    config.advertised_models = Vec::new();
    config.upstream_model_remap = None;
    let app = serve_test_router(config).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("authorization", "Bearer test-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(
        data[0]["id"], "default",
        "naive clients must always see a non-empty list"
    );
}

#[tokio::test]
async fn models_requires_bearer_auth() {
    let config = GatewayConfig::test_default();
    let app = serve_test_router(config).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "/v1/models must be bearer-gated like other /v1/* endpoints"
    );
}
