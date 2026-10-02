//! Rebuild economics (spec §5.7, v1.4 §4.5): what keeping the cached history
//! costs against rebuilding it, in uncached-input-token units.

use std::collections::BTreeMap;

use nevoflux_llm::ProviderType;
use serde::{Deserialize, Serialize};

/// A provider's prompt-cache prices relative to its uncached input price,
/// and how long a cached prefix lives.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CacheRate {
    /// Reading a cached token costs this fraction of an uncached one.
    pub read: f64,
    /// Writing (caching) a token costs this multiple of an uncached one.
    pub write: f64,
    /// Seconds a cached prefix stays warm after its last use (0: no cache).
    pub ttl_secs: u64,
}

/// H when Jev gave none: remaining requests that will read the prefix.
pub const DEFAULT_H: u32 = 3;
/// Pollution threshold (spec §5.7, θ).
pub const THETA: f64 = 0.8;
/// A polluted rebuild may cost up to this much more than keeping (ρ).
pub const RHO: f64 = 0.25;
/// A cost-formula rebuild must save at least this share (m).
pub const M: f64 = 0.05;

/// Built-in rates by wire; `[jev.cache.<wire>]` overrides them.
pub fn cache_rate(wire: ProviderType, overrides: &BTreeMap<String, CacheRate>) -> CacheRate {
    let key = wire_key(wire);
    if let Some(r) = overrides.get(key) {
        return *r;
    }
    match wire {
        ProviderType::Anthropic => CacheRate {
            read: 0.1,
            write: 1.25,
            ttl_secs: 300,
        },
        ProviderType::DeepSeek => CacheRate {
            read: 0.1,
            write: 1.0,
            ttl_secs: 3600,
        },
        ProviderType::OpenAi => CacheRate {
            read: 0.5,
            write: 1.0,
            ttl_secs: 300,
        },
        _ => CacheRate {
            read: 1.0,
            write: 1.0,
            ttl_secs: 0,
        },
    }
}

fn wire_key(wire: ProviderType) -> &'static str {
    match wire {
        ProviderType::Anthropic => "anthropic",
        ProviderType::DeepSeek => "deepseek",
        ProviderType::OpenAi => "openai",
        _ => "other",
    }
}

/// Remaining requests that will read the prefix: Jev's remaining tool-using
/// steps plus the answer request (P2-3a carry-over).
pub fn remaining_requests(h_tool_steps: Option<u32>) -> u32 {
    h_tool_steps.map_or(DEFAULT_H, |h| h + 1)
}

/// Keeping a prefix of `p` tokens for `h` more requests.
pub fn keep_cost(p: f64, h: u32, rate: CacheRate, warm: bool) -> f64 {
    let h = h as f64;
    if warm {
        h * p * rate.read
    } else {
        p * rate.write + (h - 1.0).max(0.0) * p * rate.read
    }
}

/// Rebuilding into a prefix of `a` tokens, plus the Jev tokens it took.
pub fn rebuild_cost(a: f64, h: u32, rate: CacheRate, jev_tokens: f64) -> f64 {
    a * rate.write + (h as f64 - 1.0).max(0.0) * a * rate.read + jev_tokens
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Keep,
    Rebuild,
}

/// v1.4 §4.5: rebuild when it saves at least m, or when polluted and it costs
/// at most ρ more.
pub fn decide(keep: f64, rebuild: f64, polluted: bool) -> Decision {
    if rebuild < keep * (1.0 - M) || (polluted && rebuild <= keep * (1.0 + RHO)) {
        Decision::Rebuild
    } else {
        Decision::Keep
    }
}

/// Whether the provider still holds the cached prefix.
pub fn warm(last_request_ms: Option<i64>, now_ms: i64, rate: CacheRate) -> bool {
    match last_request_ms {
        Some(last) if rate.ttl_secs > 0 => now_ms - last < rate.ttl_secs as i64 * 1000,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() <= b.abs() * 0.01
    }

    #[test]
    fn the_v14_examples_hold() {
        let (p, a, h) = (90_000.0, 35_000.0, 10);
        let opus48 = CacheRate {
            read: 0.1,
            write: 1.25,
            ttl_secs: 300,
        };
        let k = keep_cost(p, h, opus48, true);
        let r = rebuild_cost(a, h, opus48, 0.0);
        assert!(near(k, 90_000.0) && near(r, 75_250.0), "{k} {r}");
        assert_eq!(decide(k, r, false), Decision::Rebuild);

        let opus55 = CacheRate {
            read: 0.05,
            write: 1.25,
            ttl_secs: 300,
        };
        let k = keep_cost(p, h, opus55, true);
        let r = rebuild_cost(a, h, opus55, 0.0);
        assert!(near(k, 45_000.0) && near(r, 59_500.0), "{k} {r}");
        assert_eq!(decide(k, r, false), Decision::Keep);

        let k = keep_cost(p, h, opus55, false);
        assert!(near(k, 153_000.0), "{k}");
        assert_eq!(decide(k, r, false), Decision::Rebuild);
    }

    #[test]
    fn polluted_rebuilds_within_rho() {
        assert_eq!(decide(100.0, 120.0, true), Decision::Rebuild);
        assert_eq!(decide(100.0, 130.0, true), Decision::Keep);
        assert_eq!(decide(100.0, 120.0, false), Decision::Keep);
        assert_eq!(decide(100.0, 96.0, false), Decision::Keep, "m = 5% margin");
        assert_eq!(decide(100.0, 94.0, false), Decision::Rebuild);
    }

    #[test]
    fn h_counts_the_answer_request() {
        assert_eq!(remaining_requests(Some(4)), 5);
        assert_eq!(remaining_requests(Some(0)), 1);
        assert_eq!(remaining_requests(None), DEFAULT_H);
    }

    #[test]
    fn warmth_follows_the_ttl() {
        let r = CacheRate {
            read: 0.1,
            write: 1.25,
            ttl_secs: 300,
        };
        let now = 1_000_000_000;
        assert!(warm(Some(now - 200_000), now, r));
        assert!(!warm(Some(now - 400_000), now, r));
        assert!(!warm(None, now, r));
        let none = CacheRate { ttl_secs: 0, ..r };
        assert!(!warm(Some(now - 1), now, none));
    }

    #[test]
    fn cache_rates_default_by_wire_and_are_overridable() {
        let none = BTreeMap::new();
        assert_eq!(
            cache_rate(ProviderType::Anthropic, &none),
            CacheRate {
                read: 0.1,
                write: 1.25,
                ttl_secs: 300
            }
        );
        assert_eq!(
            cache_rate(ProviderType::DeepSeek, &none),
            CacheRate {
                read: 0.1,
                write: 1.0,
                ttl_secs: 3600
            }
        );
        assert_eq!(
            cache_rate(ProviderType::OpenAi, &none),
            CacheRate {
                read: 0.5,
                write: 1.0,
                ttl_secs: 300
            }
        );
        assert_eq!(
            cache_rate(ProviderType::Groq, &none),
            CacheRate {
                read: 1.0,
                write: 1.0,
                ttl_secs: 0
            }
        );
        let mut o = BTreeMap::new();
        o.insert(
            "anthropic".to_string(),
            CacheRate {
                read: 0.05,
                write: 1.25,
                ttl_secs: 3600,
            },
        );
        assert_eq!(cache_rate(ProviderType::Anthropic, &o).read, 0.05);
    }

    #[test]
    fn the_jev_section_reads_cache_overrides() {
        let cfg: crate::config::AgentConfig =
            toml::from_str("[jev.cache.anthropic]\nread = 0.05\nwrite = 1.25\nttl_secs = 3600\n")
                .unwrap();
        assert_eq!(cfg.jev.cache["anthropic"].ttl_secs, 3600);
    }
}
