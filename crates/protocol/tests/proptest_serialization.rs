//! Property-based tests for protocol serialization using proptest.
//!
//! These tests verify that serialization round-trips work correctly for all protocol types.

use nevoflux_protocol::{
    AgentState, Channel, DaemonEnvelope, ErrorLevel, JsonRpcId, JsonRpcRequest, JsonRpcResponse,
    PermissionScope, ProxyEnvelope,
};
use proptest::prelude::*;

// Strategies for generating arbitrary values

fn arb_channel() -> impl Strategy<Value = Channel> {
    prop_oneof![Just(Channel::Chat), Just(Channel::Mcp),]
}

fn arb_agent_state() -> impl Strategy<Value = AgentState> {
    prop_oneof![
        Just(AgentState::Idle),
        Just(AgentState::Thinking),
        Just(AgentState::Executing),
        Just(AgentState::ExecutingTool),
        Just(AgentState::Waiting),
        Just(AgentState::WaitingResult),
        Just(AgentState::WaitingConfirmation),
        Just(AgentState::Complete),
        Just(AgentState::Error),
    ]
}

fn arb_permission_scope() -> impl Strategy<Value = PermissionScope> {
    prop_oneof![
        Just(PermissionScope::Once),
        Just(PermissionScope::Session),
        Just(PermissionScope::Always),
    ]
}

fn arb_error_level() -> impl Strategy<Value = ErrorLevel> {
    prop_oneof![
        Just(ErrorLevel::Warning),
        Just(ErrorLevel::Error),
        Just(ErrorLevel::Fatal),
    ]
}

fn arb_json_rpc_id() -> impl Strategy<Value = JsonRpcId> {
    prop_oneof![
        (1i64..1000).prop_map(JsonRpcId::Number),
        "[a-z]{1,10}".prop_map(JsonRpcId::String),
    ]
}

fn arb_simple_json_value() -> impl Strategy<Value = serde_json::Value> {
    prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        (-1000i64..1000).prop_map(|n| serde_json::json!(n)),
        "[a-zA-Z0-9 ]{0,20}".prop_map(|s| serde_json::json!(s)),
    ]
}

// JSON values that are non-null for response results
fn arb_non_null_json_value() -> impl Strategy<Value = serde_json::Value> {
    prop_oneof![
        any::<bool>().prop_map(serde_json::Value::Bool),
        (-1000i64..1000).prop_map(|n| serde_json::json!(n)),
        "[a-zA-Z0-9 ]{1,20}".prop_map(|s| serde_json::json!(s)),
        Just(serde_json::json!({"status": "ok"})),
    ]
}

fn arb_proxy_envelope() -> impl Strategy<Value = ProxyEnvelope> {
    (
        "[a-z]{5,10}",           // proxy_id
        "[a-z]{5,10}",           // request_id
        arb_channel(),           // channel
        arb_simple_json_value(), // payload
        0u64..2000000000000u64,  // timestamp_ms
    )
        .prop_map(
            |(proxy_id, request_id, channel, payload, timestamp_ms)| ProxyEnvelope {
                proxy_id,
                request_id,
                auth: None,
                channel,
                payload,
                timestamp_ms,
            },
        )
}

fn arb_daemon_envelope() -> impl Strategy<Value = DaemonEnvelope> {
    (
        "[a-z]{5,10}",                       // proxy_id
        proptest::option::of("[a-z]{5,10}"), // request_id
        arb_channel(),                       // channel
        arb_simple_json_value(),             // payload
        0u64..2000000000000u64,              // timestamp_ms
    )
        .prop_map(
            |(proxy_id, request_id, channel, payload, timestamp_ms)| DaemonEnvelope {
                proxy_id,
                request_id,
                channel,
                payload,
                timestamp_ms,
            },
        )
}

fn arb_json_rpc_request() -> impl Strategy<Value = JsonRpcRequest> {
    (arb_json_rpc_id(), "[a-z/]{3,20}").prop_map(|(id, method)| JsonRpcRequest::new(id, method))
}

fn arb_json_rpc_response_success() -> impl Strategy<Value = JsonRpcResponse> {
    (arb_json_rpc_id(), arb_non_null_json_value())
        .prop_map(|(id, result)| JsonRpcResponse::success(id, result))
}

proptest! {
    // Channel serialization tests
    #[test]
    fn channel_json_roundtrip(channel in arb_channel()) {
        let json = serde_json::to_string(&channel).unwrap();
        let decoded: Channel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(channel, decoded);
    }

    #[test]
    fn channel_msgpack_roundtrip(channel in arb_channel()) {
        let encoded = rmp_serde::to_vec(&channel).unwrap();
        let decoded: Channel = rmp_serde::from_slice(&encoded).unwrap();
        prop_assert_eq!(channel, decoded);
    }

    // AgentState serialization tests
    #[test]
    fn agent_state_json_roundtrip(state in arb_agent_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let decoded: AgentState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(state, decoded);
    }

    #[test]
    fn agent_state_msgpack_roundtrip(state in arb_agent_state()) {
        let encoded = rmp_serde::to_vec(&state).unwrap();
        let decoded: AgentState = rmp_serde::from_slice(&encoded).unwrap();
        prop_assert_eq!(state, decoded);
    }

    // PermissionScope serialization tests
    #[test]
    fn permission_scope_json_roundtrip(scope in arb_permission_scope()) {
        let json = serde_json::to_string(&scope).unwrap();
        let decoded: PermissionScope = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(scope, decoded);
    }

    // ErrorLevel serialization tests
    #[test]
    fn error_level_json_roundtrip(level in arb_error_level()) {
        let json = serde_json::to_string(&level).unwrap();
        let decoded: ErrorLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(level, decoded);
    }

    // JsonRpcId serialization tests
    #[test]
    fn json_rpc_id_json_roundtrip(id in arb_json_rpc_id()) {
        let json = serde_json::to_string(&id).unwrap();
        let decoded: JsonRpcId = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(id, decoded);
    }

    // ProxyEnvelope serialization tests
    #[test]
    fn proxy_envelope_json_roundtrip(envelope in arb_proxy_envelope()) {
        let json = serde_json::to_string(&envelope).unwrap();
        let decoded: ProxyEnvelope = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(envelope, decoded);
    }

    #[test]
    fn proxy_envelope_msgpack_roundtrip(envelope in arb_proxy_envelope()) {
        let encoded = rmp_serde::to_vec(&envelope).unwrap();
        let decoded: ProxyEnvelope = rmp_serde::from_slice(&encoded).unwrap();
        prop_assert_eq!(envelope, decoded);
    }

    // DaemonEnvelope serialization tests
    #[test]
    fn daemon_envelope_json_roundtrip(envelope in arb_daemon_envelope()) {
        let json = serde_json::to_string(&envelope).unwrap();
        let decoded: DaemonEnvelope = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(envelope, decoded);
    }

    #[test]
    fn daemon_envelope_msgpack_roundtrip(envelope in arb_daemon_envelope()) {
        let encoded = rmp_serde::to_vec(&envelope).unwrap();
        let decoded: DaemonEnvelope = rmp_serde::from_slice(&encoded).unwrap();
        prop_assert_eq!(envelope, decoded);
    }

    // JsonRpcRequest serialization tests
    #[test]
    fn json_rpc_request_json_roundtrip(request in arb_json_rpc_request()) {
        let json = serde_json::to_string(&request).unwrap();
        let decoded: JsonRpcRequest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(request.jsonrpc, decoded.jsonrpc);
        prop_assert_eq!(request.method, decoded.method);
        prop_assert_eq!(request.id, decoded.id);
    }

    // JsonRpcResponse serialization tests
    #[test]
    fn json_rpc_response_json_roundtrip(response in arb_json_rpc_response_success()) {
        let json = serde_json::to_string(&response).unwrap();
        let decoded: JsonRpcResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&response.jsonrpc, &decoded.jsonrpc);
        prop_assert_eq!(&response.id, &decoded.id);
        prop_assert!(response.is_success() == decoded.is_success());
    }

    // Cross-format compatibility tests
    #[test]
    fn proxy_envelope_json_then_msgpack(envelope in arb_proxy_envelope()) {
        // JSON encode, decode, then MessagePack encode, decode
        let json = serde_json::to_string(&envelope).unwrap();
        let from_json: ProxyEnvelope = serde_json::from_str(&json).unwrap();
        let msgpack = rmp_serde::to_vec(&from_json).unwrap();
        let from_msgpack: ProxyEnvelope = rmp_serde::from_slice(&msgpack).unwrap();
        prop_assert_eq!(envelope, from_msgpack);
    }

    #[test]
    fn daemon_envelope_msgpack_then_json(envelope in arb_daemon_envelope()) {
        // MessagePack encode, decode, then JSON encode, decode
        let msgpack = rmp_serde::to_vec(&envelope).unwrap();
        let from_msgpack: DaemonEnvelope = rmp_serde::from_slice(&msgpack).unwrap();
        let json = serde_json::to_string(&from_msgpack).unwrap();
        let from_json: DaemonEnvelope = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(envelope, from_json);
    }
}

// Additional deterministic tests to complement property tests

#[test]
fn test_channel_all_variants_serialize() {
    let channels = [Channel::Chat, Channel::Mcp];
    for channel in channels {
        let json = serde_json::to_string(&channel).unwrap();
        let decoded: Channel = serde_json::from_str(&json).unwrap();
        assert_eq!(channel, decoded);
    }
}

#[test]
fn test_agent_state_all_variants_serialize() {
    let states = [
        AgentState::Idle,
        AgentState::Thinking,
        AgentState::Executing,
        AgentState::ExecutingTool,
        AgentState::Waiting,
        AgentState::WaitingResult,
        AgentState::WaitingConfirmation,
        AgentState::Complete,
        AgentState::Error,
    ];
    for state in states {
        let json = serde_json::to_string(&state).unwrap();
        let decoded: AgentState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, decoded);
    }
}

#[test]
fn test_error_level_all_variants_serialize() {
    let levels = [ErrorLevel::Warning, ErrorLevel::Error, ErrorLevel::Fatal];
    for level in levels {
        let json = serde_json::to_string(&level).unwrap();
        let decoded: ErrorLevel = serde_json::from_str(&json).unwrap();
        assert_eq!(level, decoded);
    }
}

#[test]
fn test_permission_scope_all_variants_serialize() {
    let scopes = [
        PermissionScope::Once,
        PermissionScope::Session,
        PermissionScope::Always,
    ];
    for scope in scopes {
        let json = serde_json::to_string(&scope).unwrap();
        let decoded: PermissionScope = serde_json::from_str(&json).unwrap();
        assert_eq!(scope, decoded);
    }
}
