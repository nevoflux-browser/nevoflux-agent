//! NevoFlux Native Agent CLI
//!
//! Entry points:
//! - `nevoflux` - Proxy mode (Native Messaging bridge)
//! - `nevoflux --daemon` - Core daemon
//! - `nevoflux --mcp` - MCP server mode
//! - `nevoflux --status` - Show daemon status
//! - `nevoflux --stop` - Stop daemon

use clap::Parser;

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
