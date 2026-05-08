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
