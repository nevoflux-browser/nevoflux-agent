//! Skill choice (spec §5.7, J15): one Noul per skill, asked at turn start
//! with the tool Nouls; the likeliest skill at or above the threshold, at
//! most one a turn. Pure.

use std::collections::BTreeMap;

use nevoflux_builtin_wasm::{Message, SkillSummary};

use super::tools::short_description;
use super::wire::Question;

/// Skill Nouls share the turn-start request with tool Nouls; their keys
/// carry this prefix so a skill and a tool may share a name.
pub const KEY_PREFIX: &str = "skill/";

/// The skills Jev chooses among, as (name, one-line description).
pub fn candidates(skills: &[SkillSummary]) -> Vec<(String, String)> {
    skills
        .iter()
        .map(|s| (s.name.clone(), short_description(&s.description)))
        .collect()
}

/// One Noul per skill, keyed `skill/<name>`.
pub fn questions(candidates: &[(String, String)]) -> BTreeMap<String, Question> {
    candidates
        .iter()
        .map(|(name, short)| {
            (
                format!("{KEY_PREFIX}{name}"),
                Question::Noul {
                    instructions: format!(
                        "Does completing the user's request need the skill `{name}` ({short})? A skill is a set of instructions for one kind of task."
                    ),
                    when_true: "The request needs this skill.".into(),
                    when_false: "The request does not need this skill.".into(),
                },
            )
        })
        .collect()
}

/// Answers split into (tools, skills); skill keys lose their prefix.
pub fn split(p: &BTreeMap<String, f64>) -> (BTreeMap<String, f64>, BTreeMap<String, f64>) {
    let mut tools = BTreeMap::new();
    let mut skills = BTreeMap::new();
    for (k, v) in p {
        match k.strip_prefix(KEY_PREFIX) {
            Some(name) => skills.insert(name.to_string(), *v),
            None => tools.insert(k.clone(), *v),
        };
    }
    (tools, skills)
}

/// Skills whose `skill_load` call is in `history`.
pub fn loaded_in(history: &[Message]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in history {
        for c in &m.tool_calls {
            if c.name != "skill_load" {
                continue;
            }
            if let Some(name) = c.arguments["name"].as_str() {
                if !out.iter().any(|n| n == name) {
                    out.push(name.to_string());
                }
            }
        }
    }
    out
}

/// The likeliest skill at or above `threshold` that is not loaded already
/// (ties by name): at most one a turn (spec §5.7).
pub fn choose_skill(
    p: &BTreeMap<String, f64>,
    threshold: f64,
    loaded: &[String],
) -> Option<(String, f64)> {
    p.iter()
        .filter(|(name, &v)| v >= threshold && !loaded.contains(name))
        .max_by(|a, b| a.1.total_cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(n, v)| (n.clone(), *v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nevoflux_builtin_wasm::{Message, SkillSummary, ToolCall};
    use serde_json::json;

    fn skill(name: &str, desc: &str) -> SkillSummary {
        SkillSummary {
            name: name.into(),
            description: desc.into(),
            tags: vec![],
        }
    }
    fn probs(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs.iter().map(|(n, p)| (n.to_string(), *p)).collect()
    }

    #[test]
    fn each_skill_gets_one_prefixed_noul() {
        let q = questions(&candidates(&[skill(
            "research",
            "Deep research. Many sources.",
        )]));
        assert_eq!(q.len(), 1);
        let v = serde_json::to_value(&q["skill/research"]).unwrap();
        assert_eq!(v["type"], "noul");
        assert!(v["instructions"].as_str().unwrap().contains("research"));
        assert!(v["instructions"]
            .as_str()
            .unwrap()
            .contains("Deep research"));
    }

    #[test]
    fn answers_split_into_tools_and_skills() {
        let (t, s) = split(&probs(&[("web_search", 0.9), ("skill/research", 0.85)]));
        assert_eq!(t, probs(&[("web_search", 0.9)]));
        assert_eq!(s, probs(&[("research", 0.85)]));
    }

    #[test]
    fn the_skill_must_clear_the_threshold() {
        assert_eq!(choose_skill(&probs(&[("research", 0.79)]), 0.8, &[]), None);
        assert_eq!(
            choose_skill(&probs(&[("research", 0.8)]), 0.8, &[]).map(|(n, _)| n),
            Some("research".to_string())
        );
    }

    #[test]
    fn at_most_one_skill_the_likeliest_and_not_one_already_loaded() {
        let p = probs(&[("a", 0.85), ("b", 0.95), ("c", 0.95)]);
        assert_eq!(
            choose_skill(&p, 0.8, &[]).map(|(n, _)| n),
            Some("b".to_string())
        );
        assert_eq!(
            choose_skill(&p, 0.8, &["b".into(), "c".into()]).map(|(n, _)| n),
            Some("a".to_string())
        );
    }

    #[test]
    fn loaded_in_reads_skill_load_calls() {
        let h = vec![
            Message::user("q"),
            Message::assistant_with_tool_calls_and_reasoning(
                String::new(),
                vec![ToolCall {
                    id: "t1".into(),
                    call_id: None,
                    name: "skill_load".into(),
                    arguments: json!({"name": "research"}),
                    signature: None,
                }],
                None,
            ),
            Message::tool("t1".to_string(), "body".to_string()),
        ];
        assert_eq!(loaded_in(&h), vec!["research".to_string()]);
    }
}
