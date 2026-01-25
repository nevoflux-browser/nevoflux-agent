# NevoFlux Agent

AI-powered browser assistant with native computer control capabilities.

## Features

- **Browser Automation**: Control browser tabs via extension with navigate, click, type, and screenshot capabilities
- **Computer Control**: Screenshots, mouse, keyboard support for Linux (X11), macOS, and Windows
- **MCP Integration**: Works with Claude Code via Model Context Protocol (stdio transport)
- **Native Messaging**: Secure bridge between browser extension and daemon via ZeroMQ
- **LLM Integration**: Supports Qwen provider with extensible provider system via rig-core
- **WASM Skills**: Extensible skill system using WebAssembly modules
- **Session Management**: Persistent session storage with SQLite backend

## Installation

### Prerequisites

- Rust 1.75+ (for building from source)
- Linux: X11 display server (Wayland not yet supported)
- macOS: Accessibility permissions required for computer control
- Windows: Administrator privileges for native messaging host installation

### Build from Source

```bash
git clone https://github.com/nevoflux/nevoflux-agent
cd nevoflux-agent
cargo build --release
```

The binary will be available at `target/release/nevoflux`.

### Install Native Messaging Host

```bash
# Linux (Chrome/Chromium)
mkdir -p ~/.config/chromium/NativeMessagingHosts
cp install/native-host/com.nevoflux.agent.json ~/.config/chromium/NativeMessagingHosts/

# macOS
mkdir -p ~/Library/Application\ Support/Google/Chrome/NativeMessagingHosts
cp install/native-host/com.nevoflux.agent.json ~/Library/Application\ Support/Google/Chrome/NativeMessagingHosts/

# Windows (PowerShell as Administrator)
# Registry key will be set automatically by setup script
```

## Usage

### Start Daemon

The daemon is the core processing server that handles requests from the MCP bridge and browser extension.

```bash
nevoflux --daemon
```

With verbose logging:

```bash
nevoflux --daemon --verbose
```

### Check Status

```bash
nevoflux --status
```

### Stop Daemon

```bash
nevoflux --stop
```

### MCP Mode (for Claude Code)

Run as an MCP server for integration with Claude Code:

```bash
nevoflux --mcp
```

Add to your Claude Code MCP configuration:

```json
{
  "mcpServers": {
    "nevoflux": {
      "command": "/path/to/nevoflux",
      "args": ["--mcp"]
    }
  }
}
```

### Configuration Management

Initialize default configuration:

```bash
nevoflux config init
```

Show current configuration:

```bash
nevoflux config show
```

Get a specific value:

```bash
nevoflux config get llm.default_provider
```

Set a configuration value:

```bash
nevoflux config set llm.default_provider qwen
nevoflux config set llm.max_tokens 8192
```

List configuration values by prefix:

```bash
nevoflux config list daemon.
```

Delete a configuration value:

```bash
nevoflux config delete llm.default_model
```

### Interactive Setup

Run the setup wizard for guided configuration:

```bash
nevoflux setup
```

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        NevoFlux Agent                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ┌───────────┐    ┌───────────┐    ┌───────────────────────┐   │
│  │  Claude   │    │  Browser  │    │      Main Binary      │   │
│  │   Code    │    │ Extension │    │      (nevoflux)       │   │
│  └─────┬─────┘    └─────┬─────┘    └───────────┬───────────┘   │
│        │                │                      │               │
│        │ stdio          │ Native               │               │
│        │                │ Messaging            │               │
│        ▼                ▼                      ▼               │
│  ┌───────────┐    ┌───────────┐    ┌───────────────────────┐   │
│  │    MCP    │    │  Bridge   │    │       Daemon          │   │
│  │  Server   │◄──►│  (ZeroMQ) │◄──►│    (Core Engine)      │   │
│  └───────────┘    └───────────┘    └───────────┬───────────┘   │
│                                                │               │
│                                    ┌───────────┴───────────┐   │
│                                    │                       │   │
│                              ┌─────▼─────┐  ┌─────────────┐   │
│                              │   WASM    │  │   Computer  │   │
│                              │  Skills   │  │   Control   │   │
│                              └───────────┘  └─────────────┘   │
│                                                               │
│  ┌───────────────────────────────────────────────────────┐   │
│  │                    Shared Crates                       │   │
│  │  ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────────┐  │   │
│  │  │Protocol │ │ Storage │ │   LLM   │ │   Testing   │  │   │
│  │  └─────────┘ └─────────┘ └─────────┘ └─────────────┘  │   │
│  └───────────────────────────────────────────────────────┘   │
│                                                               │
└───────────────────────────────────────────────────────────────┘
```

### Crate Structure

| Crate | Description |
|-------|-------------|
| `nevoflux` | Main binary with CLI interface |
| `nevoflux-daemon` | Core daemon with session management, WASM runtime, and agent execution |
| `nevoflux-mcp` | MCP server/client implementation with stdio transport |
| `nevoflux-bridge` | Native messaging bridge between browser and daemon |
| `nevoflux-protocol` | Shared message types and serialization |
| `nevoflux-storage` | SQLite-based persistent storage |
| `nevoflux-llm` | LLM provider abstraction (Qwen, extensible) |
| `nevoflux-computer` | Cross-platform computer control (screenshot, mouse, keyboard) |
| `nevoflux-skills` | WASM skill loading and management |
| `nevoflux-builtin-wasm` | Built-in WASM skill modules |
| `nevoflux-testing` | Testing utilities and mocks |

## MCP Tools

The following tools are exposed via the MCP protocol:

### Browser Tools

| Tool | Description | Parameters |
|------|-------------|------------|
| `browser_navigate` | Navigate to URL | `url` (required) |
| `browser_click` | Click element by CSS selector | `selector` (required) |
| `browser_screenshot` | Capture page screenshot | `full_page` (optional, default: false) |
| `browser_type` | Type text into element | `selector`, `text` (required), `clear` (optional) |

### Agent Tools

| Tool | Description | Parameters |
|------|-------------|------------|
| `agent_chat` | Send message to AI agent | `message` (required), `context` (optional) |

### Computer Tools

| Tool | Description | Parameters |
|------|-------------|------------|
| `computer_screenshot` | Capture screen | `monitor` (optional, 0-based index) |
| `computer_mouse_move` | Move cursor and optionally click | `x`, `y` (required), `click` (optional: left/right/middle/double) |
| `computer_type_text` | Type text at cursor | `text` (required), `delay_ms` (optional) |

### Built-in Agent Tools

| Tool | Description |
|------|-------------|
| `read_file` | Read file contents |
| `write_file` | Write content to file |
| `list_files` | List directory contents |

## Configuration

Configuration file location: `~/.config/nevoflux/config.toml`

### Example Configuration

```toml
[daemon]
port_range_start = 19500
port_range_end = 19600
idle_timeout_secs = 1800
heartbeat_timeout_secs = 30
heartbeat_interval_secs = 10
max_concurrent_requests = 100
keep_alive_for_mcp = true

[daemon.session]
max_sessions = 500
inactive_days = 90
max_storage_mb = 500
auto_create = true

[daemon.context]
system_prompt_reserve = 2000
safety_margin = 500
max_history_messages = 50
include_memory = true
include_current_page = true

[llm]
default_provider = "qwen"
default_model = "qwen-max"
max_tokens = 4096
temperature = 0.7
timeout_secs = 120
max_retries = 3

[storage]
max_size_mb = 1024
wal_mode = true
vacuum_on_startup = false

[logging]
level = "info"
stdout = true
json_format = false
```

### Environment Variables

LLM providers may require API keys via environment variables:

```bash
export QWEN_API_KEY="your-api-key"
export DASHSCOPE_API_KEY="your-api-key"  # Alternative for Qwen
```

## Development

### Run Tests

```bash
# Run all tests
cargo test --workspace

# Run tests for a specific crate
cargo test -p nevoflux-daemon

# Run with verbose output
cargo test --workspace -- --nocapture
```

### Linting

```bash
cargo clippy --workspace
```

### Formatting

```bash
# Check formatting
cargo fmt --check

# Apply formatting
cargo fmt
```

### Generate Documentation

```bash
cargo doc --workspace --open
```

### Build for Release

```bash
cargo build --release --workspace
```

## Platform Notes

### Linux

- Requires X11 display server for computer control features
- Wayland support is not yet implemented
- Install `xdotool` for enhanced input simulation (optional)

### macOS

- Requires Accessibility permissions for mouse/keyboard control
- Grant permissions via System Preferences > Security & Privacy > Privacy > Accessibility

### Windows

- Administrator privileges required for native messaging host registration
- Uses Windows API for screen capture and input simulation

## License

AGPL-3.0
