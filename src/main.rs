//! NevoFlux Native Agent CLI
//!
//! Entry points:
//! - `nevoflux` - Proxy mode (Native Messaging bridge)
//! - `nevoflux --daemon` - Core daemon
//! - `nevoflux --mcp` - MCP server mode
//! - `nevoflux --status` - Show daemon status
//! - `nevoflux --stop` - Stop daemon

use clap::Parser;
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

fn main() {
    let args = Args::parse();

    // For now, just print which mode was selected
    if args.daemon {
        println!("Starting daemon mode...");
    } else if args.mcp {
        println!("Starting MCP server mode...");
    } else if args.status {
        run_status();
    } else if args.stop {
        println!("Stopping daemon...");
    } else {
        println!("Starting proxy mode...");
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
        let dir = get_data_dir();
        let port_file = dir.join("daemon.port");
        assert!(port_file.to_string_lossy().contains("nevoflux"));
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
}
