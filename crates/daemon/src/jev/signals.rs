//! Jev request ① (spec §5.4): per-step signals asked in parallel with tool
//! execution — remaining steps (H = P25), drift, irrelevant bulk, whether
//! another action is needed. The state carries no page content and no
//! argument values (§5.8).

use std::collections::BTreeMap;

use super::oracle::{DecisionOracle, JevOracle, OracleContext, Verdict};
use super::privacy::{scope_for, Scope};
use super::wire::{Answer, JevResponse, Question};

/// Jev's semantic pollution signals count above this (spec §5.7, θ).
pub const DRIFT_THETA: f64 = 0.8;

/// `remaining_steps` levels and the step count each stands for.
pub const STEP_LEVELS: &[(&str, u32)] = &[
    ("1", 1),
    ("2", 2),
    ("3", 3),
    ("4-5", 4),
    ("6-8", 6),
    ("9-12", 9),
    ("13-20", 13),
    ("more than 20", 21),
];

/// Argument keys whose values may go to Jev: locators, never content.
const SAFE_ARG_KEYS: &[&str] = &[
    "url",
    "selector",
    "element_id",
    "tab_id",
    "ref",
    "path",
    "query",
    "tool_name",
];
const ARG_VALUE_CHARS: usize = 120;
/// The query as sent in the state (the request cap is 64k tokens).
const QUERY_CHARS: usize = 1_000;

/// One tool call of the step being signalled.
pub struct StepCall<'a> {
    pub name: &'a str,
    pub arguments: &'a serde_json::Value,
}

/// What request ① is built from.
pub struct SignalInput<'a> {
    pub query: &'a str,
    pub step: u32,
    pub calls: &'a [StepCall<'a>],
    pub loaded_tools: &'a [String],
    /// One stub line per compressed chunk (`[id · tool · bytes · level]`).
    pub chunk_stubs: &'a [String],
    /// The active tab as (url, title); only its domain and title are sent.
    pub tab: Option<(&'a str, &'a str)>,
}

/// The step's answers. `None` where Jev gave no usable answer.
#[derive(Debug, Clone, PartialEq)]
pub struct StepSignals {
    pub step: u32,
    /// H: P25 of the remaining-steps distribution (spec §5.7).
    pub h: Option<u32>,
    pub drift: Option<f64>,
    pub irrelevant_bulk: Option<f64>,
    pub needs_action: Option<f64>,
}

/// Keys of `arguments` with their values blanked, except locator keys (cut to
/// 120 chars). One level into `arguments`, for `tool_call_dynamic`.
pub fn arg_summary(arguments: &serde_json::Value, sensitive: &[String]) -> serde_json::Value {
    let Some(obj) = arguments.as_object() else {
        return serde_json::Value::Null;
    };
    let mut out = serde_json::Map::new();
    for (k, v) in obj {
        let kept = if k == "arguments" && v.is_object() {
            let mut inner = serde_json::Map::new();
            for (ik, iv) in v.as_object().into_iter().flatten() {
                inner.insert(ik.clone(), safe_value(ik, iv, sensitive));
            }
            serde_json::Value::Object(inner)
        } else {
            safe_value(k, v, sensitive)
        };
        out.insert(k.clone(), kept);
    }
    serde_json::Value::Object(out)
}

fn safe_value(key: &str, v: &serde_json::Value, sensitive: &[String]) -> serde_json::Value {
    if !SAFE_ARG_KEYS.contains(&key) {
        return serde_json::Value::String("…".into());
    }
    // A sensitive site's URL is reduced to its host (§5.8): paths and
    // queries can carry account ids or tokens.
    if key == "url" {
        if let Some(u) = v.as_str() {
            if scope_for(u, sensitive) == Scope::MetadataOnly {
                let host = reqwest::Url::parse(u)
                    .ok()
                    .and_then(|p| p.host_str().map(str::to_string))
                    .unwrap_or_else(|| "…".to_string());
                return serde_json::Value::String(host);
            }
        }
    }
    match v {
        serde_json::Value::String(s) => {
            serde_json::Value::String(s.chars().take(ARG_VALUE_CHARS).collect())
        }
        serde_json::Value::Number(_) | serde_json::Value::Bool(_) => v.clone(),
        _ => serde_json::Value::String("…".into()),
    }
}

/// The request ① state.
pub fn state(input: &SignalInput, sensitive: &[String]) -> serde_json::Value {
    let actions: Vec<serde_json::Value> = input
        .calls
        .iter()
        .map(|c| serde_json::json!({"tool": c.name, "args": arg_summary(c.arguments, sensitive)}))
        .collect();
    let mut st = serde_json::json!({
        "query": input.query.chars().take(QUERY_CHARS).collect::<String>(),
        "step": input.step,
        "actions": actions,
        "loaded_tools": input.loaded_tools,
        "compressed_chunks": input.chunk_stubs,
    });
    if let Some((url, title)) = input.tab {
        let domain = reqwest::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        st["tab"] = serde_json::json!({"domain": domain, "title": title});
    }
    st
}

fn noul(instructions: &str, yes: &str, no: &str) -> Question {
    Question::Noul {
        instructions: instructions.into(),
        when_true: yes.into(),
        when_false: no.into(),
    }
}

/// The four request ① questions (spec §5.4).
pub fn questions() -> BTreeMap<String, Question> {
    let mut q = BTreeMap::new();
    q.insert(
        "remaining_steps".to_string(),
        Question::Score {
            instructions: "An AI agent is working on the user's query; the state shows its \
                latest step. How many more tool-using steps will it most likely need to finish?"
                .into(),
            levels: STEP_LEVELS.iter().map(|(l, _)| l.to_string()).collect(),
        },
    );
    q.insert(
        "drift".to_string(),
        noul(
            "Has the agent's most recent action drifted away from the user's original goal?",
            "It drifted",
            "It is on track",
        ),
    );
    q.insert(
        "irrelevant_bulk".to_string(),
        noul(
            "Does the agent's current context hold a lot of material unrelated to its current \
             subgoal that could distract its next decision?",
            "Yes, a lot of unrelated material",
            "No",
        ),
    );
    q.insert(
        "needs_action".to_string(),
        noul(
            "Does the agent still need to take another action (tool call) to complete the \
             user's query?",
            "Yes",
            "No, it can answer now",
        ),
    );
    q
}

/// H from a Score `probabilities` map keyed by level index ("0", "1", …):
/// the step count of the first level where the cumulative probability
/// reaches 0.25. `None` when the map is empty or unreadable.
pub fn p25_steps(probabilities: &serde_json::Value) -> Option<u32> {
    let map = probabilities.as_object()?;
    if map.is_empty() {
        return None;
    }
    let mut cumulative = 0.0;
    for (i, (_, steps)) in STEP_LEVELS.iter().enumerate() {
        cumulative += map
            .get(&i.to_string())
            .and_then(|p| p.as_f64())
            .unwrap_or(0.0);
        if cumulative >= 0.25 {
            return Some(*steps);
        }
    }
    None
}

/// The step's signals from a Jev answer; missing answers stay `None`.
pub fn parse(step: u32, r: &JevResponse) -> StepSignals {
    let h = match r.answer_for("remaining_steps") {
        Some(Answer::Other(v)) => v.get("probabilities").and_then(p25_steps),
        _ => None,
    };
    StepSignals {
        step,
        h,
        drift: r.noul("drift"),
        irrelevant_bulk: r.noul("irrelevant_bulk"),
        needs_action: r.noul("needs_action"),
    }
}

/// What request ① may be asked against, from the browser's live tab list
/// (the turn-start tab goes stale after a navigate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalScope {
    /// The active tab is known and ordinary: (url, title).
    Page(String, String),
    /// A candidate tab is sensitive or unknown: do not ask this step.
    Skip,
    /// No browser: no page in the state.
    NoTab,
}

pub fn signal_scope(tabs: Option<&serde_json::Value>, sensitive: &[String]) -> SignalScope {
    let Some(tabs) = tabs else {
        return SignalScope::NoTab;
    };
    let Some(urls) = super::visibility::candidate_urls(tabs, &serde_json::json!({})) else {
        return SignalScope::Skip;
    };
    if urls.is_empty() || urls.iter().any(|u| scope_for(u, sensitive) != Scope::Full) {
        return SignalScope::Skip;
    }
    let title = tabs
        .get("tabs")
        .and_then(|t| t.as_array())
        .and_then(|list| {
            list.iter()
                .find(|t| t.get("url").and_then(|u| u.as_str()) == Some(urls[0].as_str()))
        })
        .and_then(|t| t.get("title").and_then(|x| x.as_str()))
        .unwrap_or("")
        .to_string();
    SignalScope::Page(urls[0].clone(), title)
}

/// Ask request ①; `None` when Jev fell back (the oracle logged it).
pub async fn ask_signals(
    oracle: &JevOracle,
    ctx: &OracleContext,
    input: &SignalInput<'_>,
    sensitive: &[String],
) -> Option<StepSignals> {
    match oracle.ask(ctx, state(input, sensitive), questions()).await {
        Verdict::Answered(r) => Some(parse(input.step, &r)),
        Verdict::Fallback { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arg_summary_never_carries_values() {
        let a = serde_json::json!({"selector": "#password", "value": "hunter2", "text": "my secret note", "url": "https://a.example/"});
        let s = arg_summary(&a, &[]).to_string();
        assert!(s.contains("#password") && s.contains("https://a.example/"));
        assert!(!s.contains("hunter2") && !s.contains("secret note"));
        let dynamic = serde_json::json!({"tool_name": "send_mail", "arguments": {"to": "x@y.z", "body": "private"}});
        let d = arg_summary(&dynamic, &[]).to_string();
        assert!(d.contains("send_mail") && !d.contains("private") && !d.contains("x@y.z"));
    }

    #[test]
    fn sensitive_urls_are_reduced_to_their_domain() {
        // Review I4: request ① must honour the sensitive-site list too.
        let a = serde_json::json!({"url": "https://www.paypal.com/myaccount/123?token=x"});
        let s = arg_summary(&a, &[]).to_string();
        assert!(s.contains("www.paypal.com"), "{s}");
        assert!(!s.contains("myaccount") && !s.contains("token"), "{s}");
        let user = serde_json::json!({"url": "https://intranet.acme.example/hr/42"});
        let u = arg_summary(&user, &["acme.example".into()]).to_string();
        assert!(!u.contains("/hr/42"), "{u}");
        let ok = serde_json::json!({"url": "https://en.wikipedia.org/wiki/Rust"});
        assert!(arg_summary(&ok, &[]).to_string().contains("/wiki/Rust"));
    }

    #[test]
    fn the_state_caps_the_query() {
        let q = "q".repeat(10_000);
        let st = state(
            &SignalInput {
                query: &q,
                step: 0,
                calls: &[],
                loaded_tools: &[],
                chunk_stubs: &[],
                tab: None,
            },
            &[],
        );
        assert!(st["query"].as_str().unwrap().len() <= 1_000);
    }

    #[test]
    fn the_scope_comes_from_the_live_tabs() {
        let tabs = serde_json::json!({"tabs": [
            {"id": 1, "url": "https://shop.example/cart", "title": "Cart", "active": true}
        ]});
        assert_eq!(
            signal_scope(Some(&tabs), &[]),
            SignalScope::Page("https://shop.example/cart".into(), "Cart".into())
        );
        let bank = serde_json::json!({"tabs": [
            {"id": 1, "url": "https://www.paypal.com/x", "title": "PayPal", "active": true}
        ]});
        assert_eq!(signal_scope(Some(&bank), &[]), SignalScope::Skip);
        assert_eq!(signal_scope(None, &[]), SignalScope::NoTab);
    }

    #[test]
    fn the_state_has_no_page_content() {
        let args = serde_json::json!({"element_id": "e3"});
        let calls = [StepCall {
            name: "browser_click_by_id",
            arguments: &args,
        }];
        let st = state(
            &SignalInput {
                query: "buy socks",
                step: 2,
                calls: &calls,
                loaded_tools: &["browser_navigate".into()],
                chunk_stubs: &["[c1 · read · 9000 bytes · short]".into()],
                tab: Some(("https://shop.example/cart", "Cart")),
            },
            &[],
        );
        assert_eq!(st["tab"]["domain"], "shop.example");
        assert_eq!(st["tab"]["title"], "Cart");
        assert_eq!(st["actions"][0]["tool"], "browser_click_by_id");
        assert_eq!(st["actions"][0]["args"]["element_id"], "e3");
        assert_eq!(st["step"], 2);
        assert_eq!(st["query"], "buy socks");
    }

    #[test]
    fn h_is_the_p25_of_the_distribution() {
        let p = serde_json::json!({"0": 0.1, "1": 0.1, "2": 0.1, "3": 0.4, "4": 0.3});
        assert_eq!(p25_steps(&p), Some(3)); // cumulative 0.3 reached at level "3"
        assert_eq!(p25_steps(&serde_json::json!({"0": 0.9})), Some(1));
        assert_eq!(p25_steps(&serde_json::json!({})), None);
    }

    #[test]
    fn parse_reads_the_live_score_shape_and_tolerates_missing_answers() {
        let body = serde_json::json!({"answers": {
            "remaining_steps": {"type": "score", "score": 0.4, "confidence": 0.5, "probabilities": {"0": 0.05, "1": 0.3, "2": 0.65}, "legend": {}},
            "drift": {"noul": 0.92}
        }, "usage": {}});
        let r: JevResponse = serde_json::from_value(body).unwrap();
        let s = parse(4, &r);
        assert_eq!(
            (s.step, s.h, s.drift, s.irrelevant_bulk),
            (4, Some(2), Some(0.92), None)
        );
    }

    #[test]
    fn questions_are_the_four_of_request_one() {
        let q = questions();
        assert_eq!(
            q.keys().cloned().collect::<Vec<_>>(),
            vec![
                "drift",
                "irrelevant_bulk",
                "needs_action",
                "remaining_steps"
            ]
        );
        assert!(
            matches!(&q["remaining_steps"], Question::Score { levels, .. } if levels.len() == STEP_LEVELS.len())
        );
    }
}
