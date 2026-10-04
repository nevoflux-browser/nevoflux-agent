//! Tool assembly (spec §5.5): which of the mode's tools to offer. Pure: the
//! catalog, Jev's answers and the current set are the inputs.

use std::collections::BTreeMap;

use nevoflux_builtin_wasm::{Message, ToolDefinition};

use super::wire::{Answer, JevResponse, Question};

/// Always offered when Jev picks the tools (spec §5.5).
pub const CORE_TOOLS: &[&str] = &[
    "browser_navigate",
    "browser_get_markdown",
    "browser_get_tabs",
];
/// Never candidates: offered whenever a set is (`recall`, `act`).
pub const NOT_CANDIDATES: &[&str] = &["recall", "act"];
/// A warm turn changes its set only for a new tool this likely (§5.5).
pub const LOAD_P: f64 = 0.7;
/// Nouls per Jev request: ~100 tokens each keeps a request far under 64k.
pub const MAX_NOULS_PER_REQUEST: usize = 100;
/// Characters of a tool's one-line description.
const SHORT_CHARS: usize = 160;

/// A tool's one-line description: its first sentence, at most 160 chars.
pub fn short_description(description: &str) -> String {
    description
        .split('.')
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(SHORT_CHARS)
        .collect()
}

/// The tools Jev chooses among, as (name, one-line description), in the
/// catalog's order.
pub fn candidates(catalog: &[ToolDefinition]) -> Vec<(String, String)> {
    catalog
        .iter()
        .filter(|t| !CORE_TOOLS.contains(&t.name.as_str()))
        .filter(|t| !NOT_CANDIDATES.contains(&t.name.as_str()))
        .map(|t| (t.name.clone(), short_description(&t.description)))
        .collect()
}

/// One Noul per candidate, keyed by its name (spec §5.4: tier-1 Nouls).
pub fn questions(candidates: &[(String, String)]) -> BTreeMap<String, Question> {
    candidates
        .iter()
        .map(|(name, short)| {
            (
                name.clone(),
                Question::Noul {
                    instructions: format!(
                        "Will completing the user's request likely need the tool `{name}` ({short})?"
                    ),
                    when_true: "The request will likely need this tool.".into(),
                    when_false: "The request will not need this tool.".into(),
                },
            )
        })
        .collect()
}

/// Every Noul answer across the (possibly split) requests.
pub fn probabilities(answers: &[JevResponse]) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for r in answers {
        for name in r.answers.keys() {
            if let Some(p) = r.noul(name) {
                out.insert(name.clone(), p);
            }
        }
    }
    out
}

/// Share of the candidates Jev must answer for its answer to count; less
/// is treated as a fallback (spec §5.8), never as "no tool is likely".
pub const MIN_ANSWERED: f64 = 0.9;

/// `p` restricted to the candidates asked, or `None` when Jev answered
/// fewer than [`MIN_ANSWERED`] of them.
pub fn asked(
    mut p: BTreeMap<String, f64>,
    candidates: &[(String, String)],
) -> Option<BTreeMap<String, f64>> {
    p.retain(|name, _| candidates.iter().any(|(c, _)| c == name));
    let need = (candidates.len() as f64 * MIN_ANSWERED).ceil() as usize;
    (p.len() >= need).then_some(p)
}

/// The `k` most likely tools: probability descending, then name.
pub fn top_k(p: &BTreeMap<String, f64>, k: usize) -> Vec<String> {
    let mut all: Vec<(&String, f64)> = p.iter().map(|(n, p)| (n, *p)).collect();
    all.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    all.into_iter().take(k).map(|(n, _)| n.clone()).collect()
}

/// `names` with the core tools, sorted and without repeats.
pub fn with_core(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = names
        .into_iter()
        .chain(CORE_TOOLS.iter().map(|s| s.to_string()))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// A tool-set decision and how it differs from the previous set.
#[derive(Debug, Clone, PartialEq)]
pub struct SetDecision {
    pub names: Vec<String>,
    /// `initial`, `ttl_expired`, `tool_change` or `kept`.
    pub reason: &'static str,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

/// Spec §5.5: no set → choose; an expired cache → re-choose for free; a
/// warm one → change only for an unloaded top-K tool with p ≥ [`LOAD_P`].
pub fn decide_set(
    current: Option<&[String]>,
    p: &BTreeMap<String, f64>,
    k: usize,
    warm: bool,
) -> SetDecision {
    let top = top_k(p, k);
    let fresh = with_core(top.iter().cloned());
    let Some(cur) = current else {
        return SetDecision {
            names: fresh,
            reason: "initial",
            added: Vec::new(),
            removed: Vec::new(),
        };
    };
    let (names, reason) = if !warm {
        (fresh, "ttl_expired")
    } else if top
        .iter()
        .any(|n| !cur.contains(n) && p.get(n).is_some_and(|&x| x >= LOAD_P))
    {
        (fresh, "tool_change")
    } else {
        (cur.to_vec(), "kept")
    };
    let mut added: Vec<String> = names.iter().filter(|n| !cur.contains(n)).cloned().collect();
    let mut removed: Vec<String> = cur.iter().filter(|n| !names.contains(n)).cloned().collect();
    added.sort();
    removed.sort();
    SetDecision {
        names,
        reason,
        added,
        removed,
    }
}

/// `act` takes Jev's choice only this confident (spec §5.5 fallback).
pub const CHOOSE_P: f64 = 0.5;

/// `act(intent)`'s question: which candidate fits the intent.
pub fn choice_question(candidates: &[(String, String)]) -> Question {
    Question::Choice {
        instructions: "Which tool fits the intent in the state, given the user's request?".into(),
        options: candidates.iter().cloned().collect(),
    }
}

/// The chosen tool and its probability, when at least [`CHOOSE_P`].
pub fn chosen(r: &JevResponse) -> Option<(String, f64)> {
    r.answers.values().find_map(|a| match a {
        Answer::Choice {
            choice,
            probabilities,
        } => {
            let p = probabilities.get(choice).copied().unwrap_or(0.0);
            (p >= CHOOSE_P).then(|| (choice.clone(), p))
        }
        _ => None,
    })
}

/// Tools with a native call in `history`: they cannot be unloaded while the
/// pair is there (v1.4 §4.6).
pub fn pinned(history: &[Message]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in history {
        for c in &m.tool_calls {
            if !out.contains(&c.name) {
                out.push(c.name.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nevoflux_builtin_wasm::{Message, ToolCall, ToolDefinition};
    use serde_json::json;

    fn def(name: &str, desc: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: desc.into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn probs(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs.iter().map(|(n, p)| (n.to_string(), *p)).collect()
    }
    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_short_description_is_the_first_sentence() {
        assert_eq!(
            short_description("Search the web. Returns links."),
            "Search the web"
        );
        assert_eq!(short_description(&"x".repeat(400)).chars().count(), 160);
        assert_eq!(short_description("No period here"), "No period here");
    }

    #[test]
    fn core_recall_and_act_are_not_candidates() {
        let c = candidates(&[
            def("browser_navigate", "Go."),
            def("web_search", "Search."),
            def("recall", "Recall."),
            def("act", "Act."),
            def("think", "Think."),
        ]);
        let n: Vec<&str> = c.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(n, vec!["web_search", "think"]);
    }

    #[test]
    fn each_candidate_gets_one_noul_naming_it() {
        let q = questions(&[("web_search".into(), "Search the web".into())]);
        assert_eq!(q.len(), 1);
        let v = serde_json::to_value(&q["web_search"]).unwrap();
        assert_eq!(v["type"], "noul");
        assert!(v["instructions"].as_str().unwrap().contains("web_search"));
        assert!(v["instructions"]
            .as_str()
            .unwrap()
            .contains("Search the web"));
    }

    #[test]
    fn top_k_orders_by_probability_then_name() {
        let p = probs(&[("b", 0.9), ("a", 0.9), ("c", 0.2), ("d", 0.5)]);
        assert_eq!(top_k(&p, 3), names(&["a", "b", "d"]));
    }

    #[test]
    fn the_first_set_is_core_plus_top_k() {
        let d = decide_set(
            None,
            &probs(&[("web_search", 0.9), ("think", 0.1)]),
            1,
            true,
        );
        assert_eq!(d.reason, "initial");
        assert_eq!(d.names, with_core(names(&["web_search"])));
        assert!(d.added.is_empty() && d.removed.is_empty());
    }

    #[test]
    fn a_warm_turn_keeps_the_set_unless_a_strong_new_tool_appears() {
        let cur = with_core(names(&["think"]));
        let weak = decide_set(
            Some(&cur),
            &probs(&[("web_search", 0.6), ("think", 0.5)]),
            1,
            true,
        );
        assert_eq!(weak.reason, "kept");
        assert_eq!(weak.names, cur);
        let strong = decide_set(
            Some(&cur),
            &probs(&[("web_search", 0.95), ("think", 0.5)]),
            1,
            true,
        );
        assert_eq!(strong.reason, "tool_change");
        assert_eq!(strong.names, with_core(names(&["web_search"])));
        assert_eq!(strong.added, names(&["web_search"]));
        assert_eq!(strong.removed, names(&["think"]));
    }

    #[test]
    fn a_cold_turn_reselects_for_free() {
        let cur = with_core(names(&["think"]));
        let d = decide_set(
            Some(&cur),
            &probs(&[("web_search", 0.6), ("think", 0.5)]),
            1,
            false,
        );
        assert_eq!(d.reason, "ttl_expired");
        assert_eq!(d.names, with_core(names(&["web_search"])));
    }

    #[test]
    fn pinned_names_the_native_calls_in_history() {
        let h = vec![
            Message::user("q"),
            Message::assistant_with_tool_calls_and_reasoning(
                String::new(),
                vec![ToolCall {
                    id: "t1".into(),
                    call_id: None,
                    name: "read".into(),
                    arguments: json!({}),
                    signature: None,
                }],
                None,
            ),
            Message::tool("t1".to_string(), "ok".to_string()),
        ];
        assert_eq!(pinned(&h), names(&["read"]));
    }

    #[test]
    fn probabilities_merge_split_requests() {
        let a: JevResponse = serde_json::from_value(json!({"answers": {"x": {"noul": 0.4}}, "usage": {"input_tokens": 1, "output_tokens": 1}})).unwrap();
        let b: JevResponse = serde_json::from_value(json!({"answers": {"y": {"noul": 0.8}}, "usage": {"input_tokens": 1, "output_tokens": 1}})).unwrap();
        assert_eq!(probabilities(&[a, b]), probs(&[("x", 0.4), ("y", 0.8)]));
    }

    #[test]
    fn chosen_takes_a_confident_choice_only() {
        let r = |p: f64| -> JevResponse {
            serde_json::from_value(json!({"answers": {"tool": {"choice": "think", "probabilities": {"think": p, "read": 1.0 - p}}},
                "usage": {"input_tokens": 1, "output_tokens": 1}})).unwrap()
        };
        assert_eq!(chosen(&r(0.6)).map(|(n, _)| n), Some("think".to_string()));
        assert_eq!(chosen(&r(0.4)), None);
    }

    #[test]
    fn the_choice_question_offers_each_candidate() {
        let q = choice_question(&[("web_search".into(), "Search the web".into())]);
        let v = serde_json::to_value(&q).unwrap();
        assert_eq!(v["type"], "choice");
        assert_eq!(v["criteria"]["web_search"], "Search the web");
    }

    #[test]
    fn only_asked_candidates_count_and_most_must_be_answered() {
        let cands: Vec<(String, String)> = (0..10)
            .map(|i| (format!("t{i}"), "d".to_string()))
            .collect();
        let full: BTreeMap<String, f64> = (0..10).map(|i| (format!("t{i}"), 0.5)).collect();
        let mut extra = full.clone();
        extra.insert("hallucinated".into(), 0.99);
        let kept = asked(extra, &cands).expect("all answered");
        assert!(!kept.contains_key("hallucinated"));
        let mut part = full.clone();
        part.remove("t0");
        part.remove("t1");
        assert_eq!(asked(part, &cands), None, "8 of 10 answered is a fallback");
        assert_eq!(asked(BTreeMap::new(), &cands), None);
    }
}
