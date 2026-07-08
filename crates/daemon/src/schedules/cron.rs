//! Cron parsing for schedules. 5-field expressions, interpreted in the
//! daemon's local timezone; results returned as UTC unix seconds.
//! Minimum interval between fires is 1 hour — finer cadences belong to /loop.

use chrono::{Local, TimeZone};
use croner::Cron;

const MIN_GAP_SECS: u64 = 3600;
/// How many successive occurrences to sample when validating cadence.
const VALIDATE_SAMPLES: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum CronError {
    #[error("invalid cron expression: {0}")]
    Parse(String),
    #[error("cron fires more often than every {min_gap_secs}s; use /loop for sub-hourly cadences")]
    TooFrequent { min_gap_secs: u64 },
    #[error("cron expression never fires in the future")]
    NoFutureFire,
}

fn parse(expr: &str) -> Result<Cron, CronError> {
    Cron::new(expr)
        .parse()
        .map_err(|e| CronError::Parse(e.to_string()))
}

/// Next fire strictly after `after_utc`, as UTC seconds.
pub fn next_after(expr: &str, after_utc: i64) -> Result<i64, CronError> {
    let cron = parse(expr)?;
    let after_local = Local
        .timestamp_opt(after_utc, 0)
        .single()
        .ok_or(CronError::NoFutureFire)?;
    let next = cron
        .find_next_occurrence(&after_local, false)
        .map_err(|_| CronError::NoFutureFire)?;
    Ok(next.timestamp())
}

/// Parse + enforce the 1-hour minimum cadence by sampling successive fires,
/// then return the first fire strictly after `after_utc`.
///
/// Sampling (rather than inspecting the parsed pattern directly) also
/// catches DST-induced short gaps: a clock-forward transition can make two
/// nominally-hourly local fires land less than an hour apart in UTC, and a
/// clock-back transition can make two fires collide. Successive
/// `next_after` calls walk real calendar time, so both cases surface here.
pub fn validate_and_next(expr: &str, after_utc: i64) -> Result<i64, CronError> {
    let first = next_after(expr, after_utc)?;
    let mut prev = first;
    for _ in 0..VALIDATE_SAMPLES {
        let next = next_after(expr, prev)?;
        if (next - prev) < MIN_GAP_SECS as i64 {
            return Err(CronError::TooFrequent {
                min_gap_secs: MIN_GAP_SECS,
            });
        }
        prev = next;
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_9am_is_valid() {
        // next fire strictly after `after`, and a second application jumps ~24h
        let t0 = validate_and_next("0 9 * * *", 1_800_000_000).expect("valid");
        assert!(t0 > 1_800_000_000);
        let t1 = next_after("0 9 * * *", t0).expect("valid");
        assert_eq!(t1 - t0, 86_400);
    }

    #[test]
    fn every_30_minutes_rejected() {
        match validate_and_next("*/30 * * * *", 1_800_000_000) {
            Err(CronError::TooFrequent { .. }) => {}
            other => panic!("expected TooFrequent, got {other:?}"),
        }
    }

    #[test]
    fn hourly_is_allowed() {
        validate_and_next("0 * * * *", 1_800_000_000).expect("hourly ok");
    }

    #[test]
    fn garbage_is_parse_error() {
        assert!(matches!(
            validate_and_next("not a cron", 0),
            Err(CronError::Parse(_))
        ));
    }

    #[test]
    fn weekdays_cron_valid() {
        validate_and_next("0 9 * * 1-5", 1_800_000_000).expect("weekdays ok");
    }
}
