# Code Mode as Internal Execution Strategy — Design Document

**Date**: 2026-03-07
**Status**: Approved

---

## 1. Problem Statement

NevoFlux currently treats Code Mode as a fourth user-facing mode (Chat / Browser / Agent / Code). This conflates two distinct use cases:

- **Tool orchestration** (invisible): LLM writes Python to batch-call multiple tools in one step, reducing LLM round trips. The user never sees the code.
- **Canvas/artifact generation** (visible): LLM generates user-facing code (React apps, scripts) for Canvas rendering.

Code Mode as a standalone mode serves neither well — it's too limited for Canvas (Monty can't do imports/classes) and too exposed for orchestration (users shouldn't need a special mode to get parallel tool execution).

Meanwhile, `python-exec` already exists as a tool in Agent mode, organically drifting toward the right architecture.

## 2. Core Design Decision

**Code Mode becomes an internal execution strategy, not a user-facing mode.**

The three user-visible modes (Chat / Browser / Agent) map to user intent. `python-exec` is renamed to `orchestrate` and enhanced with:
- Tool function signature injection
- Async/sequential tool annotations
- `ResolveFutures` support (fake async, sequential dispatch)
- LLM rewrite callback for error recovery

```
User-visible modes (unchanged):
  Chat     -> chat tools only
  Browser  -> browser tools + orchestrate
  Agent    -> all tools + orchestrate

Internal execution strategy (invisible to user):
  Agent/Browser LLM sees two execution paths:
    1. Traditional: tool_call(browser_click, {selector: "#btn"})
    2. Orchestrate: tool_call(orchestrate, {code: "..."})
```

## 3. Architecture

### 3.1 Tool Rename: `python-exec` -> `orchestrate`

`python-exec` exposes implementation details. `orchestrate` accurately describes intent — the LLM is orchestrating multiple tool calls.

### 3.2 `AgentMode::Code` Deprecation

- Protocol enum variant kept for backward compatibility
- Server silently maps incoming `"code"` mode to `AgentMode::Agent`
- Mark as `#[deprecated]` in code, remove in next protocol major bump

### 3.3 Signature Injection Strategy

**Two-tier signatures:**

| Tier | Purpose | Size | Location |
|------|---------|------|----------|
| Compact | LLM tool selection | ~800-1000 tokens | `orchestrate` tool description |
| Full stubs | Monty type checker (`ty`) | ~3000 tokens | Passed to `MontyRun` internally |

**Compact format example:**
```python
async def web_fetch(url: str) -> str  # Fetch URL content
async def browser_screenshot(full_page: bool = False) -> bytes  # Page screenshot
def browser_click(selector: str, button: str = "left") -> ClickResult  # Click element [sequential]
def bash(command: str) -> BashResult  # Execute shell command [sequential]
```

**System prompt addition (~30 tokens):**
```
When a task involves multiple steps or tools, prefer using the
orchestrate tool to batch tool calls in Python.
```

**Mode-scoped generation:** `SignatureCache` is keyed by `AgentMode`. Browser mode's `orchestrate` only includes browser-mode tools; Agent mode includes all tools. Regenerated when tool set changes (e.g., MCP server connect/disconnect).

### 3.4 Sequential vs Async Tool Marking

**Design: default sequential, whitelist async.** This is fail-safe — new tools that aren't explicitly marked default to sequential (slower but correct), not async (faster but potentially wrong).

```rust
const ASYNC_SAFE: &[&str] = &[
    // Browser read-only
    "browser_screenshot", "browser_get_content", "browser_get_elements",
    "browser_get_element", "browser_query_all", "browser_get_markdown",
    "browser_eval_js",
    // Network
    "web_search", "web_fetch",
    // File read-only
    "read_file", "glob", "grep",
    // Memory
    "memory_view", "memory_search", "memory_create",
    // MCP
    "mcp_list_tools", "mcp_call", "mcp_read_resource",
];

fn is_sequential(name: &str) -> bool {
    !ASYNC_SAFE.contains(&name)
}
```

- Async-safe tools: `async def` in signatures, can be used with `asyncio.gather()`
- Sequential tools: `def` in signatures, `[sequential]` marker in compact format
- Monty runtime enforces: drains pending async tasks before executing sync call

### 3.5 Fake Async (Phase 1)

LLM writes `asyncio.gather()` naturally. Runtime dispatches sequentially:

```rust
RunProgress::ResolveFutures { futures, state } => {
    let mut results = Vec::new();
    for future in futures {
        let result = execute_single_tool(&future.name, &future.args).await;
        results.push(result);
    }
    state = snapshot.resume_futures(results);
}
```

**Why fake async is correct for Phase 1:**
- NevoFlux's tool pipeline includes human-in-the-loop blockers (permission approval)
- Concurrent browser operations on the same tab cause race conditions
- The main Code Mode performance win is reducing LLM round trips (4-6x -> 2x), not I/O parallelism
- LLM code surface is its final form from day one; only the runtime strategy changes in Phase 2

### 3.6 Auto-Generated Parameter Mappings

Replaces the hardcoded 60-line `positional_to_named` function. Generated from `ToolDefinition.input_schema`:

```rust
fn build_param_mapping(tool: &ToolDefinition) -> Vec<String> {
    // Extract property names from JSON Schema
    // required params first, optional params after
    // Order matches schema declaration order
}
```

At execution time, positional args mapped by index, kwargs used directly, kwargs override positional:

```rust
RunProgress::FunctionCall { name, args, kwargs, .. } => {
    let mapping = param_mappings.get(&name);
    let mut named = Map::new();
    for (i, val) in args.iter().enumerate() {
        if let Some(key) = mapping.and_then(|m| m.get(i)) {
            named.insert(key.clone(), val.clone());
        }
    }
    for (k, v) in kwargs { named.insert(k, v); }
    execute_tool(&name, &named).await;
}
```

LLMs naturally write `web_fetch("https://example.com")` (positional) for single-arg functions. Fighting this with keyword-only params would cause unnecessary retries.

### 3.7 Return Format

```json
{
  "output": "print() output concatenated",
  "result": "<final expression value as JSON>",
  "success": true,
  "error": null
}
```

**No `tool_calls` array in the return.** Individual tool results can be enormous (full page HTML, base64 screenshots). Including them in the return would cause token explosion in the next LLM context — defeating Code Mode's purpose. The LLM controls what it returns via the final expression. Tool execution details go to TraceCollector (already exists).

### 3.8 Input Variable Injection

`orchestrate` takes only a `code` parameter. Monty `inputs` are empty.

If the LLM needs context (current URL, tab ID), it calls tools within the code:
```python
url = await browser_get_url()
content = await browser_get_content()
```

**Decision**: Path A (tool calls for context). Path B (auto-inject context variables) is a future optimization if LLMs frequently waste calls fetching basic context. Documented here so implementers don't have to re-derive this decision.

### 3.9 LLM Rewrite Callback

Wired from day one. `CodeModeExecutor` already has the callback mechanism — currently a no-op in `execute_python_simple`. Replace with a real callback that routes through the agent's LLM session.

**Rationale**: Auto-fix (Layer 2) and Linter (Layer 3) handle mechanical errors (imports, decorators). But the most common Code Mode errors are semantic — wrong param names, misunderstood return types, logic errors. Only the LLM can fix those. Without the rewrite callback, every runtime error triggers a full WASM agent loop round-trip, which is heavier than an internal rewrite.

## 4. New Module: `signature.rs`

New file: `crates/daemon/src/agent/code_mode/signature.rs`

### 4.1 Core Types

```rust
pub struct SignatureCache {
    /// Compact one-liner signatures for orchestrate tool description
    compact: HashMap<AgentMode, String>,
    /// Full stubs with docstrings + TypedDict for Monty type checker
    full_stubs: HashMap<AgentMode, String>,
    /// Positional parameter name mappings per tool
    /// NOTE: assumes tool names are unique across sources.
    /// If MCP tools enter orchestrate, they need name prefixing.
    param_mappings: HashMap<String, Vec<String>>,
}
```

### 4.2 Type Mapping Pipeline

```
JSON Schema type          ->  Python type
─────────────────────────────────────────
{"type": "string"}        ->  str
{"type": "integer"}       ->  int
{"type": "number"}        ->  float
{"type": "boolean"}       ->  bool
{"type": "array", items}  ->  list[ItemType]
{"type": "object", props} ->  TypedDict (named)
{"type": "object"} (bare) ->  dict[str, Any]
{"enum": [...]}           ->  Literal["a", "b"]
{"const": "value"}        ->  Literal["value"]
{"anyOf": [...]}          ->  A | B
```

### 4.3 TypedDict Deduplication

Multiple tools may share structurally identical return types (e.g., `browser_click` and `browser_click_by_id` both return `ClickResult`). Deduplication is by **schema content hash**, not by name. Two schemas with the same structure but different source tools produce one TypedDict definition.

### 4.4 Signature Generation

Two generation functions per tool:

- `generate_compact_signature(tool) -> String`: one-liner with inline comment
- `generate_full_stub(tool) -> String`: multi-line with docstring, args docs, return type

Both respect `is_sequential()` for `def` vs `async def`.

## 5. File Change Summary

### New Files
- `crates/daemon/src/agent/code_mode/signature.rs`

### Modified Files
| File | Changes |
|------|---------|
| `executor.rs` | Handle `ResolveFutures` (sequential dispatch); use auto-generated `param_mappings`; remove/wrap `execute_python_simple`; remove hardcoded `positional_to_named` |
| `mod.rs` | Add `pub mod signature;` |
| `agent.rs` (builtin-wasm) | Rename `python-exec` -> `orchestrate`; dynamic tool description with compact signatures; `AgentMode::Code` -> `Agent` behavior; system prompt hint |
| `types.rs` (builtin-wasm) | Mark `AgentMode::Code` deprecated |
| `agent_host.rs` | Wire LLM rewrite callback; map `Code` -> `Agent` |
| `server.rs` | Map `"code"` mode string to `AgentMode::Agent` |

### Removed
- `CODE_MODE_PROMPT` (Monty constraints extracted to `orchestrate` description)
- Hardcoded `positional_to_named` (60 lines -> 0)

### Unchanged
- `CodeModeExecutor` core 4-layer pipeline
- `MontyAutoFixer`, `MontyLinter`, `RepairPrompt`
- `ToolRegistry` and all tool executors
- Type conversion (`json_to_monty_object` / `monty_object_to_json`)

## 6. Pre-requisite: Monty Version Upgrade

**Upgrade Monty v0.0.4 -> v0.0.7 before starting implementation.**

Changes between v0.0.4 and v0.0.7 include async stack overflow fixes, `map()` builtin, improved heap guard management. Since this design enables `ResolveFutures`, known async bugs in v0.0.4 may cause issues. Upgrading after implementation risks API changes requiring rework. Do it first.

## 7. Phase 2 Scope (Not in This Design)

- True concurrent execution in `ResolveFutures` (`tokio::join!` / `FuturesUnordered`)
- Concurrent permission approval UI queuing
- Per-tab mutex for browser operations
- Thread-safe `TraceCollector`
- Monty snapshot persistence to SQLite (pause/resume across sessions)
- Context variable auto-injection (Path B)

## 8. Implementation Order

```
1. Upgrade Monty v0.0.4 -> v0.0.7                           [pre-req]
2. SignatureGenerator + SignatureCache                        [foundation]
   - Type mapping pipeline
   - Compact + full stub generation
   - ASYNC_SAFE whitelist
   - Auto param_mappings from input_schema
   - TypedDict dedup by content hash
3. ResolveFutures sequential dispatch                        [core]
   - Replace error branch with sequential loop
4. Rename python-exec -> orchestrate                         [integration]
   - Dynamic tool description with compact signatures
   - Mode-scoped signature injection
5. Wire LLM rewrite callback                                 [integration]
   - Replace no-op with real LLM call
6. Deprecate AgentMode::Code                                 [cleanup]
   - Server-side mapping
   - Remove CODE_MODE_PROMPT
   - Remove hardcoded positional_to_named
```
