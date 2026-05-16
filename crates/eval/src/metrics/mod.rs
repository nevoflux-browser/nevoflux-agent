//! Metrics — computed from a [`crate::runner::RunSummary`].
//!
//! Each metric is a pure function over the summary; reporters call them and
//! format the results. Add new metrics by implementing [`Metric`].

use crate::runner::RunSummary;

pub trait Metric {
    fn name(&self) -> &str;
    fn compute(&self, summary: &RunSummary) -> f64;
    fn format(&self, value: f64) -> String;
}

pub struct Accuracy;
impl Metric for Accuracy {
    fn name(&self) -> &str {
        "accuracy"
    }
    fn compute(&self, s: &RunSummary) -> f64 {
        s.accuracy()
    }
    fn format(&self, v: f64) -> String {
        format!("{:.1}%", v * 100.0)
    }
}

pub struct CostNormalizedAccuracy;
impl Metric for CostNormalizedAccuracy {
    fn name(&self) -> &str {
        "cost_normalized_accuracy"
    }
    /// CNA = passed_tasks / total_usd. Higher is better.
    /// When total cost is 0 (programmatic-only run), returns infinity sentinel = 999_999.
    fn compute(&self, s: &RunSummary) -> f64 {
        let total_cost = s.total_token_cost_usd + s.total_judge_cost_usd;
        if total_cost <= 0.0 {
            999_999.0
        } else {
            s.passed as f64 / total_cost
        }
    }
    fn format(&self, v: f64) -> String {
        if v >= 999_998.0 {
            "∞ (zero cost)".into()
        } else {
            format!("{:.2} passes/$", v)
        }
    }
}

pub struct MeanLatency;
impl Metric for MeanLatency {
    fn name(&self) -> &str {
        "mean_latency_ms"
    }
    fn compute(&self, s: &RunSummary) -> f64 {
        s.mean_latency_ms
    }
    fn format(&self, v: f64) -> String {
        format!("{:.0} ms", v)
    }
}

pub struct P99Latency;
impl Metric for P99Latency {
    fn name(&self) -> &str {
        "p99_latency_ms"
    }
    fn compute(&self, s: &RunSummary) -> f64 {
        s.p99_latency_ms as f64
    }
    fn format(&self, v: f64) -> String {
        format!("{:.0} ms", v)
    }
}

pub struct TimeoutRate;
impl Metric for TimeoutRate {
    fn name(&self) -> &str {
        "timeout_rate"
    }
    fn compute(&self, s: &RunSummary) -> f64 {
        let denom = s.effective_total();
        if denom == 0 {
            0.0
        } else {
            s.timeouts as f64 / denom as f64
        }
    }
    fn format(&self, v: f64) -> String {
        format!("{:.1}%", v * 100.0)
    }
}

pub fn standard_set() -> Vec<Box<dyn Metric>> {
    vec![
        Box::new(Accuracy),
        Box::new(CostNormalizedAccuracy),
        Box::new(MeanLatency),
        Box::new(P99Latency),
        Box::new(TimeoutRate),
    ]
}
