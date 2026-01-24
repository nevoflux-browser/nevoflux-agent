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
        println!("Checking daemon status...");
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
}
