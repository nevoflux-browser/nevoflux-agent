//! Programmatic goal check: a pure assertion over recent tool results, so a
//! machine-verifiable goal (`cargo test` exit 0, `canvas_eval` == "15") needs
//! no evaluator model at all. See spec §4.3.

use serde::{Deserialize, Serialize};

/// A deterministic assertion over recent tool results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalCheck {
    /// Only consider results from this tool; `None` = any tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Substring, or `/regex/` when wrapped in slashes.
    pub matches: String,
    /// Require the pattern to be ABSENT instead of present.
    #[serde(default)]
    pub negate: bool,
}

#[derive(Deserialize)]
struct RawCheck {
    tool: Option<String>,
    matches: Option<String>,
    #[serde(default)]
    negate: bool,
}

/// Parse the optional `check` object from goal_set arguments. Returns
/// `Ok(None)` when absent/null, `Err` when malformed or the regex is invalid.
pub fn parse_check(args: &serde_json::Value) -> Result<Option<GoalCheck>, String> {
    let Some(raw) = args.get("check") else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let rc: RawCheck =
        serde_json::from_value(raw.clone()).map_err(|e| format!("invalid check: {e}"))?;
    let matches = rc
        .matches
        .filter(|m| !m.is_empty())
        .ok_or_else(|| "check.matches is required and non-empty".to_string())?;
    // Validate the regex up-front so a bad pattern fails at set time.
    if let Some(pat) = as_regex(&matches) {
        regex::Regex::new(pat).map_err(|e| format!("invalid check regex: {e}"))?;
    }
    Ok(Some(GoalCheck {
        tool: rc.tool,
        matches,
        negate: rc.negate,
    }))
}

/// `Some(inner)` when `s` is `/inner/` (regex form), else `None` (substring).
fn as_regex(s: &str) -> Option<&str> {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('/') && t.ends_with('/') {
        Some(&t[1..t.len() - 1])
    } else {
        None
    }
}

fn one_hit(pattern: &str, content: &str) -> bool {
    match as_regex(pattern) {
        Some(inner) => regex::Regex::new(inner)
            .map(|r| r.is_match(content))
            .unwrap_or(false),
        None => content.contains(pattern),
    }
}

/// Evaluate the check against recent `(tool_name, content)` results (newest
/// first). Returns whether the goal's programmatic condition holds.
pub fn eval_check(check: &GoalCheck, tool_results: &[(String, String)]) -> bool {
    let present = tool_results
        .iter()
        .filter(|(name, _)| check.tool.as_deref().is_none_or(|t| t == name))
        .any(|(_, content)| one_hit(&check.matches, content));
    present ^ check.negate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tr(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(t, c)| (t.to_string(), c.to_string()))
            .collect()
    }

    #[test]
    fn substring_match_any_tool() {
        let c = GoalCheck {
            tool: None,
            matches: "15".into(),
            negate: false,
        };
        assert!(eval_check(&c, &tr(&[("canvas_eval", "display=15")])));
        assert!(!eval_check(&c, &tr(&[("canvas_eval", "display=20")])));
    }

    #[test]
    fn tool_filter_restricts_scope() {
        let c = GoalCheck {
            tool: Some("canvas_eval".into()),
            matches: "15".into(),
            negate: false,
        };
        // "15" only appears in a different tool's result → no match.
        assert!(!eval_check(
            &c,
            &tr(&[("bash", "15 files"), ("canvas_eval", "20")])
        ));
        assert!(eval_check(&c, &tr(&[("canvas_eval", "15")])));
    }

    #[test]
    fn regex_match() {
        let c = GoalCheck {
            tool: None,
            matches: r"/exit code: 0\b/".into(),
            negate: false,
        };
        assert!(eval_check(&c, &tr(&[("bash", "exit code: 0 done")])));
        assert!(!eval_check(&c, &tr(&[("bash", "exit code: 1")])));
    }

    #[test]
    fn negate_inverts() {
        let c = GoalCheck {
            tool: None,
            matches: "error".into(),
            negate: true,
        };
        assert!(eval_check(&c, &tr(&[("bash", "all good")])));
        assert!(!eval_check(&c, &tr(&[("bash", "error: boom")])));
    }

    #[test]
    fn empty_results_never_match_unless_negate() {
        let pos = GoalCheck {
            tool: None,
            matches: "x".into(),
            negate: false,
        };
        assert!(!eval_check(&pos, &[]));
        let neg = GoalCheck {
            tool: None,
            matches: "x".into(),
            negate: true,
        };
        assert!(eval_check(&neg, &[])); // "no x present" is true
    }

    #[test]
    fn parse_check_shapes() {
        let v = serde_json::json!({"check": {"tool": "canvas_eval", "matches": "15"}});
        let c = parse_check(&v).unwrap().unwrap();
        assert_eq!(c.tool.as_deref(), Some("canvas_eval"));
        assert_eq!(c.matches, "15");
        assert!(!c.negate);
        assert!(parse_check(&serde_json::json!({})).unwrap().is_none());
        assert!(parse_check(&serde_json::json!({"check": {"tool": "x"}})).is_err());
        // matches required
    }
}
