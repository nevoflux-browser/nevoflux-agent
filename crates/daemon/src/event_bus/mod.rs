//! EventBus - Publish/subscribe event routing for NevoFlux Agent.
//!
//! Provides topic-based event routing with three delivery modes:
//! - Ephemeral: fire-and-forget, not retained
//! - Sticky: retained in memory, replayed to new subscribers
//! - Persistent: written to SQLite for durability and history queries

pub mod topic;
pub mod types;

pub use topic::{validate_topic, validate_pattern, TopicError};
pub use types::{
    BackpressurePolicy, BusEvent, Delivery, PublisherIdentity, Subscription,
    SubscriberIdentity, TopicPattern,
};
