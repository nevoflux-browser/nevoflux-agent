//! Trigger expression grammar (spec §5.1).

use std::time::Duration;

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

        if let Some(rest) = s.strip_prefix("time:") {
            return parse_time_atom(rest);
        }
        Err(ParseError::UnknownAtom(s.to_string()))
    }
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
}
