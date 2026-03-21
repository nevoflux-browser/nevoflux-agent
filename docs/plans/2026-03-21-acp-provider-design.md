# ACP Provider Design: Replace CLI Providers with ACP Protocol

## Background

The current `claude-code` and `gemini-cli` providers spawn CLI tools as subprocesses and communicate via command-line arguments + NDJSON stdin/stdout. This approach has critical issues on Windows:

1. **Rust CVE-2024-24576**: Rust 1.77+ rejects arguments containing special characters (`<`, `>`, `%`, `&`, etc.) when the target is a `.cmd`/`.bat` file. Our system prompt (~9680 chars) contains XML tags that trigger this rejection.
2. **cmd.exe 8191-char limit**: Even bypassing the CVE check, `cmd.exe`'s command line limit is too small for our system prompt.
3. **OAuth authentication**: Direct `node.exe` invocation (bypassing `.cmd`) breaks OAuth token discovery.

## Solution

Replace both CLI providers with ACP (Agent Communication Protocol) based implementations, referencing [Goose's ACP architecture](https://github.com/block/goose). ACP communicates entirely via stdin/stdout binary frames, eliminating all command-line argument issues.

## Architecture

```
WASM agent (manages conversation history, builds system prompt)
  |-- provider=anthropic/openai/...   --> stream_rig_completion()  [unchanged]
  |-- provider=kimi-agent             --> stream_kimi_agent()      [unchanged]
  |-- provider=claude-code            --> stream_acp_completion()  [NEW - internally uses claude-acp]
  |-- provider=gemini-cli             --> stream_acp_completion()  [NEW - internally uses gemini-acp]
```

### Configuration Compatibility

Config keys remain unchanged to minimize modification scope:

```toml
[llm.claude_code]   # internally routes to claude-acp
active = true

[llm.gemini_cli]    # internally routes to gemini-acp
active = true
model = "gemini-2.5-pro"
```

`ProviderType::ClaudeCode` and `ProviderType::GeminiCli` enum values, config parsing, and factory routing remain identical. Only the internal provider implementation changes.

## Code Structure

```
crates/llm/src/providers/
  acp/
    mod.rs          # AcpProvider core (process mgmt, sacp comm, session lifecycle)
    claude.rs       # Claude ACP config (binary name, mode mapping, env vars)
    gemini.rs       # Gemini ACP config (binary name, --acp flag, env vars)
    context.rs      # Conversation context compression (token-budget based)
  claude_code/      # DELETE
  gemini_cli/       # DELETE
  kimi_agent/       # unchanged
  ...

crates/daemon/src/wasm/llm.rs
  stream_acp_completion()  # NEW - bridges AcpProvider to LlmStreamChunk
```

## ACP Provider Design

### AcpProviderConfig

Per-agent configuration, similar to Goose's `AcpProviderConfig`:

```rust
pub struct AcpProviderConfig {
    pub command: PathBuf,           // Resolved binary path (via `which` crate)
    pub args: Vec<String>,          // CLI args (e.g., ["--acp"] for gemini)
    pub env: Vec<(String, String)>, // Environment variables to set
    pub env_remove: Vec<String>,    // Environment variables to remove
    pub work_dir: PathBuf,          // Working directory for the subprocess
    pub session_mode: String,       // ACP session mode (e.g., "plan")
}
```

### AcpProvider

Core struct managing the ACP process lifecycle. Uses channel-based architecture
(matching Goose's pattern) for thread safety — the actual `ClientToAgent` is
owned by a background tokio task, while `AcpProvider` holds a `Sender` for
requests:

```rust
pub struct AcpProvider {
    config: AcpProviderConfig,
    tx: mpsc::Sender<ClientRequest>,  // Channel to background client loop
}

enum ClientRequest {
    NewSession {
        response_tx: oneshot::Sender<Result<SessionId>>,
    },
    Prompt {
        session_id: SessionId,
        content: Vec<ContentBlock>,
        response_tx: mpsc::Sender<AcpUpdate>,
    },
    Shutdown,
}

enum AcpUpdate {
    Text(String),
    Thought(String),
    Complete(StopReason),
    Error(String),
}
```

Key methods:

- `connect(config) -> Result<Self>`: Spawn ACP process, create sacp `ByteStreams` transport (requires `tokio-util` compat adapters), build `ClientToAgent` via builder pattern with notification/request handlers, send `InitializeRequest`, spawn background client loop
- `new_session() -> Result<SessionId>`: Send `NewSessionRequest` via channel, then `SetSessionModeRequest` to set plan mode (two-step process)
- `prompt(session_id, content) -> Receiver<AcpUpdate>`: Send `PromptRequest` via channel, stream `AcpUpdate` events from notification handlers
- `shutdown()`: Send `Shutdown` via channel, background task exits, process terminates
- `is_alive() -> bool`: Check if the background task and process are still running

### ClientToAgent Builder Pattern

The sacp `ClientToAgent` requires a builder with notification and request
handlers registered before connecting:

```rust
let client = ClientToAgent::builder()
    .on_receive_notification(move |notification| {
        // Handle AgentMessageChunk -> AcpUpdate::Text
        // Handle AgentThoughtChunk -> AcpUpdate::Thought
        // Handle ToolCall events (ignored in plan mode)
    })
    .on_receive_request(move |request| {
        // Handle RequestPermissionRequest (auto-reject in plan mode)
    })
    .connect_to(transport)?
    .run_until(handle_requests);
```

### Claude ACP Config

```rust
const CLAUDE_ACP_BINARY: &str = "claude-agent-acp";

fn build_config() -> AcpProviderConfig {
    AcpProviderConfig {
        command: resolve_via_which(CLAUDE_ACP_BINARY),
        args: vec![],
        env: vec![],
        env_remove: vec!["CLAUDECODE".to_string()],
        work_dir: current_workspace_dir(),
        session_mode: "plan".to_string(),
    }
}
```

Available modes (for future use): `bypassPermissions`, `default`, `acceptEdits`, `plan`.

### Gemini ACP Config

```rust
fn build_config(model: &str) -> AcpProviderConfig {
    let mut args = vec!["--acp".to_string()];
    if model != "default" {
        args.extend(["--model".to_string(), model.to_string()]);
    }
    AcpProviderConfig {
        command: resolve_via_which("gemini"),
        args,
        env: vec![],
        env_remove: vec![],
        work_dir: current_workspace_dir(),
        session_mode: "plan".to_string(),
    }
}
```

Available modes (for future use): `yolo`, `default`, `auto_edit`, `plan`.

## Session Management

### Process Lifecycle

```
daemon start
  --> AcpProvider stored in DaemonHostFunctions (or global registry)
  --> ACP process spawned lazily on first request
  --> InitializeRequest / InitializeResponse
  --> background client loop running

per user message:
  --> NewSessionRequest --> SessionId (fresh, no accumulated state)
  --> SetSessionModeRequest("plan") --> mode confirmed
  --> PromptRequest(session_id, compressed_context) --> stream AcpUpdates
  --> session discarded after response complete

ACP process crash:
  --> background client loop detects disconnect
  --> next request triggers automatic reconnection (spawn new process)

daemon shutdown:
  --> Server::shutdown() sends Shutdown via channel
  --> background task sends graceful close to ACP process
  --> kill_on_drop(true) as safety net
```

### Why Per-Request Sessions

The WASM agent manages conversation history and sends the full context (system prompt + all history + new message) with each request. If we reuse ACP sessions, the agent's internal history would duplicate with ours. Creating a new session per request:

- Avoids context duplication
- Keeps state clean
- Fully compatible with existing WASM agent architecture
- Session creation is lightweight (two JSON-RPC calls: NewSession + SetMode, no process spawn)

### Crash Recovery

If the ACP process crashes:

1. The `mpsc::Sender<ClientRequest>` channel closes
2. `is_alive()` returns false
3. Next `stream_acp_completion` call detects dead provider
4. Automatically calls `connect()` to spawn a new process
5. Retries the request on the new connection

## Conversation Context Compression

### Problem

WASM agent sends the full conversation history with each request. Sending all
messages to a per-request ACP session wastes tokens and may exceed context
limits. Long conversations need compression.

### Dynamic Token-Budget Compression

Use a **dynamic token budget** based on the model's context window, not a fixed
number. Different models have vastly different limits (gemini 1M vs claude 200K),
so the budget must adapt automatically.

```rust
const HISTORY_BUDGET_RATIO: f32 = 0.3;    // 30% of model context for history
const RECENT_TURNS_PROTECTED: usize = 3;  // Always keep last N turns intact

fn history_token_budget(model_context_limit: usize) -> usize {
    // Reserve 70% for system prompt + new response generation
    (model_context_limit as f32 * HISTORY_BUDGET_RATIO) as usize
}

fn build_prompt_content(
    request: &LlmChatRequest,
    model_context_limit: usize,
) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();

    // 1. System prompt (always included, full)
    if let Some(system) = &request.system {
        blocks.push(ContentBlock::Text(TextContent::new(
            format!("[System Instructions]\n{}\n[End System Instructions]", system)
        )));
    }

    // 2. Compress conversation history within dynamic budget
    let budget = history_token_budget(model_context_limit);
    let messages = &request.messages;
    let compressed = compress_history(messages, budget, RECENT_TURNS_PROTECTED);
    blocks.push(ContentBlock::Text(TextContent::new(compressed)));

    blocks
}
```

### Compression Algorithm

```rust
fn compress_history(
    messages: &[LlmMessage],
    token_budget: usize,
    protected_recent: usize,
) -> String {
    let total = messages.len();

    // If everything fits in budget, send full history
    let full_text = format_all_messages(messages);
    if estimate_tokens(&full_text) <= token_budget {
        return full_text;
    }

    // Split: older messages (compressible) + recent messages (protected)
    let split_point = total.saturating_sub(protected_recent * 2); // *2 for user+assistant pairs
    let older = &messages[..split_point];
    let recent = &messages[split_point..];

    // Recent messages: always full
    let recent_text = format_all_messages(recent);
    let recent_tokens = estimate_tokens(&recent_text);
    let remaining_budget = token_budget.saturating_sub(recent_tokens);

    // Older messages: compress with middle-out priority
    let summary = compress_older_messages(older, remaining_budget);

    format!(
        "[Earlier conversation summary]\n{}\n[End summary]\n\n{}",
        summary, recent_text
    )
}
```

### Compression Rules by Message Role

**User messages**: head truncation

```
[user] {first 200 chars}...
```

**Assistant messages**: head + tail (conclusions often at end)

```
[assistant] {first 100 chars}...{last 100 chars}
```

**Tool messages**: preserve tool name, key params, status, and result snippet

```
[tool: read_file("config.toml") -> ok | [workspace]\nmembers = ["daemon"...]]
[tool: computer_screenshot() -> ok | (image omitted)]
[tool: bash("cargo test") -> error | test xyz failed: assertion...]
```

### Middle-Out Compression Priority

Inspired by Goose's tool response filtering strategy: the earliest and most
recent messages in the older section carry the most context value. Middle
messages are compressed most aggressively.

```
Older messages:  [msg0] [msg1] [msg2] [msg3] [msg4] [msg5] [msg6] [msg7]
                  ^^^^                                              ^^^^
                 keep     <--- compress most aggressively --->     keep
                 more                                              more
                detail                                            detail

Priority order (last to be compressed):
  1. First 2 messages (initial context/setup)
  2. Last 2 messages before the protected recent window
  3. Middle messages (compressed first, most aggressively)
```

Implementation: when compressing older messages, assign each message a
compression level based on its position. Messages near the edges get longer
excerpts (200 chars), messages in the middle get shorter excerpts (80 chars)
or are reduced to one-line summaries if budget is tight.

### ContextLengthExceeded Recovery

If the ACP agent returns a context length error, automatically retry with
progressively more aggressive compression (inspired by Goose's error recovery):

```rust
async fn stream_acp_with_retry(
    acp: &AcpProvider,
    request: &LlmChatRequest,
    model_context_limit: usize,
    tx: mpsc::Sender<LlmStreamChunk>,
) -> Result<()> {
    // Level 0: normal compression (30% of context for history)
    // Level 1: aggressive (15% of context)
    // Level 2: minimal (system prompt + last message only)
    for level in 0..=2 {
        let budget = match level {
            0 => (model_context_limit as f32 * 0.30) as usize,
            1 => (model_context_limit as f32 * 0.15) as usize,
            _ => 0,  // No history, only system prompt + last message
        };

        let content = build_prompt_content_with_budget(request, budget);
        let session_id = acp.new_session().await?;

        match acp.prompt(session_id, content).await {
            Ok(stream) => return stream_updates(stream, tx).await,
            Err(e) if is_context_length_error(&e) && level < 2 => {
                tracing::warn!(
                    "Context length exceeded at compression level {}, retrying with level {}",
                    level, level + 1
                );
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}
```

### Token Estimation

Use a simple heuristic (chars / 4) for token estimation. Precise tokenization
is not needed — the budget is a soft limit to prevent context explosion, not an
exact fit.

```rust
fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}
```

## System Prompt Handling

### Current Problem

System prompt (~9680 chars with XML tool definitions) passed via `--system-prompt` command-line argument, which fails on Windows.

### ACP Solution

System prompt is included as the first `ContentBlock` in each `PromptRequest`,
sent via stdin/stdout binary frames. No command-line arguments needed for
content.

The system prompt contains custom tool definitions (browser use tools,
computer control tools, skills) that the ACP agent does not know about. In plan
mode, the ACP agent's built-in system prompt has minimal impact (no tool
execution), so our injected system prompt provides the tool definitions that
guide the model's `<tool_call>` XML output for daemon-side execution.

## ACP Mode: Chat/Plan Only

ACP agents run in **plan mode** (no tool execution):

- Agent only generates text responses
- Tool calls are expressed as `<tool_call>` XML markers in text output
- Daemon parses `<tool_call>` markers and executes tools via proxy/sidebar
- Tool results are sent back as part of the next prompt's conversation history

This preserves the existing tool execution flow through the daemon. The ACP
agent's built-in tools are disabled in plan mode, avoiding conflicts with our
custom tool definitions.

## stream_acp_completion Integration

New function in `crates/daemon/src/wasm/llm.rs`:

```rust
async fn stream_acp_completion(
    request: LlmChatRequest,
    tx: mpsc::Sender<LlmStreamChunk>,
    provider: ProviderType,
) -> Result<()> {
    // 1. Get or reconnect AcpProvider (cached in DaemonHostFunctions)
    let acp = get_or_reconnect_acp_provider(provider).await?;

    // 2. Get model context limit for dynamic budget calculation
    let model_context_limit = default_context_window_for(provider);

    // 3. Stream with automatic ContextLengthExceeded recovery
    stream_acp_with_retry(&acp, &request, model_context_limit, tx).await
}

async fn stream_updates(
    mut response_rx: mpsc::Receiver<AcpUpdate>,
    tx: mpsc::Sender<LlmStreamChunk>,
) -> Result<()> {
    while let Some(update) = response_rx.recv().await {
        match update {
            AcpUpdate::Text(text) => {
                tx.send(LlmStreamChunk {
                    text: Some(text),
                    tool_calls: vec![],
                    done: false,
                    reasoning: None,
                    images: vec![],
                }).await?;
            }
            AcpUpdate::Thought(thought) => {
                tx.send(LlmStreamChunk {
                    text: None,
                    tool_calls: vec![],
                    done: false,
                    reasoning: Some(thought),
                    images: vec![],
                }).await?;
            }
            AcpUpdate::Complete(_) => {
                tx.send(LlmStreamChunk {
                    text: None,
                    tool_calls: vec![],
                    done: true,
                    reasoning: None,
                    images: vec![],
                }).await?;
                break;
            }
            AcpUpdate::Error(e) => return Err(DaemonError::InternalError(e).into()),
        }
    }
    Ok(())
}
```

### Provider Routing

In the existing match on `ProviderType`:

```rust
ProviderType::ClaudeCode => {
    stream_acp_completion(request, tx, provider).await
}
ProviderType::GeminiCli => {
    stream_acp_completion(request, tx, provider).await
}
```

### AcpProvider Storage

The `AcpProvider` instances are cached in `DaemonHostFunctions` (or a dedicated
`AcpProviderRegistry`), keyed by `ProviderType`. The provider is created lazily
on first use and reconnected automatically on crash.

```rust
// In DaemonHostFunctions or a separate registry
acp_providers: Arc<TokioMutex<HashMap<ProviderType, AcpProvider>>>,
```

## Dependencies

### New

- `sacp = "10.1.0"` — ACP protocol library (crates.io)
- `tokio-util` with `compat` feature — bridge tokio I/O to futures I/O for sacp's `ByteStreams`

### Existing

- `which = "7"` — Command resolution with PATHEXT support
- `futures` — already in llm crate
- `tokio` — already in llm crate

### User Installation

- Claude: `npm install -g @zed-industries/claude-agent-acp`
- Gemini: `npm install -g @google/gemini-cli` (uses `--acp` flag)

## Files to Delete

- `crates/llm/src/providers/claude_code/` (entire directory)
- `crates/llm/src/providers/gemini_cli/` (entire directory)
- `crates/llm/tests/claude_code_provider.rs`
- `crates/llm/tests/gemini_cli_provider.rs`

## Files to Create

- `crates/llm/src/providers/acp/mod.rs` — AcpProvider, ClientRequest, AcpUpdate, process spawn, client loop
- `crates/llm/src/providers/acp/claude.rs` — Claude ACP config and mode mapping
- `crates/llm/src/providers/acp/gemini.rs` — Gemini ACP config and mode mapping
- `crates/llm/src/providers/acp/context.rs` — Token-budget conversation compression

## Files to Modify

- `crates/llm/Cargo.toml` — add `sacp`, `tokio-util` dependencies
- `crates/llm/src/providers/mod.rs` — replace module declarations
- `crates/llm/src/util.rs` — keep `which`-based resolution (used by ACP)
- `crates/daemon/src/wasm/llm.rs` — add `stream_acp_completion`, update routing, add `build_prompt_content`
- `crates/daemon/src/agent_host.rs` — add `acp_providers` field to `DaemonHostFunctions`
- `crates/daemon/src/server.rs` — add ACP provider shutdown in `Server::shutdown()`
- `crates/daemon/Cargo.toml` — add `sacp` if needed for type re-exports

## Windows Compatibility

All communication via stdin/stdout binary frames (sacp protocol). No
command-line arguments for content. The only command-line args are short flags
like `--acp` or `--model`, which contain no special characters. This completely
eliminates:

- CVE-2024-24576 batch file argument rejection
- cmd.exe 8191-char command line limit
- OAuth token discovery issues (`.cmd` wrapper runs normally for short args)

## Testing Strategy

1. **Unit tests for context compression**: verify token-budget compression, head+tail extraction, tool result formatting
2. **Unit tests for `AcpProvider`**: mock sacp transport, verify session lifecycle (NewSession + SetMode two-step)
3. **Unit tests for content building**: verify system prompt + compressed messages formatting
4. **Integration tests**: spawn real `claude-agent-acp` / `gemini --acp` (skipped in CI if not installed)
5. **Crash recovery tests**: verify automatic reconnection when ACP process dies
6. **Existing daemon tests**: streaming flow tests remain applicable
