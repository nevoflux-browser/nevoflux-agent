//! Token accounting for a single assistant reply.
//!
//! Provider-reported usage is preferred; when a provider reports nothing — or
//! reports zero while content actually flowed — the numbers fall back to a
//! character estimate and the bucket is flagged `estimated`. See
//! `docs/superpowers/specs/2026-09-22-message-token-stats-design.md` in the
//! browser repo for the full design.

use nevoflux_protocol::{TurnUsage, UsageBucket};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Whether a character counts as one whole token.
///
/// Covers CJK unified ideographs plus extensions A/B, kana, hangul syllables,
/// compatibility ideographs and fullwidth forms.
fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x3040..=0x30FF      // hiragana / katakana
            | 0x3400..=0x4DBF // CJK extension A
            | 0x4E00..=0x9FFF // CJK unified ideographs
            | 0xAC00..=0xD7AF // hangul syllables
            | 0xF900..=0xFAFF // CJK compatibility ideographs
            | 0xFF00..=0xFF60 // fullwidth forms
            | 0x20000..=0x2A6DF // CJK extension B
    )
}

/// Estimate the token count of a piece of text.
///
/// CJK characters count as one token each, everything else as one token per
/// four characters rounded up. Only used when a provider reported no usage;
/// results are always flagged as estimated.
pub fn estimate_tokens(text: &str) -> u64 {
    let mut cjk = 0u64;
    let mut other = 0u64;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}

/// One finished LLM call, submitted by `agent_host` when the call settles.
#[derive(Debug, Clone)]
pub struct CallStats {
    /// This call belongs to a subagent.
    pub is_subagent: bool,
    /// The provider runs its own agent loop (ACP family and Kimi Agent).
    pub external_agent: bool,
    /// Input tokens as reported by the provider, `None` when not reported.
    pub reported_input: Option<u64>,
    /// Output tokens as reported by the provider, `None` when not reported.
    pub reported_output: Option<u64>,
    /// Estimated input, always computed so it can serve as the fallback.
    pub estimated_input: u64,
    /// Estimated output, always computed so it can serve as the fallback.
    pub estimated_output: u64,
    /// Generation window in milliseconds (first chunk to last). `None` for
    /// non-streaming calls.
    pub decode_ms: Option<u64>,
    /// Request-to-first-chunk latency in milliseconds. `None` for
    /// non-streaming calls.
    pub first_token_ms: Option<u64>,
    /// Model used by this call.
    pub model: String,
}

/// Resolve one figure: a reported non-zero value wins, otherwise fall back to
/// the estimate when there is anything to estimate.
fn resolve(reported: Option<u64>, estimated: u64) -> (u64, bool) {
    match reported {
        Some(v) if v > 0 => (v, false),
        _ if estimated > 0 => (estimated, true),
        _ => (0, false),
    }
}

/// Usage accumulator for one assistant reply.
///
/// A single instance is shared for the whole turn: the main agent, its
/// subagents and any proxy hosts all write into it, so the snapshot read when
/// the done frame is sent and the one written to the database are the same
/// set of facts.
pub struct TurnStats {
    started: Instant,
    calls: Mutex<Vec<CallStats>>,
}

impl TurnStats {
    /// Start a new accumulator; the turn's clock starts now.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Instant::now(),
            calls: Mutex::new(Vec::new()),
        })
    }

    /// Record one finished LLM call.
    pub fn record(&self, call: CallStats) {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(call);
        }
    }

    /// Summarise into a wire snapshot. Returns `None` when no LLM call ran.
    pub fn snapshot(&self) -> Option<TurnUsage> {
        let calls = self.calls.lock().ok()?;
        if calls.is_empty() {
            return None;
        }

        let mut usage = TurnUsage {
            total_ms: Some(self.started.elapsed().as_millis() as u64),
            ..Default::default()
        };
        let mut subagent = UsageBucket::default();
        let mut has_subagent = false;
        let mut decode_total = 0u64;
        let mut has_decode = false;

        for call in calls.iter() {
            let (input, input_est) = resolve(call.reported_input, call.estimated_input);
            let (output, output_est) = resolve(call.reported_output, call.estimated_output);
            let bucket = if call.is_subagent {
                has_subagent = true;
                &mut subagent
            } else {
                &mut usage.main
            };
            bucket.input += input;
            bucket.output += output;
            bucket.calls += 1;
            bucket.estimated |= input_est || output_est;

            if call.is_subagent {
                continue;
            }
            // Everything below is main-agent only: subagents often run in
            // parallel, so their windows must not be summed into tok/s.
            usage.last_input = Some(input);
            usage.model = Some(call.model.clone());
            usage.external_agent |= call.external_agent;
            if let Some(ms) = call.decode_ms {
                decode_total += ms;
                has_decode = true;
            }
            if usage.first_token_ms.is_none() {
                usage.first_token_ms = call.first_token_ms;
            }
        }

        if has_subagent {
            usage.subagent = Some(subagent);
        }
        if has_decode {
            usage.decode_ms = Some(decode_total);
        }
        Some(usage)
    }
}

#[cfg(test)]
mod estimate_tests {
    use super::*;

    #[test]
    fn empty_text_is_zero() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn ascii_counts_four_chars_per_token_rounded_up() {
        assert_eq!(estimate_tokens("hello world!"), 3); // 12 / 4
        assert_eq!(estimate_tokens("abcde"), 2); // 5 / 4 rounds up
    }

    #[test]
    fn cjk_counts_one_token_per_character() {
        assert_eq!(estimate_tokens("你好世界"), 4);
        assert_eq!(estimate_tokens("こんにちは"), 5);
        assert_eq!(estimate_tokens("안녕하세요"), 5);
    }

    #[test]
    fn mixed_text_adds_both_parts() {
        // 4 CJK characters plus 9 non-CJK (space included) -> 4 + ceil(9/4)
        assert_eq!(estimate_tokens("你好世界 hello123"), 4 + 3);
    }
}

#[cfg(test)]
mod turn_stats_tests {
    use super::*;

    /// A main-agent streaming call; tests override what they care about.
    fn call() -> CallStats {
        CallStats {
            is_subagent: false,
            external_agent: false,
            reported_input: Some(100),
            reported_output: Some(20),
            estimated_input: 999,
            estimated_output: 999,
            decode_ms: Some(1000),
            first_token_ms: Some(300),
            model: "m1".into(),
        }
    }

    #[test]
    fn no_calls_means_no_snapshot() {
        assert!(TurnStats::new().snapshot().is_none());
    }

    #[test]
    fn reported_usage_wins_and_is_not_flagged_estimated() {
        let s = TurnStats::new();
        s.record(call());
        let u = s.snapshot().unwrap();
        assert_eq!((u.main.input, u.main.output, u.main.calls), (100, 20, 1));
        assert!(!u.main.estimated);
    }

    #[test]
    fn zero_output_with_streamed_content_falls_back_to_estimate() {
        let s = TurnStats::new();
        s.record(CallStats {
            reported_output: Some(0),
            estimated_output: 120,
            ..call()
        });
        let u = s.snapshot().unwrap();
        assert_eq!(u.main.output, 120);
        assert!(u.main.estimated);
    }

    #[test]
    fn absent_usage_falls_back_to_estimate() {
        let s = TurnStats::new();
        s.record(CallStats {
            reported_input: None,
            reported_output: None,
            estimated_input: 50,
            estimated_output: 7,
            ..call()
        });
        let u = s.snapshot().unwrap();
        assert_eq!((u.main.input, u.main.output), (50, 7));
        assert!(u.main.estimated);
    }

    #[test]
    fn zero_report_with_nothing_to_estimate_is_not_flagged() {
        let s = TurnStats::new();
        s.record(CallStats {
            reported_input: Some(0),
            reported_output: Some(0),
            estimated_input: 0,
            estimated_output: 0,
            ..call()
        });
        let u = s.snapshot().unwrap();
        assert_eq!((u.main.input, u.main.output), (0, 0));
        assert!(
            !u.main.estimated,
            "nothing to estimate must not be called an estimate"
        );
    }

    #[test]
    fn subagent_calls_land_in_their_own_bucket() {
        let s = TurnStats::new();
        s.record(call());
        s.record(CallStats {
            is_subagent: true,
            reported_input: Some(40),
            reported_output: Some(9),
            ..call()
        });
        let u = s.snapshot().unwrap();
        assert_eq!((u.main.input, u.main.calls), (100, 1));
        let sub = u.subagent.unwrap();
        assert_eq!((sub.input, sub.output, sub.calls), (40, 9, 1));
    }

    #[test]
    fn subagent_bucket_is_absent_without_subagent_calls() {
        let s = TurnStats::new();
        s.record(call());
        assert!(s.snapshot().unwrap().subagent.is_none());
    }

    #[test]
    fn decode_time_sums_main_agent_streaming_calls_only() {
        let s = TurnStats::new();
        s.record(CallStats {
            decode_ms: Some(1000),
            ..call()
        });
        s.record(CallStats {
            decode_ms: Some(500),
            ..call()
        });
        s.record(CallStats {
            is_subagent: true,
            decode_ms: Some(9999),
            ..call()
        });
        s.record(CallStats {
            decode_ms: None,
            ..call()
        });
        assert_eq!(s.snapshot().unwrap().decode_ms, Some(1500));
    }

    #[test]
    fn decode_time_is_absent_when_no_main_streaming_call_has_one() {
        let s = TurnStats::new();
        s.record(CallStats {
            decode_ms: None,
            first_token_ms: None,
            ..call()
        });
        assert!(s.snapshot().unwrap().decode_ms.is_none());
    }

    #[test]
    fn last_input_and_model_come_from_the_last_main_call() {
        let s = TurnStats::new();
        s.record(CallStats {
            reported_input: Some(100),
            model: "m1".into(),
            ..call()
        });
        s.record(CallStats {
            reported_input: Some(3204),
            model: "m2".into(),
            ..call()
        });
        s.record(CallStats {
            is_subagent: true,
            reported_input: Some(7),
            model: "sub".into(),
            ..call()
        });
        let u = s.snapshot().unwrap();
        assert_eq!(u.last_input, Some(3204));
        assert_eq!(u.model.as_deref(), Some("m2"));
    }

    #[test]
    fn first_token_ms_comes_from_the_first_main_call() {
        let s = TurnStats::new();
        s.record(CallStats {
            first_token_ms: Some(1200),
            ..call()
        });
        s.record(CallStats {
            first_token_ms: Some(80),
            ..call()
        });
        assert_eq!(s.snapshot().unwrap().first_token_ms, Some(1200));
    }

    #[test]
    fn external_agent_is_sticky_once_any_main_call_sets_it() {
        let s = TurnStats::new();
        s.record(CallStats {
            external_agent: true,
            ..call()
        });
        s.record(call());
        assert!(s.snapshot().unwrap().external_agent);
    }

    #[test]
    fn total_ms_is_always_present() {
        let s = TurnStats::new();
        s.record(call());
        assert!(s.snapshot().unwrap().total_ms.is_some());
    }
}
