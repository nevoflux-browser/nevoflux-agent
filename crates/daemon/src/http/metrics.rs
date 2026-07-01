//! Prometheus-style metrics for the headless HTTP API (P6).

use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide counters exposed at `GET /metrics`.
#[derive(Default)]
pub struct Metrics {
    /// Total tasks accepted.
    pub tasks_total: AtomicU64,
    /// Total tasks that ended in failure.
    pub tasks_failed: AtomicU64,
    /// Total browser restarts by the supervisor.
    pub browser_restarts: AtomicU64,
}

impl Metrics {
    /// Render the counters as Prometheus text exposition format.
    pub fn render(&self) -> String {
        let t = self.tasks_total.load(Ordering::Relaxed);
        let f = self.tasks_failed.load(Ordering::Relaxed);
        let r = self.browser_restarts.load(Ordering::Relaxed);
        format!(
            "# HELP nevoflux_tasks_total Total tasks accepted.\n\
             # TYPE nevoflux_tasks_total counter\n\
             nevoflux_tasks_total {t}\n\
             # HELP nevoflux_tasks_failed Total tasks that failed.\n\
             # TYPE nevoflux_tasks_failed counter\n\
             nevoflux_tasks_failed {f}\n\
             # HELP nevoflux_browser_restarts Total browser restarts.\n\
             # TYPE nevoflux_browser_restarts counter\n\
             nevoflux_browser_restarts {r}\n"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_render_prometheus() {
        let m = Metrics::default();
        m.tasks_total.fetch_add(3, Ordering::Relaxed);
        m.tasks_failed.fetch_add(1, Ordering::Relaxed);
        let s = m.render();
        assert!(s.contains("# TYPE nevoflux_tasks_total counter"));
        assert!(s.contains("nevoflux_tasks_total 3"));
        assert!(s.contains("nevoflux_tasks_failed 1"));
        assert!(s.contains("nevoflux_browser_restarts 0"));
    }
}
