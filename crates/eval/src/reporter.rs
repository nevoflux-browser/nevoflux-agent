//! Reporter — writes a [`RunSummary`] to disk as Markdown + JSON.
//!
//! Filename pattern depends on signal grade:
//! - Authoritative: `<benchmark>-<browser_version>.md` (versioned, committed to git)
//! - Exploratory:   `<timestamp>-<benchmark>.md` (timestamped, gitignored)
//!
//! Each report header MUST include signal grade and browser_version so a
//! reader cannot accidentally publish exploratory numbers as authoritative.

use crate::{metrics, runner::RunSummary, EvalResult, SignalGrade};
use std::path::Path;
use tokio::io::AsyncWriteExt;

pub async fn write_markdown(
    summary: &RunSummary,
    out_dir: &Path,
) -> EvalResult<std::path::PathBuf> {
    tokio::fs::create_dir_all(out_dir).await?;

    let filename = match summary.signal_grade {
        SignalGrade::Authoritative => format!(
            "{}-{}.md",
            summary.benchmark,
            sanitize(&summary.browser_version)
        ),
        SignalGrade::Exploratory => format!(
            "{}-{}-{}.md",
            summary.started_at.format("%Y%m%d-%H%M%S"),
            summary.benchmark,
            sanitize(&summary.browser_version)
        ),
    };
    let path = out_dir.join(&filename);

    let metrics = metrics::standard_set();
    let mut body = String::new();

    body.push_str(&format!("# Eval Report: {}\n\n", summary.benchmark));

    // Grade banner — visually distinct so you can't miss it.
    match summary.signal_grade {
        SignalGrade::Authoritative => {
            body.push_str(
                "> ✅ **AUTHORITATIVE** — browser came from a published nevoflux release. \
                 Safe to use for public reporting, leaderboard submissions, and \
                 marketing materials.\n\n",
            );
        }
        SignalGrade::Exploratory => {
            body.push_str(
                "> 🔬 **EXPLORATORY** — browser is a dev build or absent. \
                 Use for iteration signal only. **Do NOT publish these numbers.** \
                 For authoritative scores, run against a release binary.\n\n",
            );
        }
    }

    body.push_str("## Run metadata\n\n");
    body.push_str(&format!("- **Browser**: `{}`\n", summary.browser_version));
    body.push_str(&format!(
        "- **Signal grade**: `{:?}`\n",
        summary.signal_grade
    ));
    body.push_str(&format!("- **Judge**: `{}`\n", summary.judge));
    body.push_str(&format!(
        "- **Run started**: {}\n",
        summary.started_at.to_rfc3339()
    ));
    body.push_str(&format!(
        "- **Run finished**: {}\n\n",
        summary.finished_at.to_rfc3339()
    ));

    body.push_str("## Task counts\n\n");
    body.push_str(&format!("- **Total loaded**: {}\n", summary.total));
    body.push_str(&format!(
        "- **Effective (excludes skipped)**: {}\n",
        summary.effective_total()
    ));
    body.push_str(&format!("- **Passed**: {}\n", summary.passed));
    body.push_str(&format!("- **Failed**: {}\n", summary.failed));
    body.push_str(&format!(
        "- **Skipped (browser unavailable)**: {}\n",
        summary.skipped
    ));
    body.push_str(&format!("- **Timeouts**: {}\n\n", summary.timeouts));

    body.push_str("## Metrics\n\n");
    body.push_str("| Metric | Value |\n|---|---|\n");
    for m in &metrics {
        let v = m.compute(summary);
        body.push_str(&format!("| {} | {} |\n", m.name(), m.format(v)));
    }
    body.push('\n');

    body.push_str("## Cost\n\n");
    body.push_str(&format!(
        "- **Token cost (agent runs)**: ${:.4}\n",
        summary.total_token_cost_usd
    ));
    body.push_str(&format!(
        "- **Judge cost (LLM-as-judge)**: ${:.4}\n",
        summary.total_judge_cost_usd
    ));
    body.push_str(&format!(
        "- **Total**: ${:.4}\n\n",
        summary.total_token_cost_usd + summary.total_judge_cost_usd
    ));

    body.push_str("## Failed Tasks\n\n");
    let failed: Vec<_> = summary
        .per_task
        .iter()
        .filter(|o| o.verdict.as_ref().map(|v| !v.correct).unwrap_or(false))
        .collect();
    if failed.is_empty() {
        body.push_str("_None — all judged tasks passed._\n\n");
    } else {
        body.push_str("| Task ID | Reason |\n|---|---|\n");
        for o in failed.iter().take(20) {
            let explanation = o
                .verdict
                .as_ref()
                .map(|v| {
                    v.explanation
                        .replace('|', "\\|")
                        .chars()
                        .take(80)
                        .collect::<String>()
                })
                .unwrap_or_else(|| "(no verdict)".into());
            body.push_str(&format!("| `{}` | {} |\n", o.task.id, explanation));
        }
        if failed.len() > 20 {
            body.push_str(&format!(
                "\n_{} more failures truncated._\n",
                failed.len() - 20
            ));
        }
    }

    if summary.skipped > 0 {
        body.push_str("\n## Skipped Tasks\n\n");
        body.push_str(&format!(
            "_{} tasks marked `requires_browser: true` were skipped because runner is in DaemonOnly mode._\n",
            summary.skipped
        ));
        body.push_str(
            "_For coverage of these, re-run with `--browser-mode external` or `--browser-mode release`._\n",
        );
    }

    // JSON sidecar — written first so it exists even if the Markdown write fails.
    let json_path = path.with_extension("json");
    let json = serde_json::to_string_pretty(summary)?;
    tokio::fs::write(&json_path, json).await?;

    let mut file = tokio::fs::File::create(&path).await?;
    file.write_all(body.as_bytes()).await?;
    file.flush().await?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        judge::Verdict,
        runner::{RunSummary, TaskOutcome},
        NevoFluxMode, SignalGrade, Task, TaskResult, TaskStatus,
    };

    fn make_summary() -> RunSummary {
        RunSummary {
            benchmark: "test".into(),
            judge: "programmatic".into(),
            signal_grade: SignalGrade::Exploratory,
            browser_version: "test-0.0.0".into(),
            total: 1,
            passed: 1,
            failed: 0,
            skipped: 0,
            timeouts: 0,
            mean_latency_ms: 10.0,
            p99_latency_ms: 12,
            total_token_cost_usd: 0.0,
            total_judge_cost_usd: 0.0,
            started_at: chrono::Utc::now(),
            finished_at: chrono::Utc::now(),
            per_task: vec![TaskOutcome {
                task: Task {
                    id: "t".into(),
                    category: "c".into(),
                    mode: NevoFluxMode::Chat,
                    prompt: "p".into(),
                    setup: vec![],
                    reference: None,
                    assertions: vec![],
                    requires_browser: false,
                    metadata: Default::default(),
                    supports_platform: vec![],
                },
                result: TaskResult {
                    task_id: "t".into(),
                    status: TaskStatus::Completed,
                    final_answer: Some("a".into()),
                    latency_ms: 10,
                    token_cost: None,
                    error: None,
                    trace_ids: vec![],
                    observed_events: vec![],
                    outbound_hosts: vec![],
                },
                verdict: Some(Verdict {
                    correct: true,
                    score: 1.0,
                    explanation: "ok".into(),
                    judge_cost_usd: 0.0,
                }),
            }],
        }
    }

    #[tokio::test]
    async fn writes_markdown_and_json_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_markdown(&make_summary(), tmp.path()).await.unwrap();
        assert!(path.exists(), "Markdown file should exist");
        assert!(
            path.with_extension("json").exists(),
            "JSON sidecar should exist"
        );
        let md = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            md.contains("EXPLORATORY"),
            "Grade banner should contain EXPLORATORY"
        );
        assert!(
            md.contains("accuracy"),
            "Metrics section should contain accuracy metric"
        );
    }
}

pub async fn write_json(summary: &RunSummary, out_dir: &Path) -> EvalResult<std::path::PathBuf> {
    tokio::fs::create_dir_all(out_dir).await?;
    let filename = match summary.signal_grade {
        SignalGrade::Authoritative => format!(
            "{}-{}.json",
            summary.benchmark,
            sanitize(&summary.browser_version)
        ),
        SignalGrade::Exploratory => format!(
            "{}-{}-{}.json",
            summary.started_at.format("%Y%m%d-%H%M%S"),
            summary.benchmark,
            sanitize(&summary.browser_version)
        ),
    };
    let path = out_dir.join(&filename);
    let json = serde_json::to_string_pretty(summary)?;
    tokio::fs::write(&path, json).await?;
    Ok(path)
}

/// Append a single line to trends.json for dashboard time-series.
/// Only authoritative runs are included — exploratory would pollute the trend.
pub async fn append_trend(summary: &RunSummary, trends_path: &Path) -> EvalResult<()> {
    if !matches!(summary.signal_grade, SignalGrade::Authoritative) {
        // Skip exploratory runs in trend tracking.
        return Ok(());
    }

    let metrics = metrics::standard_set();
    let mut row = serde_json::Map::new();
    row.insert("benchmark".into(), summary.benchmark.clone().into());
    row.insert(
        "browser_version".into(),
        summary.browser_version.clone().into(),
    );
    row.insert("timestamp".into(), summary.started_at.to_rfc3339().into());
    for m in &metrics {
        row.insert(m.name().into(), m.compute(summary).into());
    }
    let line = format!("{}\n", serde_json::to_string(&row)?);

    if let Some(parent) = trends_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(trends_path)
        .await?;
    file.write_all(line.as_bytes()).await?;
    Ok(())
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
