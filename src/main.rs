//! NevoFlux Native Agent CLI
//!
//! Entry points:
//! - `nevoflux` - Proxy mode (Native Messaging bridge)
//! - `nevoflux --daemon` - Core daemon
//! - `nevoflux --mcp` - MCP server mode
//! - `nevoflux --status` - Show daemon status
//! - `nevoflux --stop` - Stop daemon
//! - `nevoflux config <action>` - Configuration management
//! - `nevoflux setup` - Interactive setup wizard

mod cli;
mod completions;
mod logging;

use clap::Parser;
use cli::{Cli, Commands, ConfigAction};
use fs2::FileExt;
use nevoflux_storage::Storage;
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
/// and the daemon (via ZeroMQ). Uses full-duplex communication to allow receiving
/// messages (like cancel requests or browser tool responses) while streaming.
async fn run_proxy(verbose: bool) -> Result<(), Box<dyn std::error::Error>> {
    use nevoflux_bridge::{run_async_proxy, AsyncProxyConfig, BridgeConfig};
    use tokio::io::{stdin, stdout};

    // Initialize logging to file only (stdout/stderr must be silent for Native Messaging)
    let log_file = get_data_dir().join("proxy.log");
    logging::init_file_only_logging(log_file, verbose);

    tracing::debug!("Starting async proxy mode (full-duplex)");

    let bridge_config = BridgeConfig::new().with_data_dir(get_data_dir());
    let config = AsyncProxyConfig::new().with_bridge(bridge_config);

    run_async_proxy(stdin(), stdout(), config).await?;

    Ok(())
}

/// Run the MCP server mode.
///
/// This starts an MCP server that communicates via stdio, allowing
/// Claude Code or other MCP clients to use browser automation tools.
async fn run_mcp(verbose: bool) -> Result<(), Box<dyn std::error::Error>> {
    use nevoflux_mcp::{create_tools, run_stdio_server, McpServer, McpServerConfig};

    // Initialize logging to stderr (stdout is for MCP protocol)
    logging::init_stderr_logging(verbose, Some("nevoflux=info"));

    logging::log_startup(env!("CARGO_PKG_VERSION"));
    tracing::info!("Starting MCP server mode");

    // Create server with default configuration
    let config = McpServerConfig::default();
    let mut server = McpServer::with_config(config);

    // Register all tools from the tools module
    for tool in create_tools() {
        server.register_tool(tool);
    }

    // Run the stdio server
    run_stdio_server(server).await?;

    Ok(())
}

/// Run the daemon.
async fn run_daemon(verbose: bool) -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    logging::init_logging(verbose, None);
    logging::log_startup(env!("CARGO_PKG_VERSION"));

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

    // Create session manager with database
    let db_path = get_db_path();
    let session_manager = std::sync::Arc::new(
        nevoflux_daemon::SessionManager::new(db_path.to_str().unwrap_or("nevoflux.db"))
            .expect("Failed to create session manager"),
    );

    // Start server
    let config = nevoflux_daemon::ServerConfig::default();
    let router = std::sync::Arc::new(nevoflux_daemon::Router::new());

    let server = nevoflux_daemon::start_server(config, router, session_manager).await?;
    let port = server.port();

    // Write port/pid files
    write_daemon_files(port)?;

    tracing::info!("Daemon started on port {}", port);

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;

    logging::log_shutdown();

    // Cleanup
    let data_dir = get_data_dir();
    let _ = std::fs::remove_file(data_dir.join("daemon.port"));
    let _ = std::fs::remove_file(data_dir.join("daemon.pid"));

    Ok(())
}

/// Get the database path for storage.
fn get_db_path() -> PathBuf {
    get_data_dir().join("nevoflux.db")
}

/// Open storage, creating if necessary.
fn open_storage() -> Result<Storage, Box<dyn std::error::Error>> {
    let db_path = get_db_path();
    ensure_data_dir()?;
    Ok(Storage::open(&db_path)?)
}

/// Run the config show command.
fn run_config_show() {
    match open_storage() {
        Ok(storage) => match storage.config().list() {
            Ok(entries) => {
                if entries.is_empty() {
                    println!("No configuration entries found.");
                    println!("Run 'nevoflux config init' to initialize default configuration.");
                } else {
                    println!("Configuration:");
                    println!("{}", "-".repeat(60));
                    for entry in entries {
                        let value_str = serde_json::to_string_pretty(&entry.value)
                            .unwrap_or_else(|_| entry.value.to_string());
                        println!("{} = {}", entry.key, value_str);
                    }
                }
            }
            Err(e) => {
                eprintln!("Error listing configuration: {}", e);
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("Error opening storage: {}", e);
            std::process::exit(1);
        }
    }
}

/// Run the config init command.
fn run_config_init() {
    match open_storage() {
        Ok(storage) => {
            let defaults = [
                ("app.name", serde_json::json!("NevoFlux")),
                ("app.version", serde_json::json!(env!("CARGO_PKG_VERSION"))),
                ("daemon.port", serde_json::json!(0)), // 0 means auto-assign
                ("daemon.auto_start", serde_json::json!(true)),
                ("logging.level", serde_json::json!("info")),
                ("logging.file", serde_json::json!(null)),
            ];

            for (key, value) in defaults {
                // Only set if not already exists
                match storage.config().get(key) {
                    Ok(Some(_)) => {
                        println!("  {} (already set, skipping)", key);
                    }
                    Ok(None) => {
                        if let Err(e) = storage.config().set(key, value.clone()) {
                            eprintln!("Error setting {}: {}", key, e);
                        } else {
                            println!("  {} = {}", key, value);
                        }
                    }
                    Err(e) => {
                        eprintln!("Error checking {}: {}", key, e);
                    }
                }
            }
            println!("\nConfiguration initialized.");
        }
        Err(e) => {
            eprintln!("Error opening storage: {}", e);
            std::process::exit(1);
        }
    }
}

/// Run the config get command.
fn run_config_get(key: &str) {
    match open_storage() {
        Ok(storage) => match storage.config().get(key) {
            Ok(Some(value)) => {
                let value_str =
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
                println!("{}", value_str);
            }
            Ok(None) => {
                eprintln!("Key '{}' not found", key);
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("Error getting configuration: {}", e);
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("Error opening storage: {}", e);
            std::process::exit(1);
        }
    }
}

/// Run the config set command.
fn run_config_set(key: &str, value: &str) {
    match open_storage() {
        Ok(storage) => {
            // Try to parse as JSON, fall back to string
            let json_value: serde_json::Value = serde_json::from_str(value)
                .unwrap_or_else(|_| serde_json::Value::String(value.to_string()));

            match storage.config().set(key, json_value.clone()) {
                Ok(()) => {
                    println!("Set {} = {}", key, json_value);
                }
                Err(e) => {
                    eprintln!("Error setting configuration: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("Error opening storage: {}", e);
            std::process::exit(1);
        }
    }
}

/// Run the config delete command.
fn run_config_delete(key: &str) {
    match open_storage() {
        Ok(storage) => match storage.config().delete(key) {
            Ok(true) => {
                println!("Deleted '{}'", key);
            }
            Ok(false) => {
                eprintln!("Key '{}' not found", key);
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("Error deleting configuration: {}", e);
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("Error opening storage: {}", e);
            std::process::exit(1);
        }
    }
}

/// Run the config list command.
fn run_config_list(prefix: &str) {
    match open_storage() {
        Ok(storage) => {
            let result = if prefix.is_empty() {
                storage.config().list()
            } else {
                storage.config().list_by_prefix(prefix)
            };

            match result {
                Ok(entries) => {
                    if entries.is_empty() {
                        if prefix.is_empty() {
                            println!("No configuration entries found.");
                        } else {
                            println!("No configuration entries found with prefix '{}'", prefix);
                        }
                    } else {
                        for entry in entries {
                            let value_str = serde_json::to_string(&entry.value)
                                .unwrap_or_else(|_| entry.value.to_string());
                            println!("{} = {}", entry.key, value_str);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error listing configuration: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("Error opening storage: {}", e);
            std::process::exit(1);
        }
    }
}

/// Run the setup wizard.
fn run_setup() {
    println!("NevoFlux Setup Wizard");
    println!("{}", "=".repeat(40));
    println!();

    // Initialize default configuration
    println!("Initializing default configuration...");
    run_config_init();

    println!();
    println!("Setup complete!");
    println!();
    println!("Next steps:");
    println!("  1. Start the daemon: nevoflux --daemon");
    println!("  2. Check status: nevoflux --status");
    println!("  3. View configuration: nevoflux config show");
}

/// Handle config subcommands.
fn handle_config_command(action: ConfigAction) {
    match action {
        ConfigAction::Show => run_config_show(),
        ConfigAction::Init => run_config_init(),
        ConfigAction::Get { key } => run_config_get(&key),
        ConfigAction::Set { key, value } => run_config_set(&key, &value),
        ConfigAction::Delete { key } => run_config_delete(&key),
        ConfigAction::List { prefix } => run_config_list(&prefix),
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Handle subcommands first (they don't require async)
    if let Some(command) = cli.command {
        match command {
            Commands::Config { action } => {
                handle_config_command(action);
                return;
            }
            Commands::Setup => {
                run_setup();
                return;
            }
            Commands::Completions { shell } => {
                completions::generate_completions(shell);
                return;
            }
            Commands::External(_) => {
                // Firefox passes manifest path and extension ID as arguments
                // Ignore them and continue to proxy mode (default behavior)
            }
        }
    }

    // Handle flags
    if cli.daemon {
        if let Err(e) = run_daemon(cli.verbose).await {
            eprintln!("Daemon error: {}", e);
            std::process::exit(1);
        }
    } else if cli.mcp {
        if let Err(e) = run_mcp(cli.verbose).await {
            eprintln!("MCP server error: {}", e);
            std::process::exit(1);
        }
    } else if cli.status {
        run_status();
    } else if cli.stop {
        run_stop();
    } else if let Err(e) = run_proxy(cli.verbose).await {
        eprintln!("Proxy error: {}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Mutex to serialize tests that modify NEVOFLUX_DATA_DIR environment variable.
    // This prevents race conditions when tests run in parallel.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn test_get_data_dir_returns_path() {
        let dir = get_data_dir();
        assert!(dir.exists() || dir.parent().map(|p| p.exists()).unwrap_or(false));
    }

    #[test]
    fn test_port_file_path() {
        let _guard = ENV_MUTEX.lock().unwrap();

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
        let _guard = ENV_MUTEX.lock().unwrap();

        let temp = tempfile::TempDir::new().unwrap();
        std::env::set_var("NEVOFLUX_DATA_DIR", temp.path());

        let status = check_daemon_status();
        assert!(matches!(status, DaemonStatus::NotRunning));

        std::env::remove_var("NEVOFLUX_DATA_DIR");
    }

    #[test]
    fn test_check_daemon_status_with_port_file() {
        let _guard = ENV_MUTEX.lock().unwrap();

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
        let _guard = ENV_MUTEX.lock().unwrap();

        let temp = tempfile::TempDir::new().unwrap();
        std::env::set_var("NEVOFLUX_DATA_DIR", temp.path());

        let result = stop_daemon();
        assert!(result.is_ok());

        std::env::remove_var("NEVOFLUX_DATA_DIR");
    }

    #[test]
    fn test_stop_daemon_cleans_files() {
        let _guard = ENV_MUTEX.lock().unwrap();

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
