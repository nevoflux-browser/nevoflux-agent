//! `jev.test`: the settings page's "test connection" (spec §5.9 item 3) —
//! three tiny requests on one connection, the median latency, and a
//! `timeout_ms` suggestion.

use std::time::{Duration, Instant};

use super::client::{JevClient, JevError};
use super::wire::Question;
use crate::kb_wizard::{err_response, ok_response};
use crate::server::SharedAgentConfig;

const CMD: &str = "jev.test";

/// Twice the median latency, rounded up to 100 ms, never below the spec's
/// default of 800 ms.
pub fn suggested_timeout_ms(p50_ms: u64) -> u64 {
    let doubled = p50_ms.saturating_mul(2);
    let rounded = doubled.div_ceil(100) * 100;
    rounded.max(800)
}

pub async fn handle_test(
    params: &serde_json::Value,
    shared_config: &SharedAgentConfig,
) -> serde_json::Value {
    let request_id = crate::local::rpc::request_id(params);
    let jev = shared_config
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .jev
        .clone();
    let client = match JevClient::from_config(&jev) {
        Ok(c) => c,
        Err(_) => {
            return err_response(
                &request_id,
                CMD,
                "NOT_CONFIGURED",
                "Jev is disabled or has no endpoint/key",
            )
        }
    };
    let mut questions = std::collections::BTreeMap::new();
    questions.insert(
        "ping".to_string(),
        Question::Noul {
            instructions: "Is this a connection test?".into(),
            when_true: "yes".into(),
            when_false: "no".into(),
        },
    );
    let mut latencies = Vec::new();
    for _ in 0..3 {
        let t = Instant::now();
        match client
            .ask(
                serde_json::json!({"purpose": "connection test"}),
                questions.clone(),
                Duration::from_secs(10),
            )
            .await
        {
            Ok(_) => latencies.push(t.elapsed().as_millis() as u64),
            Err(JevError::Refused(m)) => return err_response(&request_id, CMD, "REFUSED", m),
            Err(e) => return err_response(&request_id, CMD, "FAILED", e.to_string()),
        }
    }
    let mut sorted = latencies.clone();
    sorted.sort_unstable();
    let p50 = sorted[1];
    ok_response(
        &request_id,
        CMD,
        serde_json::json!({
            "latency_ms": latencies,
            "p50_ms": p50,
            "suggested_timeout_ms": suggested_timeout_ms(p50),
            "model": jev.model,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opt-in live check against the real System One endpoint. Needs
    /// `NEVOFLUX_API_KEY_TYPESAFE`; sends only public data; prints latency and
    /// the raw shape of one Score answer, never the key.
    ///
    /// `cargo test -j 3 -p nevoflux-daemon --lib jev_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn jev_live_test_connection() {
        let mut cfg = crate::config::AgentConfig::default();
        cfg.jev.enabled = true;
        if cfg.jev.resolved_api_key().is_none() {
            eprintln!("skipped: {} is not set", crate::config::JEV_KEY_ENV);
            return;
        }
        let jev = cfg.jev.clone();
        let shared = std::sync::Arc::new(std::sync::RwLock::new(std::sync::Arc::new(cfg)));
        let resp = handle_test(&serde_json::json!({"request_id": "live"}), &shared).await;
        let p = &resp["payload"];
        println!(
            "jev.test success={} latency_ms={} p50_ms={} suggested_timeout_ms={} error={}",
            p["success"],
            p["data"]["latency_ms"],
            p["data"]["p50_ms"],
            p["data"]["suggested_timeout_ms"],
            p["error"]["code"]
        );
        assert_eq!(p["success"], true);

        let client = JevClient::from_config(&jev).unwrap();
        let mut q = std::collections::BTreeMap::new();
        q.insert(
            "steps".to_string(),
            Question::Score {
                instructions: "How many browser steps will this task take?".into(),
                levels: vec![
                    "1-2 steps".into(),
                    "3-5 steps".into(),
                    "6-10 steps".into(),
                    "more than 10 steps".into(),
                ],
            },
        );
        let r = client
            .ask(
                serde_json::json!({"query": "read a Wikipedia article about Rust"}),
                q,
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        println!("score answer: {:?}", r.answer_for("steps"));
        println!(
            "usage: input={} output={}",
            r.usage.input_tokens, r.usage.output_tokens
        );
    }

    #[test]
    fn suggested_timeout_is_twice_the_median_and_never_below_the_default() {
        assert_eq!(suggested_timeout_ms(260), 800);
        assert_eq!(suggested_timeout_ms(400), 800);
        assert_eq!(suggested_timeout_ms(451), 1000);
        assert_eq!(suggested_timeout_ms(700), 1400);
    }

    #[tokio::test]
    async fn testing_with_jev_disabled_sends_nothing() {
        let cfg = crate::config::AgentConfig::default(); // jev disabled
        let shared = std::sync::Arc::new(std::sync::RwLock::new(std::sync::Arc::new(cfg)));
        let resp = handle_test(&serde_json::json!({"request_id": "r1"}), &shared).await;
        assert_eq!(resp["payload"]["success"], false);
        assert_eq!(resp["payload"]["error"]["code"], "NOT_CONFIGURED");
    }

    #[tokio::test]
    async fn testing_reports_latency_and_a_timeout_suggestion() {
        let (url, bodies) = crate::jev::test_support::answering(
            serde_json::json!({"answers": {"ping": {"noul": 0.5}}, "usage": {"input_tokens": 9, "output_tokens": 1}}),
            std::time::Duration::ZERO,
        )
        .await;
        let mut cfg = crate::config::AgentConfig::default();
        cfg.jev.enabled = true;
        cfg.jev.endpoint = url;
        cfg.jev.api_key = "k".into();
        let shared = std::sync::Arc::new(std::sync::RwLock::new(std::sync::Arc::new(cfg)));
        let resp = handle_test(&serde_json::json!({"request_id": "r2"}), &shared).await;
        assert_eq!(resp["payload"]["success"], true, "{resp}");
        assert_eq!(
            resp["payload"]["data"]["latency_ms"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert!(
            resp["payload"]["data"]["suggested_timeout_ms"]
                .as_u64()
                .unwrap()
                >= 800
        );
        assert_eq!(bodies.lock().unwrap().len(), 3);
    }
}
