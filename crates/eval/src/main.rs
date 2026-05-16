//! `nevoflux-eval` CLI.
//!
//! Three browser modes, three corresponding usage patterns:
//!
//! ```bash
//! # 1. Daemon-only — fastest, no browser. Catches daemon-side regressions.
//! nevoflux-eval run --benchmark nevoflux-suite --browser-mode daemon-only
//!
//! # 2. External dev instance — connect to your locally-running nevoflux.
//! #    You ran `just dev` in the nevoflux repo first. Exploratory signal grade.
//! nevoflux-eval run --benchmark online-mind2web \
//!     --browser-mode external --browser-endpoint http://localhost:5959 --limit 20
//!
//! # 3. Release binary — download a published release. Authoritative signal grade.
//! nevoflux-eval run --benchmark online-mind2web \
//!     --browser-mode release --browser-version v0.3.2
//! ```
//!
//! Reports auto-route by signal grade:
//!   - Authoritative → eval/reports/authoritative/<benchmark>-<version>.md  (committed)
//!   - Exploratory   → eval/reports/exploratory/<timestamp>-<benchmark>.md  (gitignored)

use clap::{Parser, Subcommand, ValueEnum};
use nevoflux_eval::{benchmarks, browser::BrowserLaunchMode, judge, reporter, Runner, RunnerConfig};
use std::path::PathBuf;
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "nevoflux-eval", about = "NevoFlux evaluation harness")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a benchmark.
    Run {
        #[arg(long)]
        benchmark: String,
        #[arg(long)]
        judge: Option<String>,
        #[arg(long, default_value = "127.0.0.1:19500")]
        daemon_addr: String,
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        filter: Option<String>,

        // Browser mode flags.
        #[arg(long, value_enum, default_value_t = BrowserModeArg::DaemonOnly)]
        browser_mode: BrowserModeArg,
        #[arg(long, default_value = "http://localhost:5959")]
        browser_endpoint: String,
        #[arg(long)]
        browser_version: Option<String>,
        #[arg(long, default_value = ".cache/nevoflux-releases")]
        browser_cache_dir: PathBuf,

        // Report output.
        #[arg(long, default_value = "eval/reports")]
        out_dir: PathBuf,
        #[arg(long, default_value = "eval/reports/trends.json")]
        trends: PathBuf,
    },

    /// List registered benchmarks and judges.
    List,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum BrowserModeArg {
    /// No browser. Tasks with requires_browser=true are skipped.
    DaemonOnly,
    /// Connect to an externally-running dev instance (you started it manually).
    External,
    /// Download and launch a published release binary.
    Release,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nevoflux_eval=debug".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::List => {
            println!("Registered benchmarks:");
            for b in benchmarks::registry() {
                println!(
                    "  {:<20} {} (network: {})",
                    b.name(),
                    b.description(),
                    b.requires_network()
                );
            }
            println!("\nRegistered judges:");
            for j in judge::registry() {
                println!("  {}", j.name());
            }
            println!("\nBrowser modes:");
            println!("  daemon-only  no browser, daemon-only tier (Exploratory grade)");
            println!("  external     connect to running nevoflux dev instance (Exploratory grade)");
            println!("  release      download published release binary (Authoritative grade)");
        }
        Command::Run {
            benchmark,
            judge: judge_name,
            daemon_addr,
            timeout,
            limit,
            filter,
            browser_mode,
            browser_endpoint,
            browser_version,
            browser_cache_dir,
            out_dir,
            trends,
        } => {
            let bench = benchmarks::find(&benchmark).ok_or_else(|| {
                anyhow::anyhow!("benchmark `{}` not found; run `nevoflux-eval list`", benchmark)
            })?;

            let judge_name = judge_name.unwrap_or_else(|| bench.default_judge().to_string());
            let judge = judge::find(&judge_name).ok_or_else(|| {
                anyhow::anyhow!("judge `{}` not found; run `nevoflux-eval list`", judge_name)
            })?;

            let browser = match browser_mode {
                // TODO(Task 16): resolve daemon_binary and state_dir from CLI flags properly.
                // For now, use placeholder paths so the crate compiles; Task 16 replaces this.
                BrowserModeArg::DaemonOnly => BrowserLaunchMode::DaemonOnly {
                    daemon_binary: PathBuf::from("target/release/nevoflux-agent"),
                    state_dir: PathBuf::from(".eval-state"),
                },
                BrowserModeArg::External => BrowserLaunchMode::ExternalDevInstance {
                    endpoint: browser_endpoint,
                },
                BrowserModeArg::Release => {
                    let v = browser_version.ok_or_else(|| {
                        anyhow::anyhow!("--browser-version required for --browser-mode=release")
                    })?;
                    BrowserLaunchMode::ReleaseBinary {
                        version: v,
                        cache_dir: browser_cache_dir,
                    }
                }
            };

            let signal_grade = browser.signal_grade();
            info!(benchmark = %benchmark, judge = %judge_name, grade = ?signal_grade, "starting eval");

            let cfg = RunnerConfig {
                daemon_addr,
                task_timeout_secs: timeout,
                parallelism: 1,
                task_filter: filter,
                limit,
                browser_mode: browser,
            };
            let runner = Runner::new(cfg);
            let summary = runner.run(bench.as_ref(), judge.as_ref()).await?;

            // Route output by signal grade.
            let graded_dir = out_dir.join(summary.signal_grade.subdir());
            let md_path = reporter::write_markdown(&summary, &graded_dir).await?;
            let json_path = reporter::write_json(&summary, &graded_dir).await?;
            reporter::append_trend(&summary, &trends).await?;

            info!(
                accuracy = format!("{:.1}%", summary.accuracy() * 100.0),
                grade = ?summary.signal_grade,
                browser = %summary.browser_version,
                skipped = summary.skipped,
                report = ?md_path,
                json = ?json_path,
                "eval finished"
            );

            // Optional CI threshold check.
            if let Ok(threshold) = std::env::var("EVAL_MIN_ACCURACY") {
                let threshold: f64 = threshold.parse()?;
                if summary.accuracy() < threshold {
                    error!(
                        accuracy = summary.accuracy(),
                        threshold = threshold,
                        "below CI threshold — failing build"
                    );
                    std::process::exit(1);
                }
            }
        }
    }

    Ok(())
}
