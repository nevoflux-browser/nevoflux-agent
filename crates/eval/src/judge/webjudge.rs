//! `WebJudge` — LLM-as-judge for browser-based benchmarks (Online-Mind2Web,
//! BrowseComp etc.).
//!
//! Sends the task's prompt + final_answer + evaluation_criteria to an LLM
//! and parses a PASS/FAIL verdict. The LLM endpoint is configured via
//! `WebJudge::with_llm_config` (CLI defaults from the daemon's own config
//! — same provider for free reuse of the mock LLM in CI).
//!
//! Wire shape: simple Anthropic chat completion request. The daemon's mock
//! server supports both Anthropic and OpenAI formats; WebJudge uses Anthropic.

use crate::{
    judge::{Judge, Verdict},
    EvalResult, Task, TaskResult,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub struct WebJudge {
    base_url: String,
    api_key: String,
    model: String,
    http: reqwest::Client,
}

impl WebJudge {
    pub fn new() -> Self {
        // Default to env-configurable endpoint. Real runs override via
        // with_llm_config or set NEVOFLUX_WEBJUDGE_BASE_URL.
        let base_url = std::env::var("NEVOFLUX_WEBJUDGE_BASE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:0".into());
        let api_key = std::env::var("NEVOFLUX_WEBJUDGE_API_KEY").unwrap_or_else(|_| "mock".into());
        let model = std::env::var("NEVOFLUX_WEBJUDGE_MODEL")
            .unwrap_or_else(|_| "claude-3-5-sonnet-latest".into());
        Self {
            base_url,
            api_key,
            model,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
        }
    }

    pub fn with_llm_config(mut self, base_url: String, api_key: String, model: String) -> Self {
        self.base_url = base_url;
        self.api_key = api_key;
        self.model = model;
        self
    }
}

impl Default for WebJudge {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<AnthropicMessage>,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: &'static str,
    content: String,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct AnthropicContent {
    text: String,
}

#[derive(Debug, Deserialize, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

fn build_prompt(task: &Task, final_answer: &str, evaluation_criteria: &str) -> String {
    format!(
        "You are evaluating an AI agent's response to a browser-based task.\n\
         \n\
         ## Task\n\
         {prompt}\n\
         \n\
         ## Evaluation criteria\n\
         {criteria}\n\
         \n\
         ## Agent's final answer\n\
         {answer}\n\
         \n\
         ## Your verdict\n\
         Reply with a single line beginning with either `PASS` or `FAIL`, \
         followed by a one-sentence justification.",
        prompt = task.prompt,
        criteria = evaluation_criteria,
        answer = final_answer,
    )
}

#[async_trait]
impl Judge for WebJudge {
    fn name(&self) -> &str {
        "webjudge"
    }

    async fn judge(&self, task: &Task, result: &TaskResult) -> EvalResult<Verdict> {
        let final_answer = result.final_answer.as_deref().unwrap_or("");
        let evaluation_criteria = task
            .metadata
            .get("evaluation_criteria")
            .and_then(|v| v.as_str())
            .unwrap_or("(no explicit criteria — judge holistically)");

        let prompt = build_prompt(task, final_answer, evaluation_criteria);
        let req = AnthropicRequest {
            model: self.model.clone(),
            max_tokens: 200,
            messages: vec![AnthropicMessage {
                role: "user",
                content: prompt,
            }],
        };

        let resp = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&req)
            .send()
            .await
            .map_err(|e| crate::EvalError::JudgeFailure {
                judge: "webjudge".into(),
                task_id: task.id.clone(),
                reason: format!("transport: {e}"),
            })?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(crate::EvalError::JudgeFailure {
                judge: "webjudge".into(),
                task_id: task.id.clone(),
                reason: format!("HTTP {status}: {body}"),
            });
        }

        let body: AnthropicResponse =
            resp.json()
                .await
                .map_err(|e| crate::EvalError::JudgeFailure {
                    judge: "webjudge".into(),
                    task_id: task.id.clone(),
                    reason: format!("parse: {e}"),
                })?;

        let text = body
            .content
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        // Parse PASS / FAIL verdict.
        let trimmed = text.trim();
        let (correct, score) = if trimmed.to_uppercase().starts_with("PASS") {
            (true, 1.0)
        } else if trimmed.to_uppercase().starts_with("FAIL") {
            (false, 0.0)
        } else if trimmed.contains("Eval mock response") {
            // Mock LLM detected — treat as PASS for smoke purposes only.
            (true, 1.0)
        } else {
            // Unparseable response — neither PASS nor FAIL. Conservatively fail.
            (false, 0.0)
        };

        let judge_cost_usd = body
            .usage
            .as_ref()
            .map(|u| {
                crate::judge::pricing::estimate_cost_usd(
                    &self.model,
                    u.input_tokens,
                    u.output_tokens,
                )
            })
            .unwrap_or(0.0);

        Ok(Verdict {
            correct,
            score,
            explanation: trimmed.to_string(),
            judge_cost_usd,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NevoFluxMode, TaskStatus};
    use axum::{routing::post, Json, Router};
    use tokio::net::TcpListener;

    async fn spawn_test_server(canned_text: &'static str) -> std::net::SocketAddr {
        let app = Router::new().route(
            "/v1/messages",
            post(move || async move {
                Json(serde_json::json!({
                    "content": [{"type": "text", "text": canned_text}],
                    "usage": {"input_tokens": 42, "output_tokens": 17},
                }))
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    fn dummy_task() -> Task {
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "evaluation_criteria".into(),
            serde_json::Value::String("answer mentions cats".into()),
        );
        Task {
            id: "t".into(),
            category: "online-mind2web".into(),
            mode: NevoFluxMode::Browser,
            prompt: "Tell me about cats".into(),
            setup: vec![],
            reference: None,
            assertions: vec![],
            requires_browser: true,
            metadata,
            supports_platform: vec![],
        }
    }

    fn dummy_result(text: &str) -> TaskResult {
        TaskResult {
            task_id: "t".into(),
            status: TaskStatus::Completed,
            final_answer: Some(text.into()),
            latency_ms: 1,
            token_cost: None,
            error: None,
            trace_ids: vec![],
            observed_events: vec![],
            outbound_hosts: vec![],
        }
    }

    #[tokio::test]
    async fn judge_passes_on_PASS_response() {
        let addr = spawn_test_server("PASS — agent correctly mentioned cats.").await;
        let j = WebJudge::new().with_llm_config(
            format!("http://{addr}"),
            "test".into(),
            "test-model".into(),
        );
        let v = j
            .judge(&dummy_task(), &dummy_result("Cats are mammals."))
            .await
            .unwrap();
        assert!(v.correct);
        assert_eq!(v.score, 1.0);
    }

    #[tokio::test]
    async fn judge_fails_on_FAIL_response() {
        let addr = spawn_test_server("FAIL — answer did not mention cats.").await;
        let j = WebJudge::new().with_llm_config(
            format!("http://{addr}"),
            "test".into(),
            "test-model".into(),
        );
        let v = j
            .judge(&dummy_task(), &dummy_result("I like dogs."))
            .await
            .unwrap();
        assert!(!v.correct);
        assert_eq!(v.score, 0.0);
    }

    #[tokio::test]
    async fn judge_treats_mock_response_as_PASS() {
        // The daemon's mock server returns this exact string.
        let addr = spawn_test_server("Eval mock response.").await;
        let j =
            WebJudge::new().with_llm_config(format!("http://{addr}"), "mock".into(), "mock".into());
        let v = j
            .judge(&dummy_task(), &dummy_result("anything"))
            .await
            .unwrap();
        assert!(v.correct);
    }

    #[tokio::test]
    async fn judge_cost_usd_reflects_usage_for_known_model() {
        let addr = spawn_test_server("PASS — yes.").await;
        let j = WebJudge::new().with_llm_config(
            format!("http://{addr}"),
            "test".into(),
            "claude-3-5-sonnet-20240620".into(),
        );
        let v = j
            .judge(&dummy_task(), &dummy_result("Cats are mammals."))
            .await
            .unwrap();
        assert!(v.correct);
        // 42 input * $0.003/1k + 17 output * $0.015/1k = 0.000126 + 0.000255 = 0.000381
        assert!(
            (v.judge_cost_usd - 0.000381).abs() < 1e-9,
            "got {}",
            v.judge_cost_usd
        );
    }

    #[tokio::test]
    async fn judge_cost_usd_zero_for_unknown_model() {
        let addr = spawn_test_server("PASS — yes.").await;
        let j = WebJudge::new().with_llm_config(
            format!("http://{addr}"),
            "test".into(),
            "totally-unknown-model".into(),
        );
        let v = j.judge(&dummy_task(), &dummy_result("any")).await.unwrap();
        assert_eq!(v.judge_cost_usd, 0.0);
    }
}
