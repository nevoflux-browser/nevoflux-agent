# Phase 3a Agent Dispatch — Design Notes

Inspected by: discovery task  
Date: 2026-05-16  
Branch: `feat/eval-phase3a-agent-dispatch`

---

## 1. EvalDispatchServices struct — minimum required fields

Pulled from `crates/daemon/src/server.rs` lines ~4300–4400 and the `HostServices` definition in `crates/daemon/src/wasm/services.rs:151`.

```rust
/// Minimum services `dispatch_eval_turn` needs.
/// All fields are already `Arc`-wrapped and are cheap to clone from
/// the daemon's long-lived `services` bundle.
pub struct EvalDispatchServices {
    // --- core, always required ---
    pub services: HostServices,           // wasm/services.rs:151 — contains database, skills, llm_config, etc.
    pub config: Arc<AgentConfig>,         // server.rs — passed to DaemonHostFunctions::new(config, runtime)
    pub runtime: tokio::runtime::Handle,  // server.rs — passed to DaemonHostFunctions::new(config, runtime)
    pub session_manager: Arc<SessionManager>, // needed for load_session_history(...)

    // --- registries that guard against session leaks ---
    pub cancellation_registry: CancellationRegistry,   // Arc<Mutex<HashMap<String, CancellationToken>>>
    pub interrupt_registry: InterruptRegistry,          // Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>
    pub extraction_registry: ExtractionRegistry,        // Arc<Mutex<HashMap<String, (Instant, Arc<SessionMemoryExtractor>)>>>

    // --- optional but wired in production ---
    pub canvas_video_service: Arc<CanvasVideoService>,  // .with_canvas_video_service(...)
}
```

**What eval can skip vs production:**
- `proxy_id` / `client_identity` / `channel` / `request_id` — these feed the *forwarder task* which streams chunks back to the sidebar. Eval does NOT need a forwarder: it only wants the final `AgentOutput`. Can pass dummy values (`""`, `b""`, `0`).
- `browser_sender` — can be `None` for eval (no live browser tab).
- `session_proxy_tracker` — eval sessions have no sidebar proxy; leave `None`.
- `trace_collector` — optional for eval; pass a `TraceCollector::new(...)` if tracing is desired.
- `generated_title` — sidebar display only; set `None`.

---

## 2. AgentInput construction — exact source from server.rs (~lines 4400–4450)

```rust
// Load MCP server names for system prompt injection
let mcp_servers: Vec<String> = crate::mcp_config::McpServersConfig::load()
    .map(|c| {
        c.servers
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.name.clone())
            .collect()
    })
    .unwrap_or_default();

let input = AgentInput {
    session_id: session_id.clone(),
    mode,                         // AgentMode — use AgentMode::default() for eval
    user_message: effective_message,
    history: load_session_history(
        session_manager,
        &session_id,
        config.daemon.context.max_history_messages,
    )
    .await,
    attachments,                  // Vec<Attachment> — empty for eval
    local_files,                  // Vec<LocalFileRef> — empty for eval
    custom_system_prompt: None,   // use default mode-based prompt
    tab_id,                       // Option<i64> — None for eval
    tab_ids,                      // Vec<TabInfo> — empty for eval
    skill_context,                // Option<SkillContext> — None for eval
    available_models: config.llm.configured_providers(),
    mcp_servers: mcp_servers.clone(),
    soul_context: {
        let sc = build_soul_context(&services);
        sc
    },
    tools_config: None,           // inherit mode's full tool set
    os_platform: Some(std::env::consts::OS.to_string()),
};
```

**Eval simplification:** For `dispatch_eval_turn(services, session_id, prompt)`:
- `mode` = `AgentMode::default()` (or accept as param)
- `user_message` = the `prompt` argument
- `history` = loaded from `session_manager` as shown
- all other optional fields = empty/None

`AgentInput` is defined at `crates/builtin-wasm/src/types.rs:279`.

---

## 3. Agent run call shape — exact lines from server.rs

```rust
// server.rs:4396
let agent = Agent::new(host);

// server.rs:4655
let agent_result = tokio::task::spawn_blocking(move || agent.run(&input)).await;
```

`Agent::new` takes `H: HostFunctions` — in production this is `DaemonHostFunctions`.
`agent.run(&input)` returns `Result<AgentOutput, AgentError>` (synchronous, hence `spawn_blocking`).

**Forwarder task** (server.rs ~4460–4640): spawned before `spawn_blocking`, it reads from `stream_rx` (an `UnboundedReceiver<SidebarStreamChunk>`) and forwards batched text/tool-event chunks to the sidebar via native messaging. The forwarder holds the `response_tx` sender. **Eval can skip this entirely** — just drop the `stream_tx` (or use an mpsc channel and ignore the receiver). The agent will still complete; unread chunks are simply discarded.

**Plan re-run path** (server.rs ~4760, ~4959): a second `Agent::new` + `spawn_blocking` handles the case where `AgentOutput.continue_loop` is true after plan approval. Eval does not need the re-run path; treat `AgentOutput` as terminal.

---

## 4. MockLlmProvider — trait, API, and production injection

### MockLlmProvider does NOT implement a Rust trait

After full inspection, `MockLlmProvider` (at `crates/testing/src/mocks/llm.rs:74`) implements only `Default` and its own inherent methods. It does **not** implement any `LlmProvider` trait. The `TestDaemonBuilder` holds it as a field but never wires it into `HostServices.llm_config`.

**Conclusion:** `MockLlmProvider` is currently a *recording/assertion helper* only. It cannot directly substitute for a live LLM in `Agent::run`.

### Production LLM injection path

Real LLM calls go through `HostServices.llm_config: Option<LlmConfig>` (services.rs:157), set via `services.with_llm(LlmConfig { provider, api_key, model, base_url })`.

The WASM linker (`crates/daemon/src/wasm/linker.rs:374`) reads `services.llm_config` and calls `execute_llm_chat(provider, api_key, model, request, base_url)` — this is a concrete HTTP call to the provider endpoint.

### How to inject a mock LLM for eval

Two options:
1. **Local mock HTTP server** — spin up a tiny Axum server in the test that returns canned responses in OpenAI wire format. Set `LlmConfig { base_url: Some("http://127.0.0.1:<port>"), api_key: "mock", model: "mock", provider: ProviderType::OpenAi }`. This is the cleanest approach and does not require changing production code.
2. **Provider-level override** — `DaemonHostFunctions::with_llm_override(provider, model)` (agent_host.rs:303) overrides which provider/model the host uses, but still makes real HTTP calls. Not useful for offline eval.

**Recommended approach for Phase 3a Task 4:** option (1) — a local mock HTTP server. The `MockLlmProvider`'s `next_response()` method can drive the response queue; a thin Axum handler can call it.

---

## 5. Crate name for builtin-wasm

```
# crates/builtin-wasm/Cargo.toml
name = "nevoflux-builtin-wasm"
```

In Rust, this means the crate is imported as `nevoflux_builtin_wasm` (hyphens → underscores). Use `use nevoflux_builtin_wasm::{Agent, AgentInput, AgentOutput, AgentMode};`.

---

## 6. Open questions / unknowns

1. **`SessionExtractor` is per-session, but the session may not exist yet in eval.** The production code in server.rs (~4350) gets-or-inserts from `extraction_registry` keyed by `session_id`. For eval, we must ensure the session exists in `SessionManager` before calling `dispatch_eval_turn`, or the `load_session_history` call will return an empty Vec (harmless) but `session_extractor.on_user_message()` will still run (also harmless). **Not a blocker**, but confirm the session is pre-created by the eval HTTP handler before dispatch.

2. **`build_soul_context(&services)` — does it tolerate missing knowledge_retriever?** In server.rs this call is made unconditionally. For eval `services`, `knowledge_retriever` will be `None`. Need to verify `build_soul_context` handles `None` gracefully (likely returns `None`, which is an `Option<String>` in `AgentInput`). Inspection of the call site suggests it is safe, but **verify before Task 3 implementation**.

3. **`DaemonHostFunctions::new` requires a `tokio::runtime::Handle`.** In `spawn_blocking` the handle must still be alive. In eval context, `Handle::current()` should work since we're inside the Tokio runtime. Prefer `Handle::current()` over storing in `EvalDispatchServices` to avoid stale handle issues.

4. **`agent.run(&input)` returns `AgentError` on LLM failure.** If the mock HTTP server is not running, the agent loop will fail with an HTTP error, not panic. The `AgentError` type needs to be checked — is it `pub` in `nevoflux_builtin_wasm`? Confirmed definition exists at `crates/builtin-wasm/src/agent.rs`; verify it is `pub` and derivable for `Display`.

5. **`tools_config: None` vs `ToolsConfig::None`.** For eval scenarios that should exercise only specific tools, `AgentInput.tools_config` can be set to `Some(ToolsConfig::Allow(vec!["..."]))`. This is optional for Phase 3a but may be needed for deterministic eval scenarios that should NOT invoke browser tools (which would hang waiting for a real browser).

6. **`AgentMode` default value.** The production path passes `mode` from the WebSocket message payload. For eval the right default needs to be verified — likely `AgentMode::Chat` or `AgentMode::Normal`. Check the `AgentMode` enum in `crates/builtin-wasm/src/types.rs`.

---

## Summary — what `dispatch_eval_turn` looks like

```rust
pub async fn dispatch_eval_turn(
    services: HostServices,          // pre-configured with llm_config pointing at mock server
    config: Arc<AgentConfig>,
    session_manager: Arc<SessionManager>,
    session_id: String,
    prompt: String,
) -> Result<AgentOutput, Box<dyn std::error::Error + Send + Sync>> {
    // 1. Build a fresh SessionMemoryExtractor
    let session_extractor = Arc::new(SessionMemoryExtractor::new(config.learning.extraction_interval));

    // 2. Set per-session fields on services
    let mut services_with_ctx = services
        .with_client_context(vec![], String::new())  // no sidebar proxy
        .with_session_id(session_id.clone());
    services_with_ctx.session_extractor = Some(session_extractor.clone());
    services_with_ctx.interrupt_flag = Arc::new(AtomicBool::new(false));

    // 3. Create a drop-sink for stream chunks (no sidebar to forward to)
    let (stream_tx, _stream_rx) = tokio::sync::mpsc::unbounded_channel::<SidebarStreamChunk>();

    // 4. Build DaemonHostFunctions
    let host = DaemonHostFunctions::new(config.clone(), Handle::current())
        .with_services(services_with_ctx)
        .with_sidebar_stream(stream_tx)
        .with_session_id(session_id.clone())
        .with_session_extractor(session_extractor);

    // 5. Build AgentInput (simplified)
    let input = AgentInput {
        session_id: session_id.clone(),
        mode: AgentMode::default(),
        user_message: prompt,
        history: load_session_history(&session_manager, &session_id, config.daemon.context.max_history_messages).await,
        attachments: vec![],
        local_files: vec![],
        custom_system_prompt: None,
        tab_id: None,
        tab_ids: vec![],
        skill_context: None,
        available_models: config.llm.configured_providers(),
        mcp_servers: vec![],
        soul_context: None,  // skip for eval
        tools_config: Some(ToolsConfig::None),  // disable tools for basic eval; parameterise later
        os_platform: Some(std::env::consts::OS.to_string()),
    };

    // 6. Run synchronously on a blocking thread
    let agent = Agent::new(host);
    let output = tokio::task::spawn_blocking(move || agent.run(&input)).await??;
    Ok(output)
}
```

File+line references:
- `DaemonHostFunctions::new` — `crates/daemon/src/agent_host.rs:198`
- `Agent::new` + `agent.run` — `crates/builtin-wasm/src/agent.rs:601` / run TBD
- `AgentInput` — `crates/builtin-wasm/src/types.rs:279`
- `HostServices` — `crates/daemon/src/wasm/services.rs:151`
- Production dispatch — `crates/daemon/src/server.rs:4360–4660`
