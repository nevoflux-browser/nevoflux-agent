//! In-turn context pollution (spec §5.7): code signals over the action log,
//! plus Jev's drift / irrelevant-bulk signals when they have landed. A
//! polluted turn gets a correction note at the tail — never a rebuild, and
//! page text is never inspected (injection is not a trigger).

use crate::host::StepSignalsView;

/// Actions looked at for repeated calls.
const REPEAT_WINDOW: usize = 6;
/// Failed attempts listed in a correction.
const MAX_ENTRIES: usize = 10;
const ARGS_SHOWN: usize = 80;

/// One executed tool call, as the pollution check sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionRecord {
    /// 0-based step it ran in.
    pub step: u32,
    pub tool: String,
    /// Canonical JSON of the arguments (identical calls compare equal).
    pub args_key: String,
    pub ok: bool,
    /// Normalised error kind when the call failed.
    pub error_class: Option<String>,
}

/// A polluted turn: why, and what to list.
#[derive(Debug, Clone, PartialEq)]
pub struct Pollution {
    /// `repeat_call`, `error_streak`, `bad_args`, `drift`, `irrelevant_bulk`.
    pub triggers: Vec<&'static str>,
    /// Failed or repeated attempts, newest first, at most [`MAX_ENTRIES`].
    pub entries: Vec<String>,
    /// The last action that worked, as (tool, step).
    pub last_ok: Option<(String, u32)>,
}

/// Phrases that mark a malformed call rather than a failed one.
const BAD_ARGS: &[&str] = &[
    "missing required",
    "missing field",
    "invalid argument",
    "invalid param",
    "invalid json",
    "failed to parse argument",
    "unknown field",
    "expected value",
];

fn normalise(s: &str) -> String {
    let lower: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_digit() { '#' } else { c })
        .collect();
    let mut out = String::new();
    let mut last_hash = false;
    for c in lower.chars() {
        if c == '#' {
            if !last_hash {
                out.push('#');
            }
            last_hash = true;
        } else {
            out.push(c);
            last_hash = false;
        }
    }
    out.trim().chars().take(60).collect()
}

/// The error kind of a tool result, or `None` when it did not fail:
/// `{"success":false,"error":…}` JSON, or text starting with `Error`.
/// Malformed-call errors are prefixed `bad_args: `.
pub fn error_class(content: &str) -> Option<String> {
    let trimmed = content.trim_start();
    let message = if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        let success = v.get("success").and_then(|s| s.as_bool());
        let error = v.get("error").filter(|e| !e.is_null());
        if success == Some(false) || (success.is_none() && error.is_some()) {
            match error {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(e) => e
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| e.to_string()),
                None => "failed".to_string(),
            }
        } else {
            return None;
        }
    } else {
        let lower = trimmed.to_lowercase();
        let marked = lower.starts_with("error")
            || lower.starts_with("failed")
            || BAD_ARGS.iter().any(|p| lower.starts_with(p));
        if !marked {
            return None;
        }
        trimmed.lines().next().unwrap_or("").to_string()
    };
    let class = normalise(
        message
            .trim_start_matches("Error:")
            .trim_start_matches("error:"),
    );
    let lower = message.to_lowercase();
    if BAD_ARGS.iter().any(|p| lower.contains(p)) {
        Some(format!("bad_args: {class}"))
    } else {
        Some(class)
    }
}

/// Code signals over `log` plus Jev's (when they have landed) above `theta`.
pub fn detect(
    log: &[ActionRecord],
    jev: Option<&StepSignalsView>,
    theta: f64,
) -> Option<Pollution> {
    let mut triggers = Vec::new();
    let recent = &log[log.len().saturating_sub(REPEAT_WINDOW)..];
    let repeated: Vec<&ActionRecord> = recent
        .iter()
        .enumerate()
        .filter(|(i, a)| {
            recent[..*i]
                .iter()
                .any(|b| b.tool == a.tool && b.args_key == a.args_key)
        })
        .map(|(_, a)| a)
        .collect();
    if !repeated.is_empty() {
        triggers.push("repeat_call");
    }
    if log.len() >= 3 {
        let last3 = &log[log.len() - 3..];
        let class = last3[0].error_class.as_ref();
        if class.is_some()
            && last3
                .iter()
                .all(|a| !a.ok && a.error_class.as_ref() == class)
        {
            triggers.push("error_streak");
        }
    }
    if log.len() >= 2
        && log[log.len() - 2..].iter().all(|a| {
            !a.ok
                && a.error_class
                    .as_deref()
                    .is_some_and(|c| c.starts_with("bad_args"))
        })
    {
        triggers.push("bad_args");
    }
    if let Some(s) = jev {
        if s.drift.is_some_and(|p| p > theta) {
            triggers.push("drift");
        }
        if s.irrelevant_bulk.is_some_and(|p| p > theta) {
            triggers.push("irrelevant_bulk");
        }
    }
    if triggers.is_empty() {
        return None;
    }
    let shown = |a: &ActionRecord| -> String {
        let args: String = a.args_key.chars().take(ARGS_SHOWN).collect();
        match &a.error_class {
            Some(c) if !a.ok => format!("{}({args}) → {c}", a.tool),
            _ => format!("{}({args}) → repeated", a.tool),
        }
    };
    let mut entries: Vec<String> = Vec::new();
    for a in log.iter().rev() {
        let listed = !a.ok
            || repeated
                .iter()
                .any(|r| r.tool == a.tool && r.args_key == a.args_key);
        if listed {
            let e = shown(a);
            if !entries.contains(&e) {
                entries.push(e);
            }
        }
        if entries.len() == MAX_ENTRIES {
            break;
        }
    }
    let last_ok = log
        .iter()
        .rev()
        .find(|a| {
            a.ok && !repeated
                .iter()
                .any(|r| r.tool == a.tool && r.args_key == a.args_key)
        })
        .map(|a| (a.tool.clone(), a.step));
    Some(Pollution {
        triggers,
        entries,
        last_ok,
    })
}

/// The note appended at the tail of the context (spec §5.7: what failed,
/// the current goal, the last state that worked). Built by code only.
pub fn correction_text(query: &str, p: &Pollution) -> String {
    let mut out = String::from(
        "[Correction] This turn is going in circles. Do not retry these — they failed or \
         repeated:",
    );
    for e in &p.entries {
        out.push_str("\n- ");
        out.push_str(e);
    }
    out.push_str("\nCurrent goal: ");
    out.push_str(query);
    if let Some((tool, step)) = &p.last_ok {
        out.push_str(&format!("\nLast action that worked: {tool} (step {step})"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::StepSignalsView;

    fn rec(step: u32, tool: &str, args: &str, ok: bool, class: Option<&str>) -> ActionRecord {
        ActionRecord {
            step,
            tool: tool.into(),
            args_key: args.into(),
            ok,
            error_class: class.map(str::to_string),
        }
    }

    #[test]
    fn two_identical_calls_trigger_repeat_call() {
        let log = [
            rec(0, "read", "{\"p\":1}", true, None),
            rec(1, "read", "{\"p\":1}", true, None),
        ];
        let p = detect(&log, None, 0.8).expect("polluted");
        assert_eq!(p.triggers, vec!["repeat_call"]);
        assert!(p.entries[0].contains("read"));
    }

    #[test]
    fn three_same_class_errors_trigger_error_streak() {
        let log = [
            rec(0, "click", "a", false, Some("element not found")),
            rec(1, "click", "b", false, Some("element not found")),
            rec(2, "click", "c", false, Some("element not found")),
        ];
        assert_eq!(
            detect(&log, None, 0.8).unwrap().triggers,
            vec!["error_streak"]
        );
        let two = &log[1..];
        assert!(detect(two, None, 0.8).is_none());
    }

    #[test]
    fn two_bad_args_trigger_bad_args() {
        let log = [
            rec(0, "fill", "a", false, Some("bad_args: missing field value")),
            rec(1, "type", "b", false, Some("bad_args: invalid json")),
        ];
        assert_eq!(detect(&log, None, 0.8).unwrap().triggers, vec!["bad_args"]);
    }

    #[test]
    fn drift_over_theta_triggers_and_under_does_not() {
        let log = [rec(0, "read", "a", true, None)];
        let high = StepSignalsView {
            step: 0,
            drift: Some(0.85),
            irrelevant_bulk: Some(0.81),
        };
        assert_eq!(
            detect(&log, Some(&high), 0.8).unwrap().triggers,
            vec!["drift", "irrelevant_bulk"]
        );
        let low = StepSignalsView {
            step: 0,
            drift: Some(0.8),
            irrelevant_bulk: None,
        };
        assert!(detect(&log, Some(&low), 0.8).is_none());
    }

    #[test]
    fn no_triggers_no_pollution() {
        let log = [
            rec(0, "read", "a", true, None),
            rec(1, "grep", "b", false, Some("no match")),
        ];
        assert!(detect(&log, None, 0.8).is_none());
        assert!(detect(&[], None, 0.8).is_none());
    }

    #[test]
    fn correction_lists_at_most_ten_entries_and_the_last_good_action() {
        let mut log: Vec<ActionRecord> = (0..14)
            .map(|i| {
                rec(
                    i,
                    "click",
                    &format!("e{i}"),
                    false,
                    Some("element not found"),
                )
            })
            .collect();
        log.insert(0, rec(0, "navigate", "u", true, None));
        let p = detect(&log, None, 0.8).unwrap();
        let text = correction_text("buy socks", &p);
        assert!(text.starts_with("[Correction]"));
        assert_eq!(text.lines().filter(|l| l.starts_with("- ")).count(), 10);
        assert!(text.contains("Current goal: buy socks"));
        assert!(text.contains("Last action that worked: navigate (step 0)"));
    }

    #[test]
    fn error_class_reads_tool_errors_and_flags_bad_arguments() {
        assert_eq!(error_class("File content"), None);
        assert_eq!(
            error_class(r#"{"success":false,"error":"Element 12 not found"}"#).as_deref(),
            Some("element # not found")
        );
        assert!(error_class("Error: missing required parameter `url`")
            .unwrap()
            .starts_with("bad_args"));
        assert!(error_class("Invalid JSON in tool arguments")
            .unwrap()
            .starts_with("bad_args"));
    }
}
