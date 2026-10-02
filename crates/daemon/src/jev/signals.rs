//! Jev request ① (spec §5.4): per-step signals asked in parallel with tool
//! execution — remaining steps (H = P25), drift, irrelevant bulk, whether
//! another action is needed. The state carries no page content and no
//! argument values (§5.8).

use std::collections::BTreeMap;

use super::oracle::{DecisionOracle, JevOracle, OracleContext, Verdict};
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
pub fn arg_summary(arguments: &serde_json::Value) -> serde_json::Value {
    let Some(obj) = arguments.as_object() else {
        return serde_json::Value::Null;
    };
    let mut out = serde_json::Map::new();
    for (k, v) in obj {
        let kept = if k == "arguments" && v.is_object() {
            let mut inner = serde_json::Map::new();
            for (ik, iv) in v.as_object().into_iter().flatten() {
                inner.insert(ik.clone(), safe_value(ik, iv));
            }
            serde_json::Value::Object(inner)
        } else {
            safe_value(k, v)
        };
        out.insert(k.clone(), kept);
    }
    serde_json::Value::Object(out)
}

fn safe_value(key: &str, v: &serde_json::Value) -> serde_json::Value {
    if !SAFE_ARG_KEYS.contains(&key) {
        return serde_json::Value::String("…".into());
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
pub fn state(input: &SignalInput) -> serde_json::Value {
    let actions: Vec<serde_json::Value> = input
        .calls
        .iter()
        .map(|c| serde_json::json!({"tool": c.name, "args": arg_summary(c.arguments)}))
        .collect();
    let mut st = serde_json::json!({
        "query": input.query,
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

/// Ask request ①; `None` when Jev fell back (the oracle logged it).
pub async fn ask_signals(
    oracle: &JevOracle,
    ctx: &OracleContext,
    input: &SignalInput<'_>,
) -> Option<StepSignals> {
    match oracle.ask(ctx, state(input), questions()).await {
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
        let s = arg_summary(&a).to_string();
        assert!(s.contains("#password") && s.contains("https://a.example/"));
        assert!(!s.contains("hunter2") && !s.contains("secret note"));
        let dynamic = serde_json::json!({"tool_name": "send_mail", "arguments": {"to": "x@y.z", "body": "private"}});
        let d = arg_summary(&dynamic).to_string();
        assert!(d.contains("send_mail") && !d.contains("private") && !d.contains("x@y.z"));
    }

    #[test]
    fn the_state_has_no_page_content() {
        let args = serde_json::json!({"element_id": "e3"});
        let calls = [StepCall {
            name: "browser_click_by_id",
            arguments: &args,
        }];
        let st = state(&SignalInput {
            query: "buy socks",
            step: 2,
            calls: &calls,
            loaded_tools: &["browser_navigate".into()],
            chunk_stubs: &["[c1 · read · 9000 bytes · short]".into()],
            tab: Some(("https://shop.example/cart", "Cart")),
        });
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
