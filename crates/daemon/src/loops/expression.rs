//! Trigger expression grammar (spec §5.1).

use std::time::Duration;

/// Public parse entry. Tab-existence validation happens later (Phase 4) once
/// the registry has a TabRegistry handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerExpr {
    Time(Duration),
    TimeDynamic,
    Event(String),
    State { tab: TabRef, selector: String },
    And(Vec<TriggerExpr>),
    Or(Vec<TriggerExpr>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabRef {
    Current,
    Id(u64),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("empty expression")]
    Empty,
    #[error("unexpected token at byte {0}: {1}")]
    Unexpected(usize, String),
    #[error("invalid duration: {0}")]
    BadDuration(String),
    #[error("nesting depth exceeds 3")]
    TooDeep,
    #[error("unknown atom: {0}")]
    UnknownAtom(String),
}

impl TriggerExpr {
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        Self::parse_with_depth(input.trim(), 0)
    }

    fn parse_with_depth(s: &str, depth: u8) -> Result<Self, ParseError> {
        if depth > 3 { return Err(ParseError::TooDeep); }
        if s.is_empty() { return Err(ParseError::Empty); }

        // Catch unbalanced "AND(..." / "OR(..." early so the test for unbalanced parens passes.
        if (s.starts_with("AND(") || s.starts_with("OR(")) && !s.ends_with(')') {
            return Err(ParseError::Unexpected(0, "unbalanced parentheses".into()));
        }

        if let Some(body) = s.strip_prefix("AND(").and_then(|t| t.strip_suffix(')')) {
            return parse_combinator(body, depth + 1).map(TriggerExpr::And);
        }
        if let Some(body) = s.strip_prefix("OR(").and_then(|t| t.strip_suffix(')')) {
            return parse_combinator(body, depth + 1).map(TriggerExpr::Or);
        }
        if let Some(rest) = s.strip_prefix("time:") { return parse_time_atom(rest); }
        if let Some(rest) = s.strip_prefix("event:") { return parse_event_atom(rest); }
        if let Some(rest) = s.strip_prefix("state:") { return parse_state_atom(rest); }
        Err(ParseError::UnknownAtom(s.to_string()))
    }
}

fn parse_combinator(body: &str, depth: u8) -> Result<Vec<TriggerExpr>, ParseError> {
    let parts = split_top_level_commas(body)?;
    if parts.len() < 2 {
        return Err(ParseError::Unexpected(0, "combinator needs ≥2 children".into()));
    }
    parts.into_iter().map(|p| TriggerExpr::parse_with_depth(p.trim(), depth)).collect()
}

fn split_top_level_commas(s: &str) -> Result<Vec<&str>, ParseError> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => { depth -= 1; if depth < 0 { return Err(ParseError::Unexpected(i, "unbalanced ')'".into())); } }
            ',' if depth == 0 => { out.push(&s[start..i]); start = i + 1; }
            _ => {}
        }
    }
    if depth != 0 { return Err(ParseError::Unexpected(s.len(), "unbalanced '('".into())); }
    out.push(&s[start..]);
    Ok(out)
}

fn parse_time_atom(rest: &str) -> Result<TriggerExpr, ParseError> {
    if rest == "dynamic" { return Ok(TriggerExpr::TimeDynamic); }
    parse_duration(rest).map(|d| {
        let rounded = if d < Duration::from_secs(60) { Duration::from_secs(60) } else { d };
        TriggerExpr::Time(rounded)
    })
}

fn parse_duration(s: &str) -> Result<Duration, ParseError> {
    if s.len() < 2 { return Err(ParseError::BadDuration(s.into())); }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: u64 = num.parse().map_err(|_| ParseError::BadDuration(s.into()))?;
    if n == 0 { return Err(ParseError::BadDuration(s.into())); }
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => return Err(ParseError::BadDuration(s.into())),
    };
    Ok(Duration::from_secs(secs))
}

fn parse_event_atom(rest: &str) -> Result<TriggerExpr, ParseError> {
    if rest.is_empty() {
        return Err(ParseError::Unexpected(0, "empty event topic".into()));
    }
    Ok(TriggerExpr::Event(rest.to_string()))
}

fn parse_state_atom(rest: &str) -> Result<TriggerExpr, ParseError> {
    let after_tab = rest.strip_prefix("tab=")
        .ok_or_else(|| ParseError::Unexpected(0, "expected 'tab='".into()))?;
    let (tab_str, rest) = after_tab.split_once(':')
        .ok_or_else(|| ParseError::Unexpected(0, "expected ':' after tab=…".into()))?;
    let tab = if tab_str == "current" { TabRef::Current }
              else { TabRef::Id(tab_str.parse().map_err(|_| ParseError::Unexpected(0, format!("bad tab id: {tab_str}")))?) };
    let selector = rest.strip_suffix(":change")
        .ok_or_else(|| ParseError::Unexpected(0, "state atom must end with ':change'".into()))?;
    if selector.is_empty() {
        return Err(ParseError::Unexpected(0, "empty selector".into()));
    }
    Ok(TriggerExpr::State { tab, selector: selector.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<TriggerExpr, ParseError> {
        TriggerExpr::parse(s)
    }

    #[test]
    fn time_seconds_round_up_to_minute() {
        assert_eq!(parse("time:30s").unwrap(), TriggerExpr::Time(Duration::from_secs(60)));
    }

    #[test]
    fn time_minutes_exact() {
        assert_eq!(parse("time:5m").unwrap(), TriggerExpr::Time(Duration::from_secs(300)));
    }

    #[test]
    fn time_hours_and_days() {
        assert_eq!(parse("time:2h").unwrap(), TriggerExpr::Time(Duration::from_secs(7200)));
        assert_eq!(parse("time:1d").unwrap(), TriggerExpr::Time(Duration::from_secs(86400)));
    }

    #[test]
    fn time_zero_is_rejected() {
        assert!(matches!(parse("time:0m"), Err(ParseError::BadDuration(_))));
    }

    #[test]
    fn time_missing_suffix_is_rejected() {
        assert!(matches!(parse("time:5"), Err(ParseError::BadDuration(_))));
    }

    #[test]
    fn time_dynamic() {
        assert_eq!(parse("time:dynamic").unwrap(), TriggerExpr::TimeDynamic);
    }

    #[test]
    fn event_atom() {
        assert_eq!(
            parse("event:ui:tab:*:click").unwrap(),
            TriggerExpr::Event("ui:tab:*:click".into())
        );
    }

    #[test]
    fn event_empty_topic_rejected() {
        assert!(matches!(parse("event:"), Err(ParseError::Unexpected(_, _))));
    }

    #[test]
    fn state_current_tab() {
        assert_eq!(
            parse("state:tab=current:.chat-list:change").unwrap(),
            TriggerExpr::State { tab: TabRef::Current, selector: ".chat-list".into() }
        );
    }

    #[test]
    fn state_numeric_tab() {
        assert_eq!(
            parse("state:tab=42:#root .item:change").unwrap(),
            TriggerExpr::State { tab: TabRef::Id(42), selector: "#root .item".into() }
        );
    }

    #[test]
    fn state_missing_change_suffix_rejected() {
        assert!(parse("state:tab=current:.x").is_err());
    }

    #[test]
    fn and_two_atoms() {
        assert_eq!(
            parse("AND(time:5m,event:foo)").unwrap(),
            TriggerExpr::And(vec![
                TriggerExpr::Time(Duration::from_secs(300)),
                TriggerExpr::Event("foo".into()),
            ])
        );
    }

    #[test]
    fn or_three_atoms() {
        assert_eq!(
            parse("OR(time:5m,event:a,event:b)").unwrap(),
            TriggerExpr::Or(vec![
                TriggerExpr::Time(Duration::from_secs(300)),
                TriggerExpr::Event("a".into()),
                TriggerExpr::Event("b".into()),
            ])
        );
    }

    #[test]
    fn nested_combinator_depth_2() {
        assert!(parse("AND(time:5m,OR(event:a,event:b))").is_ok());
    }

    #[test]
    fn depth_4_rejected() {
        let s = "AND(OR(AND(OR(time:1m,time:2m),time:3m),time:4m),time:5m)";
        assert_eq!(parse(s), Err(ParseError::TooDeep));
    }

    #[test]
    fn unbalanced_parens_rejected() {
        assert!(parse("AND(time:5m,event:foo").is_err());
    }
}
