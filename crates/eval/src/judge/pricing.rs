//! Per-model unit-price table (USD per 1K tokens) used to convert
//! Anthropic-style `response.usage` into a `judge_cost_usd` on Verdict.
//!
//! Prices are best-effort snapshots as of the eval Phase 4 ship date and
//! cover the models a typical NevoFlux setup would use as a judge. Prices
//! for unrecognised models default to zero — better to under-report than
//! invent a number.

/// USD price per 1K input + output tokens for a given model identifier.
/// Returns `(input_per_1k, output_per_1k)`; both 0.0 when unknown.
pub fn price_per_1k_tokens(model: &str) -> (f64, f64) {
    // Match prefixes — Anthropic uses dated suffixes like
    // "claude-3-5-sonnet-20240620".
    let m = model.to_ascii_lowercase();
    if m.starts_with("claude-opus-4") {
        (0.015, 0.075)
    } else if m.starts_with("claude-sonnet-4") || m.starts_with("claude-3-5-sonnet") {
        (0.003, 0.015)
    } else if m.starts_with("claude-haiku-4") || m.starts_with("claude-3-5-haiku") {
        (0.0008, 0.004)
    } else if m.starts_with("claude-3-opus") {
        (0.015, 0.075)
    } else if m.starts_with("claude-3-haiku") {
        (0.00025, 0.00125)
    } else if m.starts_with("gpt-4o") {
        (0.0025, 0.01)
    } else if m.starts_with("gpt-4") {
        (0.01, 0.03)
    } else if m.starts_with("gpt-3.5") {
        (0.0005, 0.0015)
    } else {
        (0.0, 0.0)
    }
}

/// Compute USD cost from input/output tokens via the model's unit price.
pub fn estimate_cost_usd(model: &str, input_tokens: u32, output_tokens: u32) -> f64 {
    let (in_per_1k, out_per_1k) = price_per_1k_tokens(model);
    (input_tokens as f64 / 1000.0) * in_per_1k + (output_tokens as f64 / 1000.0) * out_per_1k
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_sonnet_model_returns_nonzero_price() {
        let (i, o) = price_per_1k_tokens("claude-3-5-sonnet-20240620");
        assert!(i > 0.0);
        assert!(o > i, "output tokens cost more than input");
    }

    #[test]
    fn unknown_model_returns_zero() {
        let (i, o) = price_per_1k_tokens("frobnicator-9000");
        assert_eq!(i, 0.0);
        assert_eq!(o, 0.0);
    }

    #[test]
    fn estimate_cost_zero_for_unknown_model() {
        assert_eq!(estimate_cost_usd("totally-fake", 1_000, 1_000), 0.0);
    }

    #[test]
    fn estimate_cost_uses_per_thousand_math() {
        // sonnet: $0.003 input + $0.015 output per 1k
        // 2k input + 1k output = 0.006 + 0.015 = 0.021
        let usd = estimate_cost_usd("claude-3-5-sonnet-20240620", 2_000, 1_000);
        assert!((usd - 0.021).abs() < 1e-9, "got {usd}");
    }

    #[test]
    fn case_insensitive_model_match() {
        let lower = price_per_1k_tokens("claude-3-5-sonnet-x");
        let upper = price_per_1k_tokens("Claude-3-5-Sonnet-x");
        assert_eq!(lower, upper);
    }
}
