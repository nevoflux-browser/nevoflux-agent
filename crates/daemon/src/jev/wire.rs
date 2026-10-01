//! Jev (TypeSafe System One) request/response shapes (S4 spike, spec §5.4).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One question. Questions in a request are independent (computed in parallel).
#[derive(Debug, Clone, PartialEq)]
pub enum Question {
    Noul {
        instructions: String,
        when_true: String,
        when_false: String,
    },
    Choice {
        instructions: String,
        options: BTreeMap<String, String>,
    },
    Score {
        instructions: String,
        levels: Vec<String>,
    },
}

impl Serialize for Question {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let v = match self {
            Question::Noul {
                instructions,
                when_true,
                when_false,
            } => serde_json::json!({
                "type": "noul", "instructions": instructions,
                "criteria": {"true": when_true, "false": when_false}
            }),
            Question::Choice {
                instructions,
                options,
            } => serde_json::json!({
                "type": "choice", "instructions": instructions, "criteria": options
            }),
            Question::Score {
                instructions,
                levels,
            } => serde_json::json!({
                "type": "score", "instructions": instructions, "criteria": levels
            }),
        };
        v.serialize(s)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JevRequest {
    pub state: serde_json::Value,
    pub model: String,
    pub questions: BTreeMap<String, Question>,
}

/// One answer. Shapes the client does not model yet stay as raw JSON.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Noul(f64),
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
    },
    Other(serde_json::Value),
}

impl<'de> Deserialize<'de> for Answer {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        if let Some(p) = v.get("noul").and_then(|p| p.as_f64()) {
            return Ok(Answer::Noul(p));
        }
        if let Some(c) = v.get("choice").and_then(|c| c.as_str()) {
            let probabilities = v
                .get("probabilities")
                .and_then(|p| serde_json::from_value(p.clone()).ok())
                .unwrap_or_default();
            return Ok(Answer::Choice {
                choice: c.to_string(),
                probabilities,
            });
        }
        Ok(Answer::Other(v))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct JevUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JevResponse {
    #[serde(default)]
    pub answers: BTreeMap<String, Answer>,
    #[serde(default)]
    pub usage: JevUsage,
}

impl JevResponse {
    pub fn answer_for(&self, name: &str) -> Option<&Answer> {
        self.answers.get(name)
    }

    pub fn noul(&self, name: &str) -> Option<f64> {
        match self.answers.get(name) {
            Some(Answer::Noul(p)) => Some(*p),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_serialise_to_the_system_one_shape() {
        let mut qs = BTreeMap::new();
        qs.insert(
            "drift".to_string(),
            Question::Noul {
                instructions: "Did the last action drift?".into(),
                when_true: "It drifted".into(),
                when_false: "It did not".into(),
            },
        );
        let req = JevRequest {
            state: serde_json::json!({"q": "x"}),
            model: "jev-latest".into(),
            questions: qs,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["model"], "jev-latest");
        assert_eq!(v["questions"]["drift"]["type"], "noul");
        assert_eq!(v["questions"]["drift"]["criteria"]["true"], "It drifted");
        assert_eq!(v["questions"]["drift"]["criteria"]["false"], "It did not");
    }

    #[test]
    fn responses_parse_noul_choice_and_unknown_shapes() {
        let body = serde_json::json!({
            "answers": {
                "drift": {"noul": 0.12},
                "visibility": {"choice": "short", "probabilities": {"hide": 0.1, "short": 0.7}},
                "steps": {"score": 4, "distribution": [0.1, 0.2]}
            },
            "usage": {"input_tokens": 290, "output_tokens": 20}
        });
        let r: JevResponse = serde_json::from_value(body).unwrap();
        assert_eq!(r.noul("drift"), Some(0.12));
        assert!(
            matches!(r.answer_for("visibility"), Some(Answer::Choice { choice, .. }) if choice == "short")
        );
        assert!(matches!(r.answer_for("steps"), Some(Answer::Other(_))));
        assert_eq!(r.answer_for("missing"), None);
        assert_eq!(r.usage.input_tokens, 290);
    }
}
