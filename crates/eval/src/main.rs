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
use nevoflux_eval::{
    benchmarks, browser::BrowserLaunchMode, judge, reporter, Runner, RunnerConfig,
};
use std::path::PathBuf;
use tracing::info;

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
            judge,
            daemon_addr: _,
            timeout,
            limit,
            filter,
            browser_mode,
            browser_endpoint,
            browser_version,
            browser_cache_dir,
            out_dir,
            trends: _,
        } => {
            // Resolve daemon binary path.
            // Priority: NEVOFLUX_EVAL_DAEMON_BINARY env var → sibling of current_exe.
            let daemon_binary: PathBuf = std::env::var_os("NEVOFLUX_EVAL_DAEMON_BINARY")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    // current_exe lands in target/<profile>/nevoflux-eval;
                    // the daemon binary is a sibling: target/<profile>/nevoflux-agent.
                    let mut p = std::env::current_exe().expect("current_exe");
                    p.pop(); // strip exec name — now at target/<profile>/
                    p.push("nevoflux-agent");
                    p
                });

            // Resolve state dir.
            // Priority: NEVOFLUX_EVAL_STATE_DIR env var → OS data-local dir → .cache fallback.
            let state_dir: PathBuf = std::env::var_os("NEVOFLUX_EVAL_STATE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    directories::ProjectDirs::from("com", "nevoflux", "nevoflux-eval")
                        .map(|d| d.data_local_dir().to_path_buf())
                        .unwrap_or_else(|| PathBuf::from(".cache/nevoflux-eval"))
                });

            let mode = match browser_mode {
                BrowserModeArg::DaemonOnly => BrowserLaunchMode::DaemonOnly {
                    daemon_binary: daemon_binary.clone(),
                    state_dir: state_dir.clone(),
                },
                BrowserModeArg::External => BrowserLaunchMode::ExternalDevInstance {
                    endpoint: browser_endpoint,
                },
                BrowserModeArg::Release => BrowserLaunchMode::ReleaseBinary {
                    version: browser_version.ok_or_else(|| {
                        anyhow::anyhow!("--browser-version required for --browser-mode=release")
                    })?,
                    cache_dir: browser_cache_dir,
                },
            };

            let browser = nevoflux_eval::browser::launch(&mode).await?;
            browser.ensure_ready().await?;

            let client = browser
                .lock()
                .map(nevoflux_eval::daemon_client::DaemonHttpClient::from_lock)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "this browser mode doesn't expose a daemon lock; \
                         non-daemon-only modes are stubbed for Phase 2 — see Task 9"
                    )
                })?;

            let cfg = RunnerConfig {
                daemon_addr: String::new(), // unused — HTTP discovery via lock
                task_timeout_secs: timeout,
                parallelism: 1,
                task_filter: filter,
                limit,
                browser_mode: mode.clone(),
            };
            let runner = Runner::with_client(cfg, client);

            let bench = benchmarks::find(&benchmark)
                .ok_or_else(|| anyhow::anyhow!("benchmark not found: {benchmark}"))?;
            let judge_name = judge.unwrap_or_else(|| bench.default_judge().to_string());
            let judge_inst = judge::find(&judge_name)
                .ok_or_else(|| anyhow::anyhow!("judge not found: {judge_name}"))?;

            info!(
                benchmark = %benchmark,
                judge = %judge_name,
                "starting eval run"
            );
            let summary = runner.run(&*bench, &*judge_inst).await?;

            let grade_dir = out_dir.join(summary.signal_grade.subdir());
            let report_path = reporter::write_markdown(&summary, &grade_dir).await?;
            info!(path = ?report_path, "report written");

            browser.shutdown().await?;
        }
    }

    Ok(())
}
