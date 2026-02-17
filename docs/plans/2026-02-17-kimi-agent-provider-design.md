# Kimi-Agent LLM Provider Design

## Overview

Add `kimi-agent` as a new CLI-based LLM provider in NevoFlux. Kimi-agent is the Rust implementation of Kimi Code CLI, designed for Wire mode — a JSON-RPC 2.0 based bidirectional protocol over stdin/stdout.

**Key difference from existing CLI providers**: kimi-agent uses a structured JSON-RPC 2.0 protocol with initialization handshake, bidirectional requests (tool calls, approvals), and event-based streaming. This is significantly richer than claude_code's stream-json or gemini_cli's text prompt approach.

**Reference**: [Wire Mode Documentation](https://moonshotai.github.io/kimi-cli/zh/customization/wire-mode.html)

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Approval handling | Auto-approve (`--yolo` flag) | NevoFlux has its own permission system |
| Tool strategy | Register as external tools via `initialize` | Follows existing pattern: provider returns tool calls, agent runner executes |
| Process lifecycle | Per-outer-turn (spawn/kill each turn) | Avoids conversation history duplication between NevoFlux and kimi-agent |
| Within-turn persistence | Keep alive for mid-turn tool handling | Wire protocol's `ToolCallRequest`/`ToolResult` requires ongoing connection |
| Model selection | Pass via `--model` flag | NevoFlux controls model from config.toml |
| Tool execution loop | Agent-runner controlled (external) | Preserves existing architecture |

## Architecture

### Approach: Stateful Wire Client

Per-outer-turn process lifecycle with persistent connection within a turn for tool result handling.

**State machine:**
```
NoProcess → (spawn + initialize + prompt) → InTurn → (ToolCallRequest) → WaitingForToolResult → (ToolResult) → InTurn → ... → (TurnEnd) → NoProcess
```

### Module Structure

```
crates/llm/src/providers/kimi_agent/
├── mod.rs           # Module re-exports
├── client.rs        # KimiAgentClient - config, builder, subprocess lifecycle
├── wire.rs          # WireClient - JSON-RPC 2.0 protocol, state machine
├── completion.rs    # KimiAgentCompletionModel - implements rig CompletionModel
└── types.rs         # Wire message types, response parsing, tool mapping
```

## Wire Client Design (`wire.rs`)

### State Machine

```rust
enum WireState {
    NoProcess,
    Initializing,
    Ready,
    InTurn {
        collected_text: String,
        collected_tool_calls: Vec<PendingToolCall>,
    },
    WaitingForToolResults {
        pending_ids: Vec<String>,
    },
    Finished,
}
```

### JSON-RPC 2.0 Protocol

All messages are newline-delimited JSON over stdin/stdout.

**Client → Server methods:**
- `initialize` — negotiate protocol version, register external tools
- `prompt` — send user input, trigger agent turn
- `cancel` — cancel active turn

**Server → Client notifications:**
- `event` — streaming events (ContentPart, ToolCall, TurnBegin/End, StatusUpdate)

**Server → Client requests (require response):**
- `request` with `ToolCallRequest` — external tool invocation
- `request` with `ApprovalRequest` — action approval (skipped with `--yolo`)

### Subprocess Management

```rust
pub struct WireClient {
    child: tokio::process::Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    state: WireState,
    next_request_id: u64,
}
```

**Spawn args:** `kimi-agent --yolo -m <model> -w <working_dir>`

## Client Configuration (`client.rs`)

```rust
pub struct KimiAgentClient {
    command: String,              // "kimi-agent" binary path
    model: Option<String>,        // --model flag
    working_dir: Option<String>,  // --work-dir flag
    thinking: Option<bool>,       // --thinking / --no-thinking
}
```

## CompletionModel Implementation (`completion.rs`)

```rust
pub struct KimiAgentCompletionModel {
    client: KimiAgentClient,
    wire: Arc<Mutex<Option<WireClient>>>,
}
```

### `completion()` / `stream()` Flow

**When `wire` is `None` (new turn):**
1. Spawn kimi-agent subprocess
2. Send `initialize` with external tool definitions
3. Send `prompt` with user message (built from CompletionRequest)
4. Read events until TurnEnd or ToolCallRequest
5. If tool calls: keep wire alive, return tool calls in response
6. If TurnEnd: kill process, return text response

**When `wire` is `Some` (resuming after tool execution):**
1. Send `ToolResult` responses for each tool result
2. Continue reading events until TurnEnd or next ToolCallRequest
3. Same logic for keep-alive vs kill

### Event Loop (`read_until_pause`)

| Wire Event | Action |
|---|---|
| `ContentPart { type: "text" }` | Append to accumulated text |
| `ContentPart { type: "think" }` | Map to reasoning content |
| `ToolCall` / `ToolCallPart` | Buffer tool call info |
| `ToolCallRequest` | Respond immediately, add to collected tool calls |
| `StatusUpdate` | Extract token usage |
| `TurnEnd` | Mark complete, return |
| `StepBegin` / `StepInterrupted` | Metadata only |
| stdout EOF | Error: process crashed |

### Streaming

Map wire events to rig's `RawStreamingChoice`:

| Wire Event | Streaming Output |
|---|---|
| `ContentPart(text)` | `RawStreamingChoice::Delta(text)` |
| `ContentPart(think)` | `RawStreamingChoice::Reasoning(text)` |
| `ToolCallRequest` | `RawStreamingChoice::ToolCall(...)` |
| `StatusUpdate` | Extract usage, don't yield |
| `TurnEnd` | Stream ends |

Wrapped in `ChildGuardStream` for cleanup on drop.

## Tool Registration & Mapping

### Initialize Handshake

```json
{
  "jsonrpc": "2.0",
  "method": "initialize",
  "id": "1",
  "params": {
    "protocol_version": "1.2",
    "client": { "name": "nevoflux", "version": "0.x.x" },
    "external_tools": [
      { "name": "read_file", "description": "...", "parameters": { ... } }
    ]
  }
}
```

### Tool Call Mapping

Wire `ToolCallRequest` → rig `ToolCall`:
```
{ id: "call_123", name: "read_file", arguments: {"path": "..."} }
```

### Tool Result Mapping

rig tool result → Wire `ToolResult` response:
```json
{
  "type": "ToolResult",
  "payload": {
    "tool_call_id": "call_123",
    "return_value": { "is_error": false, "output": "file contents...", "message": "" }
  }
}
```

## Edge Cases

### Initialize Timeout
- 30-second timeout for `initialize` response
- On timeout: kill process, return `LlmError::ProviderError`

### Process Crash Mid-Turn
- Detect stdout EOF (`read_line` returns `Ok(0)`)
- Check `child.try_wait()` for exit status
- Return descriptive error with exit code

### Multiple ToolCallRequests
- Wire protocol blocks per-request (sequential)
- Respond immediately to each, buffer for completion response
- State machine supports accumulating multiple tool calls

### Malformed JSON
- Skip malformed lines, log warning
- Continue processing (robustness pattern from claude_code)

## Error Handling

| Error Source | Handling |
|---|---|
| Process spawn fails | `LlmError::ProviderError("Failed to spawn kimi-agent: ...")` |
| Initialize timeout | Kill process, return error |
| JSON-RPC error (-32000 to -32003) | Map code to descriptive error message |
| Process crashes mid-turn | Detect EOF, return error with exit code |
| Malformed JSON | Skip line, log warning |
| Tool rejected during init | Log warning, continue |

## Configuration & Sidebar Integration

### 6 Files to Modify

1. **`crates/llm/src/factory.rs`** — Add `ProviderType::KimiAgent` variant
   - `from_str`: "kimi_agent", "kimi-agent", "kimi"
   - Default model: "kimi-latest"
   - Context window: 128,000
   - Env var: "MOONSHOT_API_KEY"

2. **`crates/daemon/src/config.rs`** — Add `kimi_agent: ProviderConfig` field
   - Match arms in `active_api_key()`, `active_model()`, `configured_providers()`

3. **`crates/daemon/src/server.rs`** — Config API endpoints
   - `config.llm.list`: return kimi-agent with icon
   - `config.llm.get`: return provider-specific settings
   - `config.llm.set`: save provider-specific config

4. **`crates/daemon/src/wasm/llm.rs`** — LLM router
   - Add `KimiAgent` match arm in `execute_llm_chat()`

5. **`crates/daemon/src/agent_host.rs`** — API key resolution
   - Add kimi-agent match arm

6. **`assets/icons/providers/`** — Provider icon (webp)

### Config TOML

```toml
[llm]
provider = "kimi_agent"

[llm.kimi_agent]
api_key = "..."
model = "kimi-latest"
command = "kimi-agent"
working_dir = "/tmp/nevoflux-workspace"
thinking = true
```

## Testing Strategy

- **Unit tests**: JSON-RPC message serialization/deserialization, state machine transitions, tool mapping
- **Integration tests**: Full wire protocol flow with mock kimi-agent subprocess (if feasible)
- **Response parsing**: ContentPart accumulation, tool call extraction, error handling
