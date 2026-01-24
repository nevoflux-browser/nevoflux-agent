//! NevoFlux Native Agent CLI
//!
//! Entry points:
//! - `nevoflux` - Proxy mode (Native Messaging bridge)
//! - `nevoflux --daemon` - Core daemon
//! - `nevoflux --mcp` - MCP server mode
//! - `nevoflux --status` - Show daemon status
//! - `nevoflux --stop` - Stop daemon

use clap::Parser;
use fs2::FileExt;
use std::fs::File;
use std::path::PathBuf;

/// Get the data directory for NevoFlux.
///
/// Platform-specific locations:
/// - Linux: ~/.local/share/nevoflux/
/// - macOS: ~/Library/Application Support/nevoflux/
/// - Windows: %APPDATA%\nevoflux\
fn get_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("NEVOFLUX_DATA_DIR") {
        return PathBuf::from(dir);
    }

    directories::ProjectDirs::from("com", "nevoflux", "nevoflux")
        .map(|dirs| dirs.data_dir().to_path_buf())
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".nevoflux")
        })
}

/// Ensure the data directory exists.
#[allow(dead_code)]
fn ensure_data_dir() -> std::io::Result<PathBuf> {
    let dir = get_data_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Daemon status result.
#[derive(Debug)]
enum DaemonStatus {
    /// Daemon is running.
    Running { port: u16, pid: u32 },
    /// Daemon files exist but process is not running.
    Stale { port: u16, pid: u32 },
    /// Daemon is not running.
    NotRunning,
}

/// Check if the daemon is running.
fn check_daemon_status() -> DaemonStatus {
    let data_dir = get_data_dir();
    let port_file = data_dir.join("daemon.port");
    let pid_file = data_dir.join("daemon.pid");

    // Read port file
    let port = match std::fs::read_to_string(&port_file) {
        Ok(s) => match s.trim().parse::<u16>() {
            Ok(p) => p,
            Err(_) => return DaemonStatus::NotRunning,
        },
        Err(_) => return DaemonStatus::NotRunning,
    };

    // Read PID file
    let pid = match std::fs::read_to_string(&pid_file) {
        Ok(s) => match s.trim().parse::<u32>() {
            Ok(p) => p,
            Err(_) => return DaemonStatus::Stale { port, pid: 0 },
        },
        Err(_) => return DaemonStatus::Stale { port, pid: 0 },
    };

    // Check if process is running
    if is_process_running(pid) {
        DaemonStatus::Running { port, pid }
    } else {
        DaemonStatus::Stale { port, pid }
    }
}

/// Check if a process is running.
#[cfg(unix)]
fn is_process_running(pid: u32) -> bool {
    use std::process::Command;
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_process_running(pid: u32) -> bool {
    use std::process::Command;
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid)])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}

/// Run the status command.
fn run_status() {
    match check_daemon_status() {
        DaemonStatus::Running { port, pid } => {
            println!("Daemon is running");
            println!("  port: {}", port);
            println!("  pid: {}", pid);
        }
        DaemonStatus::Stale { port, pid } => {
            println!("Daemon is not running (stale files found)");
            println!("  port file: {}", port);
            println!("  pid file: {}", pid);
            println!("Run 'nevoflux --stop' to clean up.");
        }
        DaemonStatus::NotRunning => {
            println!("Daemon is not running");
        }
    }
}

/// Stop the running daemon.
fn stop_daemon() -> std::io::Result<()> {
    let data_dir = get_data_dir();
    let port_file = data_dir.join("daemon.port");
    let pid_file = data_dir.join("daemon.pid");
    let lock_file = data_dir.join("daemon.lock");

    // Try to read PID and send SIGTERM
    if let Ok(pid_str) = std::fs::read_to_string(&pid_file) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            if is_process_running(pid) {
                #[cfg(unix)]
                {
                    use std::process::Command;
                    let _ = Command::new("kill")
                        .args(["-TERM", &pid.to_string()])
                        .output();

                    // Wait briefly for graceful shutdown
                    std::thread::sleep(std::time::Duration::from_millis(500));

                    // Force kill if still running
                    if is_process_running(pid) {
                        let _ = Command::new("kill")
                            .args(["-KILL", &pid.to_string()])
                            .output();
                    }
                }
                #[cfg(windows)]
                {
                    use std::process::Command;
                    let _ = Command::new("taskkill")
                        .args(["/F", "/PID", &pid.to_string()])
                        .output();
                }
            }
        }
    }

    // Clean up files
    let _ = std::fs::remove_file(&port_file);
    let _ = std::fs::remove_file(&pid_file);
    let _ = std::fs::remove_file(&lock_file);

    Ok(())
}

/// Run the stop command.
fn run_stop() {
    match check_daemon_status() {
        DaemonStatus::Running { port: _, pid } => {
            println!("Stopping daemon (PID {})...", pid);
            if let Err(e) = stop_daemon() {
                eprintln!("Error stopping daemon: {}", e);
                std::process::exit(1);
            }
            println!("Daemon stopped.");
        }
        DaemonStatus::Stale { .. } => {
            println!("Cleaning up stale daemon files...");
            if let Err(e) = stop_daemon() {
                eprintln!("Error cleaning up: {}", e);
                std::process::exit(1);
            }
            println!("Cleanup complete.");
        }
        DaemonStatus::NotRunning => {
            println!("Daemon is not running.");
        }
    }
}

/// Acquire the daemon lock file.
fn acquire_daemon_lock() -> std::io::Result<File> {
    let data_dir = ensure_data_dir()?;
    let lock_path = data_dir.join("daemon.lock");

    let file = File::create(&lock_path)?;
    file.try_lock_exclusive().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "Daemon is already running",
        )
    })?;

    Ok(file)
}

/// Write daemon port and PID files.
fn write_daemon_files(port: u16) -> std::io::Result<()> {
    let data_dir = get_data_dir();

    std::fs::write(data_dir.join("daemon.port"), port.to_string())?;
    std::fs::write(data_dir.join("daemon.pid"), std::process::id().to_string())?;

    Ok(())
}

/// Run in proxy mode (Native Messaging bridge).
///
/// This bridges between the browser extension (via Native Messaging on stdin/stdout)
/// and the daemon (via ZeroMQ). Messages from the browser are forwarded to the daemon,
/// and responses from the daemon are forwarded back to the browser.
async fn run_proxy() -> Result<(), Box<dyn std::error::Error>> {
    use nevoflux_bridge::{parse_native_message, BridgeConfig, Proxy, ProxyConfig};
    use tokio::io::{stdin, stdout};

    // Initialize logging to stderr (stdout is for Native Messaging)
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("nevoflux=debug".parse().unwrap()),
        )
        .init();

    tracing::debug!("Starting proxy mode");

    let bridge_config = BridgeConfig::new().with_data_dir(get_data_dir());
    let proxy_config = ProxyConfig::new().with_bridge(bridge_config);

    let mut proxy = Proxy::new(stdin(), stdout(), proxy_config);

    // Connect to daemon (will auto-start if not running)
    proxy.connect().await?;

    tracing::info!("Proxy connected to daemon");

    // Main event loop
    // Process messages from Native Messaging (browser) and forward to daemon.
    // After forwarding, wait for and forward the daemon's response back.
    loop {
        // Read from Native Messaging (browser)
        let message = match proxy.read_native_message().await {
            Ok(msg) => msg,
            Err(e) => {
                tracing::debug!("Native messaging read error: {}", e);
                break; // Browser closed connection
            }
        };

        // Parse and forward to daemon
        if let Some((request_id, channel, payload)) = parse_native_message(&message) {
            if let Err(e) = proxy.forward_to_daemon(request_id, channel, payload).await {
                tracing::error!("Failed to forward to daemon: {}", e);
                proxy.send_error("DAEMON_ERROR", &e.to_string()).await.ok();
                continue;
            }

            // Wait for daemon response and forward to sidebar
            match proxy.receive_from_daemon().await {
                Ok(env) => {
                    if let Err(e) = proxy.forward_to_sidebar(env).await {
                        tracing::error!("Failed to forward to sidebar: {}", e);
                    }
                }
                Err(e) => {
                    tracing::error!("Daemon receive error: {}", e);
                    proxy.send_error("DAEMON_ERROR", &e.to_string()).await.ok();
                }
            }
        }
    }

    proxy.shutdown().await?;
    Ok(())
}

/// Run the daemon.
async fn run_daemon() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("nevoflux=info".parse().unwrap()),
        )
        .init();

    // Acquire lock
    let _lock = match acquire_daemon_lock() {
        Ok(lock) => lock,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            eprintln!("Error: Daemon is already running");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error acquiring lock: {}", e);
            std::process::exit(1);
        }
    };

    // Start server
    let config = nevoflux_daemon::ServerConfig::default();
    let router = std::sync::Arc::new(nevoflux_daemon::Router::new());

    let server = nevoflux_daemon::start_server(config, router).await?;
    let port = server.port();

    // Write port/pid files
    write_daemon_files(port)?;

    tracing::info!("Daemon started on port {}", port);

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;

    tracing::info!("Shutting down...");

    // Cleanup
    let data_dir = get_data_dir();
    let _ = std::fs::remove_file(data_dir.join("daemon.port"));
    let _ = std::fs::remove_file(data_dir.join("daemon.pid"));

    Ok(())
}

/// NevoFlux Native Agent - AI-powered browser assistant
#[derive(Parser, Debug)]
#[command(name = "nevoflux")]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Run as daemon (core processing)
    #[arg(long)]
    daemon: bool,

    /// Run as MCP server (stdio bridge)
    #[arg(long)]
    mcp: bool,

    /// Show daemon status
    #[arg(long)]
    status: bool,

    /// Stop running daemon
    #[arg(long)]
    stop: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if args.daemon {
        if let Err(e) = run_daemon().await {
            eprintln!("Daemon error: {}", e);
            std::process::exit(1);
        }
    } else if args.mcp {
        println!("Starting MCP server mode...");
        // TODO: Implement MCP mode
    } else if args.status {
        run_status();
    } else if args.stop {
        run_stop();
    } else if let Err(e) = run_proxy().await {
        eprintln!("Proxy error: {}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_data_dir_returns_path() {
        let dir = get_data_dir();
        assert!(dir.exists() || dir.parent().map(|p| p.exists()).unwrap_or(false));
    }

    #[test]
    fn test_port_file_path() {
        // Use temp dir that contains "nevoflux" in path name for test
        let temp = tempfile::Builder::new()
            .prefix("nevoflux-test")
            .tempdir()
            .unwrap();
        std::env::set_var("NEVOFLUX_DATA_DIR", temp.path());

        let dir = get_data_dir();
        let port_file = dir.join("daemon.port");
        assert!(port_file.to_string_lossy().contains("nevoflux"));

        std::env::remove_var("NEVOFLUX_DATA_DIR");
    }

    #[test]
    fn test_check_daemon_status_not_running() {
        let temp = tempfile::TempDir::new().unwrap();
        std::env::set_var("NEVOFLUX_DATA_DIR", temp.path());

        let status = check_daemon_status();
        assert!(matches!(status, DaemonStatus::NotRunning));

        std::env::remove_var("NEVOFLUX_DATA_DIR");
    }

    #[test]
    fn test_check_daemon_status_with_port_file() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("daemon.port"), "19500").unwrap();
        std::fs::write(temp.path().join("daemon.pid"), "12345").unwrap();
        std::env::set_var("NEVOFLUX_DATA_DIR", temp.path());

        let status = check_daemon_status();
        // Should be Stale since PID 12345 is not running
        assert!(matches!(status, DaemonStatus::Stale { .. }));

        std::env::remove_var("NEVOFLUX_DATA_DIR");
    }

    #[test]
    fn test_stop_daemon_no_files() {
        let temp = tempfile::TempDir::new().unwrap();
        std::env::set_var("NEVOFLUX_DATA_DIR", temp.path());

        let result = stop_daemon();
        assert!(result.is_ok());

        std::env::remove_var("NEVOFLUX_DATA_DIR");
    }

    #[test]
    fn test_stop_daemon_cleans_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let port_file = temp.path().join("daemon.port");
        let pid_file = temp.path().join("daemon.pid");
        std::fs::write(&port_file, "19500").unwrap();
        std::fs::write(&pid_file, "99999").unwrap(); // Non-existent PID

        std::env::set_var("NEVOFLUX_DATA_DIR", temp.path());

        let result = stop_daemon();
        assert!(result.is_ok());
        assert!(!port_file.exists());
        assert!(!pid_file.exists());

        std::env::remove_var("NEVOFLUX_DATA_DIR");
    }

    #[test]
    fn test_acquire_daemon_lock_succeeds() {
        let temp = tempfile::TempDir::new().unwrap();
        std::env::set_var("NEVOFLUX_DATA_DIR", temp.path());

        let lock = acquire_daemon_lock();
        assert!(lock.is_ok());

        std::env::remove_var("NEVOFLUX_DATA_DIR");
    }
}
