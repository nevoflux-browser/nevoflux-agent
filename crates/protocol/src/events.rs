// crates/protocol/src/events.rs

//! EventBus protocol message definitions.
//!
//! Defines request and response types for EventBus communication between
//! the daemon and clients (extensions, WASM plugins, MCP servers).
//!
//! # Message Flow
//!
//! - **Client -> Daemon**: [`EventBusRequest`] variants (subscribe, publish, etc.)
//! - **Daemon -> Client**: [`EventBusResponse`] variants (confirmations, deliveries, errors)

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ============================================================================
// Default value functions for serde
// ============================================================================

fn default_buffer_size() -> usize {
    256
}

fn default_backpressure() -> String {
    "drop_oldest".into()
}

fn default_delivery() -> String {
    "ephemeral".into()
}

fn default_limit() -> usize {
    50
}

// ============================================================================
// Client -> Daemon Requests
// ============================================================================

/// Request messages sent from clients to the daemon's EventBus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum EventBusRequest {
    /// Subscribe to events matching a topic pattern.
    Subscribe(SubscribeRequest),
    /// Unsubscribe from a previously created subscription.
    Unsubscribe(UnsubscribeRequest),
    /// Publish an event to a topic.
    Publish(PublishRequest),
    /// Query historical events for a topic.
    History(HistoryRequest),
}

/// Request to subscribe to events on a topic or pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscribeRequest {
    /// Client-generated request ID for correlation.
    pub request_id: String,
    /// Topic or pattern to subscribe to (e.g. `"agent.status"` or `"agent.*"`).
    pub pattern: String,
    /// Whether `pattern` contains wildcards (`*`).
    #[serde(default)]
    pub is_wildcard: bool,
    /// Maximum number of events buffered per subscriber before backpressure kicks in.
    #[serde(default = "default_buffer_size")]
    pub buffer_size: usize,
    /// Backpressure strategy when the buffer is full.
    /// Supported values: `"drop_oldest"`, `"drop_newest"`, `"block"`.
    #[serde(default = "default_backpressure")]
    pub backpressure: String,
}

/// Request to remove an existing subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsubscribeRequest {
    /// ID of the subscription to remove (returned in [`SubscribedResponse`]).
    pub subscription_id: String,
}

/// Request to publish an event to a topic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublishRequest {
    /// Topic to publish to (e.g. `"agent.status.changed"`).
    pub topic: String,
    /// Event payload (arbitrary JSON).
    pub payload: Value,
    /// Delivery mode: `"ephemeral"` (in-memory only) or `"persistent"` (stored to SQLite).
    #[serde(default = "default_delivery")]
    pub delivery: String,
    /// Time-to-live in seconds for persistent events. `None` means no expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

/// Request to query historical events for a topic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRequest {
    /// Client-generated request ID for correlation.
    pub request_id: String,
    /// Topic to query history for.
    pub topic: String,
    /// Maximum number of events to return.
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Only return events with timestamp strictly after this value (Unix millis).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<i64>,
}

// ============================================================================
// Daemon -> Client Responses
// ============================================================================

/// Response messages sent from the daemon's EventBus to clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum EventBusResponse {
    /// Confirmation that a subscription was created.
    Subscribed(SubscribedResponse),
    /// Confirmation that a subscription was removed.
    Unsubscribed(UnsubscribedResponse),
    /// Confirmation that an event was published.
    Published(PublishedResponse),
    /// A live event delivered to a subscriber.
    EventDelivery(EventDelivery),
    /// Result of a history query.
    HistoryResult(HistoryResult),
    /// An error occurred processing a request.
    Error(EventBusErrorResponse),
}

/// Confirmation that a subscription was successfully created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscribedResponse {
    /// The original request ID from [`SubscribeRequest`].
    pub request_id: String,
    /// Unique subscription ID assigned by the daemon.
    pub subscription_id: String,
}

/// Confirmation that a subscription was removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsubscribedResponse {
    /// ID of the removed subscription.
    pub subscription_id: String,
    /// Whether the unsubscribe succeeded (false if subscription was not found).
    pub success: bool,
}

/// Confirmation that an event was published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedResponse {
    /// Unique event ID assigned by the daemon.
    pub event_id: String,
    /// Topic the event was published to.
    pub topic: String,
}

/// A live event delivered to a matching subscriber.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventDelivery {
    /// Subscription ID that matched this event.
    pub subscription_id: String,
    /// Unique event ID.
    pub event_id: String,
    /// Topic the event was published to.
    pub topic: String,
    /// Event payload.
    pub payload: Value,
    /// Kind of the publisher (e.g. `"extension"`, `"wasm"`, `"mcp"`).
    pub publisher_kind: String,
    /// Unix timestamp in milliseconds when the event was created.
    pub timestamp: i64,
}

/// A historical event returned from a history query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEvent {
    /// Unique event ID.
    pub id: String,
    /// Topic the event was published to.
    pub topic: String,
    /// Event payload.
    pub payload: Value,
    /// Kind of the publisher.
    pub publisher_kind: String,
    /// Identifier of the publisher.
    pub publisher_id: String,
    /// Unix timestamp in milliseconds when the event was created.
    pub created_at: i64,
    /// Unix timestamp in milliseconds when the event expires. `None` means no expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

/// Result of a history query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryResult {
    /// The original request ID from [`HistoryRequest`].
    pub request_id: String,
    /// The matching historical events.
    pub events: Vec<HistoryEvent>,
    /// Total number of events matching the query (may exceed `events.len()` if limited).
    pub total: usize,
}

/// Error response from the EventBus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventBusErrorResponse {
    /// Machine-readable error code (e.g. `"invalid_topic"`, `"not_found"`).
    pub code: String,
    /// Human-readable error description.
    pub message: String,
    /// The request ID that caused the error, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_subscribe_request_serialization() {
        let req = EventBusRequest::Subscribe(SubscribeRequest {
            request_id: "req-1".into(),
            pattern: "agent.status".into(),
            is_wildcard: false,
            buffer_size: 128,
            backpressure: "drop_newest".into(),
        });
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["type"], "subscribe");
        assert_eq!(json["payload"]["request_id"], "req-1");
        assert_eq!(json["payload"]["pattern"], "agent.status");
        assert_eq!(json["payload"]["is_wildcard"], false);
        assert_eq!(json["payload"]["buffer_size"], 128);
        assert_eq!(json["payload"]["backpressure"], "drop_newest");

        let roundtrip: EventBusRequest = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip, req);
    }

    #[test]
    fn test_subscribe_request_defaults() {
        let json_str = r#"{
            "type": "subscribe",
            "payload": {
                "request_id": "req-2",
                "pattern": "agent.*"
            }
        }"#;
        let req: EventBusRequest = serde_json::from_str(json_str).unwrap();
        match req {
            EventBusRequest::Subscribe(sub) => {
                assert_eq!(sub.request_id, "req-2");
                assert_eq!(sub.pattern, "agent.*");
                assert!(!sub.is_wildcard);
                assert_eq!(sub.buffer_size, 256);
                assert_eq!(sub.backpressure, "drop_oldest");
            }
            _ => panic!("Expected Subscribe variant"),
        }
    }

    #[test]
    fn test_publish_request_serialization() {
        let req = EventBusRequest::Publish(PublishRequest {
            topic: "agent.status.changed".into(),
            payload: json!({"state": "idle"}),
            delivery: "persistent".into(),
            ttl_secs: Some(3600),
        });
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["type"], "publish");
        assert_eq!(json["payload"]["topic"], "agent.status.changed");
        assert_eq!(json["payload"]["payload"]["state"], "idle");
        assert_eq!(json["payload"]["delivery"], "persistent");
        assert_eq!(json["payload"]["ttl_secs"], 3600);

        let roundtrip: EventBusRequest = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip, req);
    }

    #[test]
    fn test_publish_request_defaults() {
        let json_str = r#"{
            "type": "publish",
            "payload": {
                "topic": "test.topic",
                "payload": {"key": "value"}
            }
        }"#;
        let req: EventBusRequest = serde_json::from_str(json_str).unwrap();
        match req {
            EventBusRequest::Publish(pub_req) => {
                assert_eq!(pub_req.topic, "test.topic");
                assert_eq!(pub_req.delivery, "ephemeral");
                assert!(pub_req.ttl_secs.is_none());
            }
            _ => panic!("Expected Publish variant"),
        }
    }

    #[test]
    fn test_unsubscribe_request_serialization() {
        let req = EventBusRequest::Unsubscribe(UnsubscribeRequest {
            subscription_id: "sub-123".into(),
        });
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["type"], "unsubscribe");
        assert_eq!(json["payload"]["subscription_id"], "sub-123");

        let roundtrip: EventBusRequest = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip, req);
    }

    #[test]
    fn test_history_request_serialization() {
        let req = EventBusRequest::History(HistoryRequest {
            request_id: "hist-1".into(),
            topic: "agent.status".into(),
            limit: 25,
            after: Some(1700000000000),
        });
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["type"], "history");
        assert_eq!(json["payload"]["request_id"], "hist-1");
        assert_eq!(json["payload"]["topic"], "agent.status");
        assert_eq!(json["payload"]["limit"], 25);
        assert_eq!(json["payload"]["after"], 1700000000000_i64);

        let roundtrip: EventBusRequest = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip, req);
    }

    #[test]
    fn test_subscribed_response_serialization() {
        let resp = EventBusResponse::Subscribed(SubscribedResponse {
            request_id: "req-1".into(),
            subscription_id: "sub-456".into(),
        });
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["type"], "subscribed");
        assert_eq!(json["payload"]["request_id"], "req-1");
        assert_eq!(json["payload"]["subscription_id"], "sub-456");

        let roundtrip: EventBusResponse = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip, resp);
    }

    #[test]
    fn test_event_delivery_serialization() {
        let resp = EventBusResponse::EventDelivery(EventDelivery {
            subscription_id: "sub-456".into(),
            event_id: "evt-789".into(),
            topic: "agent.status.changed".into(),
            payload: json!({"state": "busy"}),
            publisher_kind: "extension".into(),
            timestamp: 1700000000000,
        });
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["type"], "event_delivery");
        assert_eq!(json["payload"]["subscription_id"], "sub-456");
        assert_eq!(json["payload"]["event_id"], "evt-789");
        assert_eq!(json["payload"]["topic"], "agent.status.changed");
        assert_eq!(json["payload"]["payload"]["state"], "busy");
        assert_eq!(json["payload"]["publisher_kind"], "extension");
        assert_eq!(json["payload"]["timestamp"], 1700000000000_i64);

        let roundtrip: EventBusResponse = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip, resp);
    }

    #[test]
    fn test_history_result_serialization() {
        let resp = EventBusResponse::HistoryResult(HistoryResult {
            request_id: "hist-1".into(),
            events: vec![
                HistoryEvent {
                    id: "evt-1".into(),
                    topic: "agent.status".into(),
                    payload: json!({"state": "idle"}),
                    publisher_kind: "wasm".into(),
                    publisher_id: "plugin-abc".into(),
                    created_at: 1700000000000,
                    expires_at: Some(1700003600000),
                },
                HistoryEvent {
                    id: "evt-2".into(),
                    topic: "agent.status".into(),
                    payload: json!({"state": "busy"}),
                    publisher_kind: "extension".into(),
                    publisher_id: "ext-xyz".into(),
                    created_at: 1700000001000,
                    expires_at: None,
                },
            ],
            total: 2,
        });
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["type"], "history_result");
        assert_eq!(json["payload"]["events"].as_array().unwrap().len(), 2);
        assert_eq!(json["payload"]["total"], 2);
        assert_eq!(json["payload"]["events"][0]["id"], "evt-1");
        assert_eq!(json["payload"]["events"][1]["expires_at"], serde_json::Value::Null);

        let roundtrip: EventBusResponse = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip, resp);
    }

    #[test]
    fn test_error_response_serialization() {
        let resp = EventBusResponse::Error(EventBusErrorResponse {
            code: "invalid_topic".into(),
            message: "Topic contains invalid characters".into(),
            request_id: Some("req-bad".into()),
        });
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["type"], "error");
        assert_eq!(json["payload"]["code"], "invalid_topic");
        assert_eq!(json["payload"]["message"], "Topic contains invalid characters");
        assert_eq!(json["payload"]["request_id"], "req-bad");

        let roundtrip: EventBusResponse = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip, resp);

        // Also test without request_id
        let resp_no_id = EventBusResponse::Error(EventBusErrorResponse {
            code: "internal".into(),
            message: "Something went wrong".into(),
            request_id: None,
        });
        let json_no_id = serde_json::to_value(&resp_no_id).unwrap();
        assert!(!json_no_id["payload"]
            .as_object()
            .unwrap()
            .contains_key("request_id"));
        let roundtrip_no_id: EventBusResponse = serde_json::from_value(json_no_id).unwrap();
        assert_eq!(roundtrip_no_id, resp_no_id);
    }

    #[test]
    fn test_all_request_variants_roundtrip() {
        let requests = vec![
            EventBusRequest::Subscribe(SubscribeRequest {
                request_id: "r1".into(),
                pattern: "test.*".into(),
                is_wildcard: true,
                buffer_size: 64,
                backpressure: "block".into(),
            }),
            EventBusRequest::Unsubscribe(UnsubscribeRequest {
                subscription_id: "sub-1".into(),
            }),
            EventBusRequest::Publish(PublishRequest {
                topic: "test.event".into(),
                payload: json!({"data": 42}),
                delivery: "persistent".into(),
                ttl_secs: Some(120),
            }),
            EventBusRequest::History(HistoryRequest {
                request_id: "h1".into(),
                topic: "test.event".into(),
                limit: 10,
                after: None,
            }),
        ];
        for req in &requests {
            let json = serde_json::to_string(req).unwrap();
            let roundtrip: EventBusRequest = serde_json::from_str(&json).unwrap();
            assert_eq!(&roundtrip, req);
        }
    }

    #[test]
    fn test_all_response_variants_roundtrip() {
        let responses = vec![
            EventBusResponse::Subscribed(SubscribedResponse {
                request_id: "r1".into(),
                subscription_id: "sub-1".into(),
            }),
            EventBusResponse::Unsubscribed(UnsubscribedResponse {
                subscription_id: "sub-1".into(),
                success: true,
            }),
            EventBusResponse::Published(PublishedResponse {
                event_id: "evt-1".into(),
                topic: "test.event".into(),
            }),
            EventBusResponse::EventDelivery(EventDelivery {
                subscription_id: "sub-1".into(),
                event_id: "evt-1".into(),
                topic: "test.event".into(),
                payload: json!(null),
                publisher_kind: "mcp".into(),
                timestamp: 0,
            }),
            EventBusResponse::HistoryResult(HistoryResult {
                request_id: "h1".into(),
                events: vec![],
                total: 0,
            }),
            EventBusResponse::Error(EventBusErrorResponse {
                code: "not_found".into(),
                message: "Subscription not found".into(),
                request_id: Some("r1".into()),
            }),
        ];
        for resp in &responses {
            let json = serde_json::to_string(resp).unwrap();
            let roundtrip: EventBusResponse = serde_json::from_str(&json).unwrap();
            assert_eq!(&roundtrip, resp);
        }
    }
}
