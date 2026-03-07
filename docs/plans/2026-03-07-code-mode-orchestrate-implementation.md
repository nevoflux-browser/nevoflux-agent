# Code Mode Orchestrate Execution Strategy — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Transform Code Mode from a standalone user-facing mode into an internal execution strategy (`orchestrate` tool) within Agent/Browser modes, with auto-generated tool signatures, fake async support, and auto param mappings.

**Architecture:** New `signature.rs` module generates Python function signatures from `ToolDefinition.input_schema`. The existing `orchestrate` tool description is replaced with dynamically-generated compact signatures. `ResolveFutures` changes from error to sequential dispatch. Hardcoded `positional_to_named` replaced by auto-generated mappings.

**Tech Stack:** Rust, Monty v0.0.7 (Python subset interpreter), serde_json, wasmtime

**Design doc:** `docs/plans/2026-03-07-code-mode-orchestrate-design.md`

---

## Task 1: Upgrade Monty v0.0.4 to v0.0.7

**Files:**
- Modify: `crates/daemon/Cargo.toml:98` (monty dependency line)
- Modify: `Cargo.lock` (auto-updated)

**Step 1: Update the dependency version**

In `crates/daemon/Cargo.toml`, change:
```toml
# Before:
monty = { git = "https://github.com/pydantic/monty.git", tag = "v0.0.4" }
# After:
monty = { git = "https://github.com/pydantic/monty.git", tag = "v0.0.7" }
```

**Step 2: Build and check for API changes**

Run: `cargo build -p nevoflux-daemon 2>&1 | head -50`
Expected: Either clean build, or compilation errors indicating API changes between v0.0.4 and v0.0.7.

If there are API changes in `MontyRun`, `RunProgress`, `MontyObject`, `CollectStringPrint`, `LimitedTracker`, or `ResourceLimits`, fix them in `crates/daemon/src/agent/code_mode/executor.rs`. The types used are:
- `MontyRun::new(code, filename, inputs, external_fn_names)` (line 263)
- `RunProgress::FunctionCall`, `RunProgress::Complete`, `RunProgress::OsCall`, `RunProgress::ResolveFutures` (lines 367-470)
- `MontyObject` variants (lines 101-176)
- `CollectStringPrint::new()` (line 319)
- `LimitedTracker::new(ResourceLimits)` (line 318)

**Step 3: Run existing Code Mode tests**

Run: `cargo test -p nevoflux-daemon code_mode -- --nocapture 2>&1 | tail -20`
Expected: All 19 executor tests + 98 auto_fixer tests pass.

**Step 4: Run full test suite**

Run: `cargo test --workspace 2>&1 | tail -5`
Expected: All tests pass.

**Step 5: Commit**

```bash
git add crates/daemon/Cargo.toml Cargo.lock
git commit -m "chore: upgrade monty v0.0.4 -> v0.0.7 for async fixes and map() builtin"
```

---

## Task 2: Create `signature.rs` — Type Mapping Pipeline

**Files:**
- Create: `crates/daemon/src/agent/code_mode/signature.rs`
- Modify: `crates/daemon/src/agent/code_mode/mod.rs:9-14` (add `pub mod signature`)
- Test: inline `#[cfg(test)] mod tests` in `signature.rs`

**Step 1: Write failing tests for `json_schema_to_python_type`**

Create `crates/daemon/src/agent/code_mode/signature.rs` with tests only:

```rust
//! Tool signature generation for Code Mode orchestrate tool.
//!
//! Generates Python function signatures from ToolDefinition JSON Schema,
//! used in the orchestrate tool description (compact) and Monty type checker (full).

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_primitive_types() {
        assert_eq!(json_schema_to_python_type(&json!({"type": "string"})), "str");
        assert_eq!(json_schema_to_python_type(&json!({"type": "integer"})), "int");
        assert_eq!(json_schema_to_python_type(&json!({"type": "number"})), "float");
        assert_eq!(json_schema_to_python_type(&json!({"type": "boolean"})), "bool");
    }

    #[test]
    fn test_array_type() {
        assert_eq!(
            json_schema_to_python_type(&json!({"type": "array", "items": {"type": "string"}})),
            "list[str]"
        );
    }

    #[test]
    fn test_bare_object() {
        assert_eq!(
            json_schema_to_python_type(&json!({"type": "object"})),
            "dict[str, Any]"
        );
    }

    #[test]
    fn test_enum_type() {
        assert_eq!(
            json_schema_to_python_type(&json!({"type": "string", "enum": ["left", "right", "middle"]})),
            "Literal[\"left\", \"right\", \"middle\"]"
        );
    }

    #[test]
    fn test_any_of() {
        assert_eq!(
            json_schema_to_python_type(&json!({"anyOf": [{"type": "string"}, {"type": "integer"}]})),
            "str | int"
        );
    }

    #[test]
    fn test_nullable() {
        // nullable is often {"anyOf": [{"type": "string"}, {"type": "null"}]}
        assert_eq!(
            json_schema_to_python_type(&json!({"anyOf": [{"type": "string"}, {"type": "null"}]})),
            "str | None"
        );
    }

    #[test]
    fn test_no_type() {
        assert_eq!(json_schema_to_python_type(&json!({})), "Any");
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p nevoflux-daemon signature::tests -- --nocapture 2>&1 | head -20`
Expected: FAIL — `json_schema_to_python_type` not defined.

**Step 3: Implement `json_schema_to_python_type`**

Add above the tests in `signature.rs`:

```rust
use serde_json::Value;

/// Map a JSON Schema type definition to a Python type annotation string.
pub fn json_schema_to_python_type(schema: &Value) -> String {
    // Check for enum first (takes precedence over type)
    if let Some(enum_vals) = schema.get("enum").and_then(|e| e.as_array()) {
        let literals: Vec<String> = enum_vals
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| format!("\"{}\"", s))
            .collect();
        if !literals.is_empty() {
            return format!("Literal[{}]", literals.join(", "));
        }
    }

    // Check for anyOf (union types)
    if let Some(any_of) = schema.get("anyOf").and_then(|a| a.as_array()) {
        let types: Vec<String> = any_of
            .iter()
            .map(|s| {
                if s.get("type").and_then(|t| t.as_str()) == Some("null") {
                    "None".to_string()
                } else {
                    json_schema_to_python_type(s)
                }
            })
            .collect();
        return types.join(" | ");
    }

    match schema.get("type").and_then(|t| t.as_str()) {
        Some("string") => "str".to_string(),
        Some("integer") => "int".to_string(),
        Some("number") => "float".to_string(),
        Some("boolean") => "bool".to_string(),
        Some("array") => {
            let items_type = schema
                .get("items")
                .map(|i| json_schema_to_python_type(i))
                .unwrap_or_else(|| "Any".to_string());
            format!("list[{}]", items_type)
        }
        Some("object") => {
            if schema.get("properties").is_some() {
                // Objects with properties become dict[str, Any] in compact mode.
                // Full TypedDict generation is separate.
                "dict[str, Any]".to_string()
            } else {
                "dict[str, Any]".to_string()
            }
        }
        Some("null") => "None".to_string(),
        _ => "Any".to_string(),
    }
}
```

**Step 4: Register module**

In `crates/daemon/src/agent/code_mode/mod.rs`, add after line 12:
```rust
pub mod signature;
```

**Step 5: Run tests to verify they pass**

Run: `cargo test -p nevoflux-daemon signature::tests -- --nocapture`
Expected: All 7 tests pass.

**Step 6: Commit**

```bash
git add crates/daemon/src/agent/code_mode/signature.rs crates/daemon/src/agent/code_mode/mod.rs
git commit -m "feat(code-mode): add json_schema_to_python_type for signature generation"
```

---

## Task 3: Compact & Full Signature Generation

**Files:**
- Modify: `crates/daemon/src/agent/code_mode/signature.rs`

**Step 1: Write failing tests for `generate_compact_signature` and `generate_full_stub`**

Add to tests in `signature.rs`:

```rust
    #[test]
    fn test_compact_signature_async() {
        let tool = tool_def(
            "web_fetch",
            "Fetch URL content",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "The URL to fetch"}
                },
                "required": ["url"]
            }),
        );
        let sig = generate_compact_signature(&tool);
        assert_eq!(sig, "async def web_fetch(url: str) -> Any  # Fetch URL content");
    }

    #[test]
    fn test_compact_signature_sequential() {
        let tool = tool_def(
            "browser_click",
            "Click an element on the page",
            json!({
                "type": "object",
                "properties": {
                    "selector": {"type": "string"},
                    "button": {"type": "string", "enum": ["left", "right", "middle"], "default": "left"}
                },
                "required": ["selector"]
            }),
        );
        let sig = generate_compact_signature(&tool);
        assert_eq!(
            sig,
            "def browser_click(selector: str, button: Literal[\"left\", \"right\", \"middle\"] = \"left\") -> Any  # Click an element on the page [sequential]"
        );
    }

    #[test]
    fn test_compact_signature_no_params() {
        let tool = tool_def(
            "browser_get_tabs",
            "Get all open tabs",
            json!({"type": "object", "properties": {}}),
        );
        let sig = generate_compact_signature(&tool);
        assert_eq!(sig, "async def browser_get_tabs() -> Any  # Get all open tabs");
    }

    #[test]
    fn test_full_stub() {
        let tool = tool_def(
            "web_fetch",
            "Fetch URL content",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "The URL to fetch"}
                },
                "required": ["url"]
            }),
        );
        let stub = generate_full_stub(&tool);
        assert!(stub.contains("async def web_fetch(url: str) -> Any:"));
        assert!(stub.contains("\"\"\"Fetch URL content"));
        assert!(stub.contains("url: The URL to fetch"));
        assert!(stub.contains("..."));
    }

    // Helper to create a ToolDefinition for tests
    fn tool_def(name: &str, desc: &str, schema: serde_json::Value) -> nevoflux_builtin_wasm::ToolDefinition {
        nevoflux_builtin_wasm::ToolDefinition {
            name: name.into(),
            description: desc.into(),
            input_schema: schema,
        }
    }
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p nevoflux-daemon signature::tests -- --nocapture 2>&1 | head -20`
Expected: FAIL — functions not defined.

**Step 3: Implement ASYNC_SAFE whitelist and signature generators**

Add to `signature.rs`:

```rust
use nevoflux_builtin_wasm::ToolDefinition;

/// Tools that are safe for parallel execution (async def).
/// All other tools default to sequential (def).
const ASYNC_SAFE: &[&str] = &[
    // Browser read-only
    "browser_screenshot", "browser_get_content", "browser_get_elements",
    "browser_get_element", "browser_query_all", "browser_get_markdown",
    "browser_eval_js", "browser_read_artifact", "browser_get_tabs",
    "browser_query_tabs", "browser_find_elements", "browser_element_info",
    // Network
    "web_search", "web_fetch", "fetch_page",
    // File read-only
    "read_file", "read", "glob", "grep", "list_files",
    // Memory
    "memory_search", "memory_create", "memory_update", "memory_delete",
    "memory_view", "knowledge_teach",
    // MCP
    "mcp_list_tools", "mcp_call", "mcp_read_resource",
    "tool_search", "tool_call_dynamic",
    // Meta
    "think", "switch_model",
];

/// Check if a tool is sequential (must complete before the next action).
pub fn is_sequential(name: &str) -> bool {
    !ASYNC_SAFE.contains(&name)
}

/// Generate a compact one-liner signature for a tool.
///
/// Format: `async def name(params) -> ReturnType  # Description [sequential]`
pub fn generate_compact_signature(tool: &ToolDefinition) -> String {
    let async_prefix = if is_sequential(&tool.name) { "" } else { "async " };
    let sequential_tag = if is_sequential(&tool.name) { " [sequential]" } else { "" };
    let params = generate_params_string(&tool.input_schema);

    format!(
        "{}def {}({}) -> Any  # {}{}",
        async_prefix, tool.name, params, tool.description, sequential_tag
    )
}

/// Generate a full function stub with docstring for Monty type checker.
pub fn generate_full_stub(tool: &ToolDefinition) -> String {
    let async_prefix = if is_sequential(&tool.name) { "" } else { "async " };
    let params = generate_params_string(&tool.input_schema);
    let mut stub = format!("{}def {}({}) -> Any:\n", async_prefix, tool.name, params);

    // Docstring
    stub.push_str(&format!("    \"\"\"{}",  tool.description));

    // Parameter docs
    let param_docs = generate_param_docs(&tool.input_schema);
    if !param_docs.is_empty() {
        stub.push_str("\n\n    Args:\n");
        stub.push_str(&param_docs);
    }

    stub.push_str("\"\"\"\n    ...\n");
    stub
}

/// Generate the parameter string for a function signature.
fn generate_params_string(schema: &Value) -> String {
    let props = match schema.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return String::new(),
    };

    let required: std::collections::HashSet<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut params: Vec<String> = Vec::new();

    // Required params first, then optional
    let mut required_params: Vec<(&String, &Value)> = Vec::new();
    let mut optional_params: Vec<(&String, &Value)> = Vec::new();

    for (name, prop_schema) in props {
        if required.contains(name.as_str()) {
            required_params.push((name, prop_schema));
        } else {
            optional_params.push((name, prop_schema));
        }
    }

    for (name, prop_schema) in &required_params {
        let py_type = json_schema_to_python_type(prop_schema);
        params.push(format!("{}: {}", name, py_type));
    }

    for (name, prop_schema) in &optional_params {
        let py_type = json_schema_to_python_type(prop_schema);
        let default = prop_schema
            .get("default")
            .map(|d| format_default(d))
            .unwrap_or_else(|| "None".to_string());
        params.push(format!("{}: {} = {}", name, py_type, default));
    }

    params.join(", ")
}

/// Format a JSON default value as a Python literal.
fn format_default(val: &Value) -> String {
    match val {
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("\"{}\"", s),
        _ => "None".to_string(),
    }
}

/// Generate Args section for docstring from JSON Schema properties.
fn generate_param_docs(schema: &Value) -> String {
    let props = match schema.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return String::new(),
    };

    let mut docs = String::new();
    for (name, prop_schema) in props {
        if let Some(desc) = prop_schema.get("description").and_then(|d| d.as_str()) {
            docs.push_str(&format!("        {}: {}\n", name, desc));
        }
    }
    docs
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p nevoflux-daemon signature::tests -- --nocapture`
Expected: All tests pass.

**Step 5: Commit**

```bash
git add crates/daemon/src/agent/code_mode/signature.rs
git commit -m "feat(code-mode): add compact and full signature generation with async/sequential marking"
```

---

## Task 4: `SignatureCache` and `build_param_mapping`

**Files:**
- Modify: `crates/daemon/src/agent/code_mode/signature.rs`

**Step 1: Write failing tests for `build_param_mapping` and `SignatureCache`**

Add to tests in `signature.rs`:

```rust
    #[test]
    fn test_build_param_mapping() {
        let tool = tool_def(
            "browser_click",
            "Click element",
            json!({
                "type": "object",
                "properties": {
                    "selector": {"type": "string"},
                    "button": {"type": "string", "default": "left"}
                },
                "required": ["selector"]
            }),
        );
        let mapping = build_param_mapping(&tool);
        // Required first, then optional
        assert_eq!(mapping, vec!["selector", "button"]);
    }

    #[test]
    fn test_build_param_mapping_no_params() {
        let tool = tool_def(
            "browser_get_tabs",
            "Get tabs",
            json!({"type": "object", "properties": {}}),
        );
        let mapping = build_param_mapping(&tool);
        assert!(mapping.is_empty());
    }

    #[test]
    fn test_signature_cache_compact() {
        let tools = vec![
            tool_def("web_fetch", "Fetch URL", json!({
                "type": "object",
                "properties": {"url": {"type": "string"}},
                "required": ["url"]
            })),
            tool_def("browser_click", "Click element", json!({
                "type": "object",
                "properties": {"selector": {"type": "string"}},
                "required": ["selector"]
            })),
        ];
        let cache = SignatureCache::build(&tools);

        let compact = cache.compact_signatures();
        assert!(compact.contains("async def web_fetch"));
        assert!(compact.contains("def browser_click"));
        assert!(compact.contains("[sequential]"));
    }

    #[test]
    fn test_signature_cache_param_mappings() {
        let tools = vec![
            tool_def("web_fetch", "Fetch URL", json!({
                "type": "object",
                "properties": {"url": {"type": "string"}},
                "required": ["url"]
            })),
        ];
        let cache = SignatureCache::build(&tools);
        assert_eq!(cache.param_mapping("web_fetch"), Some(&vec!["url".to_string()]));
        assert_eq!(cache.param_mapping("nonexistent"), None);
    }
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p nevoflux-daemon signature::tests -- --nocapture 2>&1 | head -20`
Expected: FAIL — `build_param_mapping`, `SignatureCache` not defined.

**Step 3: Implement `build_param_mapping` and `SignatureCache`**

Add to `signature.rs`:

```rust
use std::collections::HashMap;

/// Build positional parameter name mapping from JSON Schema.
/// Required parameters come first, then optional, preserving declaration order.
///
/// NOTE: Assumes tool names are unique across all sources (built-in + MCP).
/// If MCP tools enter orchestrate, they need name prefixing to avoid collisions.
pub fn build_param_mapping(tool: &ToolDefinition) -> Vec<String> {
    let props = match tool.input_schema.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return Vec::new(),
    };

    let required: std::collections::HashSet<&str> = tool
        .input_schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut required_params = Vec::new();
    let mut optional_params = Vec::new();

    for (name, _) in props {
        if required.contains(name.as_str()) {
            required_params.push(name.clone());
        } else {
            optional_params.push(name.clone());
        }
    }

    required_params.extend(optional_params);
    required_params
}

/// Cache of generated signatures and parameter mappings.
pub struct SignatureCache {
    compact: String,
    full_stubs: String,
    param_mappings: HashMap<String, Vec<String>>,
}

impl SignatureCache {
    /// Build the cache from a list of tool definitions.
    pub fn build(tools: &[ToolDefinition]) -> Self {
        let compact = tools
            .iter()
            .map(|t| generate_compact_signature(t))
            .collect::<Vec<_>>()
            .join("\n");

        let full_stubs = tools
            .iter()
            .map(|t| generate_full_stub(t))
            .collect::<Vec<_>>()
            .join("\n");

        let param_mappings: HashMap<String, Vec<String>> = tools
            .iter()
            .map(|t| (t.name.clone(), build_param_mapping(t)))
            .collect();

        Self {
            compact,
            full_stubs,
            param_mappings,
        }
    }

    /// Get compact signatures (for orchestrate tool description).
    pub fn compact_signatures(&self) -> &str {
        &self.compact
    }

    /// Get full stubs (for Monty type checker).
    pub fn full_stubs(&self) -> &str {
        &self.full_stubs
    }

    /// Get parameter mapping for a specific tool.
    pub fn param_mapping(&self, tool_name: &str) -> Option<&Vec<String>> {
        self.param_mappings.get(tool_name)
    }

    /// Get all external function names (for MontyRun::new).
    pub fn external_function_names(&self) -> Vec<String> {
        self.param_mappings.keys().cloned().collect()
    }
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p nevoflux-daemon signature::tests -- --nocapture`
Expected: All tests pass.

**Step 5: Commit**

```bash
git add crates/daemon/src/agent/code_mode/signature.rs
git commit -m "feat(code-mode): add SignatureCache with auto param mappings and compact/full signature tiers"
```

---

## Task 5: Handle `ResolveFutures` (Fake Async)

**Files:**
- Modify: `crates/daemon/src/agent/code_mode/executor.rs:462-469`

**Step 1: Write a failing test for async gather**

Add to tests in `executor.rs`:

```rust
    #[tokio::test]
    async fn test_resolve_futures_sequential_dispatch() {
        // Test that ResolveFutures is handled (not rejected) by executing
        // async external functions sequentially.
        // This requires Monty to support async/await, which we now allow.
        // For now, test the basic async code path doesn't error.
        let executor = CodeModeExecutor::new();
        // Simple async function call — Monty should handle single awaits
        // through the normal FunctionCall path.
        let result = executor
            .execute(
                "result = fetch(\"https://example.com\")\nprint(result)",
                &["fetch".to_string()],
                |_name, _args| {
                    Box::pin(async { Ok(serde_json::json!("page content")) })
                },
                |_prompt| Box::pin(async { Err("no rewrite".to_string()) }),
            )
            .await;
        assert!(
            result.success,
            "Expected success, got error: {:?}",
            result.error
        );
    }
```

Note: A true `asyncio.gather` test requires Monty v0.0.7's async support. If the Monty API doesn't expose `ResolveFutures` for simple sync calls, this test will pass on the existing code path. The key test is that the `ResolveFutures` branch no longer returns an error. We may need to write a Monty-specific test with actual async code once the API is verified.

**Step 2: Implement `ResolveFutures` handling**

In `executor.rs`, replace lines 462-469 (the `RunProgress::ResolveFutures` branch):

```rust
                    RunProgress::ResolveFutures(resolve_state) => {
                        // Phase 1: Fake async — dispatch futures sequentially.
                        // LLM writes asyncio.gather() naturally; runtime serializes execution.
                        // Phase 2 will replace this with tokio::join!/FuturesUnordered.
                        //
                        // The resolve_state contains pending futures that need to be
                        // executed before the gather can complete.
                        // For now, we process them one at a time through the existing
                        // FunctionCall handling by continuing the execution loop.
                        //
                        // Monty's ResolveFutures returns a state that we resume,
                        // which will yield individual FunctionCall events for each future.
                        // TODO: Verify Monty v0.0.7 API for ResolveFutures handling.
                        // The exact API depends on how Monty exposes pending futures.
                        // This may need adjustment after testing with actual async Monty code.
                        match resolve_state.run_sequential(&mut print_writer) {
                            Ok(next) => {
                                progress = next;
                            }
                            Err(exc) => {
                                let error_msg = exc.message().unwrap_or("async error").to_string();
                                let error_type = format!("{}", exc.exc_type());
                                return CodeModeResult::fail_with_output(
                                    print_writer.into_output(),
                                    format!("Async execution error: {error_type}: {error_msg}"),
                                )
                                .with_tool_results(tool_results)
                                .with_retries(retries);
                            }
                        }
                    }
```

**Important:** The exact API for `ResolveFutures` depends on Monty v0.0.7. After the Monty upgrade (Task 1), inspect the `RunProgress::ResolveFutures` variant to determine:
1. What fields it contains (futures list? state?)
2. How to resume with resolved values
3. Whether it has a `run_sequential` method or requires manual iteration

If the API is different, adapt the implementation accordingly. The key behavior is: **do not return an error, dispatch sequentially instead**.

**Step 3: Run tests**

Run: `cargo test -p nevoflux-daemon code_mode -- --nocapture`
Expected: All tests pass, including the new one.

**Step 4: Commit**

```bash
git add crates/daemon/src/agent/code_mode/executor.rs
git commit -m "feat(code-mode): handle ResolveFutures with sequential dispatch (fake async)"
```

---

## Task 6: Replace Hardcoded `positional_to_named` with Auto-Generated Mapping

**Files:**
- Modify: `crates/daemon/src/agent/code_mode/executor.rs:559-615` (replace `positional_to_named`)

**Step 1: Write a test for auto-generated mapping**

Add to tests in `executor.rs`:

```rust
    #[test]
    fn test_positional_to_named_auto_generated() {
        use super::super::signature::{SignatureCache, build_param_mapping};
        use nevoflux_builtin_wasm::ToolDefinition;

        let tool = ToolDefinition {
            name: "browser_click".into(),
            description: "Click element".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "selector": {"type": "string"},
                    "button": {"type": "string", "default": "left"}
                },
                "required": ["selector"]
            }),
        };
        let mapping = build_param_mapping(&tool);

        // Simulate positional args from Monty
        let args = serde_json::json!(["#submit", "right"]);
        let named = positional_to_named_auto(&mapping, &args);
        assert_eq!(named, serde_json::json!({"selector": "#submit", "button": "right"}));
    }

    #[test]
    fn test_positional_to_named_auto_object_passthrough() {
        let mapping = vec!["url".to_string()];
        let args = serde_json::json!({"url": "https://example.com"});
        let named = positional_to_named_auto(&mapping, &args);
        assert_eq!(named, serde_json::json!({"url": "https://example.com"}));
    }
```

**Step 2: Implement `positional_to_named_auto`**

Add a new function in `executor.rs` (above the old `positional_to_named`):

```rust
/// Convert positional args to named args using auto-generated parameter mapping.
/// If args is already an object, pass through unchanged.
fn positional_to_named_auto(param_names: &[String], args: &serde_json::Value) -> serde_json::Value {
    if args.is_object() {
        return args.clone();
    }
    let arr = match args.as_array() {
        Some(a) => a,
        None => return serde_json::json!({}),
    };
    let mut obj = serde_json::Map::new();
    for (i, val) in arr.iter().enumerate() {
        let key = if i < param_names.len() {
            param_names[i].clone()
        } else {
            format!("arg{}", i)
        };
        obj.insert(key, val.clone());
    }
    serde_json::Value::Object(obj)
}
```

**Step 3: Update `build_registry_and_executor` to use `SignatureCache`**

Modify `build_registry_and_executor` to accept and use param mappings from `SignatureCache` instead of the hardcoded function. The old `positional_to_named` function body can be removed but its call sites need updating.

In the `tool_executor` closure inside `build_registry_and_executor`, replace:
```rust
let named_args = positional_to_named(&name, &args);
```
with:
```rust
let param_names = param_cache.get(&name).cloned().unwrap_or_default();
let named_args = positional_to_named_auto(&param_names, &args);
```

This requires passing `param_mappings` into the closure. Update `build_registry_and_executor` signature to accept `param_mappings: HashMap<String, Vec<String>>`.

**Step 4: Run existing tests to verify no regressions**

Run: `cargo test -p nevoflux-daemon code_mode -- --nocapture`
Expected: All tests pass. Old `positional_to_named` tests may need updating if the function is removed.

**Step 5: Remove old `positional_to_named` and its tests**

Delete the hardcoded `positional_to_named` function (lines 560-614) and its 5 tests (`test_positional_to_named_*`). These are replaced by the auto-generated version.

**Step 6: Run tests again**

Run: `cargo test -p nevoflux-daemon code_mode -- --nocapture`
Expected: All tests pass.

**Step 7: Commit**

```bash
git add crates/daemon/src/agent/code_mode/executor.rs
git commit -m "refactor(code-mode): replace hardcoded positional_to_named with auto-generated param mapping"
```

---

## Task 7: Dynamic Orchestrate Tool Description

**Files:**
- Modify: `crates/builtin-wasm/src/agent.rs:3534-3548` (orchestrate tool definition)
- Modify: `crates/builtin-wasm/src/agent.rs:491` (CODE_MODE_PROMPT)
- Modify: `crates/builtin-wasm/src/agent.rs:629-635` (get_tools_for_mode)
- Modify: `crates/builtin-wasm/src/agent.rs:639-645` (base_prompt_for_mode)

This task makes the `orchestrate` tool description dynamic, containing compact function signatures scoped to the current mode's tool set.

**Step 1: Add a method to generate the orchestrate tool description**

In `agent.rs`, add a new method on `Agent<H>`:

```rust
    /// Generate the orchestrate tool with dynamic compact signatures.
    fn orchestrate_tool(&self, mode: AgentMode) -> ToolDefinition {
        // Get tools available in this mode (excluding orchestrate itself)
        let mode_tools = match mode {
            AgentMode::Chat => self.get_chat_tools(),
            AgentMode::Browser => self.get_browser_tools(),
            AgentMode::Agent | AgentMode::Code => self.get_agent_tools(),
        };

        // Filter out meta-tools that don't make sense inside orchestrate
        let orchestrable: Vec<&ToolDefinition> = mode_tools
            .iter()
            .filter(|t| {
                t.name != "orchestrate"
                    && t.name != "think"
                    && t.name != "plan"
                    && t.name != "create_artifact"
                    && t.name != "switch_model"
                    && t.name != "load_computer_use_tools"
                    && t.name != "subagent_spawn"
                    && t.name != "subagent_wait"
                    && t.name != "subagent_wait_all"
                    && t.name != "subagent_status"
                    && t.name != "subagent_list"
            })
            .collect();

        // Generate compact signatures
        // NOTE: This uses a simplified inline generator since the full
        // SignatureCache lives in the daemon crate (not accessible from builtin-wasm).
        // The compact format is: one function signature per line with inline comment.
        let mut signatures = String::new();
        for tool in &orchestrable {
            // Determine async/sequential based on name
            let is_seq = is_orchestrate_sequential(&tool.name);
            let prefix = if is_seq { "def" } else { "async def" };
            let tag = if is_seq { " [sequential]" } else { "" };

            // Extract param string from input_schema
            let params = extract_params_compact(&tool.input_schema);

            signatures.push_str(&format!(
                "{}  {} {}({}) -> Any  # {}{}\n",
                if signatures.is_empty() { "" } else { "" },
                prefix, tool.name, params, tool.description, tag
            ));
        }

        let description = format!(
            "Orchestrate multiple tool calls in a single Python script. \
             Use when a task needs 3+ tool calls, loops, conditionals, or data transformation.\n\
             \n\
             Available functions (call directly, no import needed):\n\
             {}\n\
             Rules:\n\
             - async def tools can be combined with asyncio.gather() for parallel execution\n\
             - def (sync) tools marked [sequential] must be called one at a time\n\
             - Do NOT use: class, match/case, import, with, yield, decorators\n\
             - Supported: variables, def, async def, if/elif/else, for/while, try/except, \
               comprehensions, f-strings, lambda, asyncio.gather\n\
             - The final expression value is returned as the tool result\n\
             - Use print() for debug output (included in result)",
            signatures.trim()
        );

        ToolDefinition {
            name: "orchestrate".into(),
            description,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "Python code to execute"
                    }
                },
                "required": ["code"]
            }),
        }
    }
```

Helper functions (add as free functions in `agent.rs`):

```rust
/// Check if a tool should be marked sequential in orchestrate signatures.
fn is_orchestrate_sequential(name: &str) -> bool {
    const ASYNC_SAFE: &[&str] = &[
        "browser_screenshot", "browser_get_content", "browser_get_elements",
        "browser_get_element", "browser_query_all", "browser_get_markdown",
        "browser_eval_js", "browser_read_artifact", "browser_get_tabs",
        "browser_query_tabs", "browser_find_elements", "browser_element_info",
        "web_search", "web_fetch", "fetch_page",
        "read_file", "read", "glob", "grep", "list_files",
        "memory_search", "memory_create", "memory_update", "memory_delete",
        "memory_view", "knowledge_teach",
        "tool_search", "tool_call_dynamic",
    ];
    !ASYNC_SAFE.contains(&name)
}

/// Extract a compact parameter string from JSON Schema for tool signatures.
fn extract_params_compact(schema: &serde_json::Value) -> String {
    let props = match schema.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return String::new(),
    };

    let required: std::collections::HashSet<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut req_params = Vec::new();
    let mut opt_params = Vec::new();

    for (name, prop_schema) in props {
        let py_type = schema_to_py_type_compact(prop_schema);
        if required.contains(name.as_str()) {
            req_params.push(format!("{}: {}", name, py_type));
        } else {
            let default = prop_schema
                .get("default")
                .map(|d| match d {
                    serde_json::Value::Null => "None",
                    serde_json::Value::Bool(true) => "True",
                    serde_json::Value::Bool(false) => "False",
                    serde_json::Value::String(s) => return format!("\"{}\"", s),
                    serde_json::Value::Number(n) => return n.to_string(),
                    _ => "None",
                }.to_string())
                .unwrap_or_else(|| "None".to_string());
            opt_params.push(format!("{}: {} = {}", name, py_type, default));
        }
    }

    req_params.extend(opt_params);
    req_params.join(", ")
}

/// Compact JSON Schema -> Python type (no TypedDict, just primitives).
fn schema_to_py_type_compact(schema: &serde_json::Value) -> &'static str {
    match schema.get("type").and_then(|t| t.as_str()) {
        Some("string") => "str",
        Some("integer") => "int",
        Some("number") => "float",
        Some("boolean") => "bool",
        Some("array") => "list",
        Some("object") => "dict",
        _ => "Any",
    }
}
```

**Step 2: Update `get_agent_tools` to use dynamic orchestrate**

In `get_agent_tools()`, replace the hardcoded `orchestrate` tool push (around line 3534-3548) with:

```rust
        // Orchestrate tool with dynamic signatures
        tools.push(self.orchestrate_tool(AgentMode::Agent));
```

Similarly, add `orchestrate` to `get_browser_tools()` if it doesn't already have it.

**Step 3: Update `AgentMode::Code` handling**

In `get_tools_for_mode` (line 634), change:
```rust
AgentMode::Code => self.get_agent_tools(),
```
to:
```rust
AgentMode::Code => self.get_agent_tools(), // Code mode deprecated, uses Agent tools
```

In `base_prompt_for_mode` (line 644), change:
```rust
AgentMode::Code => CODE_MODE_PROMPT,
```
to:
```rust
AgentMode::Code => AGENT_PROMPT, // Code mode deprecated, uses Agent prompt
```

**Step 4: Add system prompt hint**

In `build_system_prompt` or `AGENT_PROMPT`, add the one-liner:
```
When a task involves multiple steps or tools, prefer using the orchestrate tool to batch tool calls in Python.
```

**Step 5: Run tests**

Run: `cargo test -p nevoflux-builtin-wasm -- --nocapture 2>&1 | tail -20`
Expected: All ~168 tests pass.

Run: `cargo test -p nevoflux-daemon -- --nocapture 2>&1 | tail -20`
Expected: All ~928 tests pass.

**Step 6: Commit**

```bash
git add crates/builtin-wasm/src/agent.rs
git commit -m "feat(code-mode): dynamic orchestrate tool description with compact signatures and async/sequential marking"
```

---

## Task 8: Wire LLM Rewrite Callback in `agent_host.rs`

**Files:**
- Modify: `crates/daemon/src/agent_host.rs:2359-2386` (orchestrate handler)
- Modify: `crates/daemon/src/agent/code_mode/executor.rs` (update `execute_python_simple` or add new entry point)

**Step 1: Create an `execute_orchestrate` function that accepts an LLM rewrite callback**

In `executor.rs`, add a new function (or modify `execute_python_simple`):

```rust
/// Execute orchestrate tool with full Code Mode capabilities.
///
/// Unlike `execute_python_simple`, this accepts:
/// - `llm_rewrite`: real LLM callback for error recovery
/// - `param_mappings`: auto-generated from SignatureCache
pub async fn execute_orchestrate(
    code: &str,
    browser_ctx: Option<BrowserContext>,
    param_mappings: HashMap<String, Vec<String>>,
    llm_rewrite: impl Fn(&str) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> + Send + Sync,
) -> CodeModeResult {
    let (external_names, tool_executor) = build_registry_and_executor(browser_ctx, param_mappings);
    let executor = CodeModeExecutor::new();
    executor.execute(code, &external_names, tool_executor, llm_rewrite).await
}
```

**Step 2: Wire the LLM rewrite in `agent_host.rs`**

In the `tool_call_dynamic` method, replace the `execute_python_simple` call (line 2374) with `execute_orchestrate` that includes an LLM rewrite callback. The callback should:

1. Build a repair prompt from the error
2. Call the LLM via existing host function infrastructure
3. Extract the rewritten code from the LLM response

```rust
if tool_name == "orchestrate" {
    let code = arguments.get("code").and_then(|v| v.as_str()).unwrap_or("");
    if code.is_empty() {
        return Err(HostError { code: 100, message: "orchestrate: no code provided".into() });
    }

    let browser_ctx = self.services.as_ref().and_then(|s| s.browser_context());

    // Build param mappings from tool definitions
    // For now, use empty mappings (tools use kwargs from Monty)
    let param_mappings = HashMap::new();

    // LLM rewrite callback using the existing LLM infrastructure
    let llm_rewrite = |prompt: &str| -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
        let prompt = prompt.to_string();
        Box::pin(async move {
            // TODO: Wire through the agent's LLM session for rewrite
            // For now, use a no-op that indicates rewrite is not yet wired
            Err("LLM rewrite not yet wired".to_string())
        })
    };

    let runtime = tokio::runtime::Handle::current();
    let result = tokio::task::block_in_place(|| {
        runtime.block_on(async {
            execute_orchestrate(code, browser_ctx, param_mappings, llm_rewrite).await
        })
    });

    if result.success {
        // Combine print output and final result
        let mut output = result.output;
        // The result field will be added when we update CodeModeResult
        return Ok(output);
    } else {
        return Err(HostError {
            code: 100,
            message: format!("orchestrate failed: {}", result.error.unwrap_or_else(|| "unknown error".into())),
        });
    }
}
```

Note: The full LLM rewrite wiring requires access to the agent's LLM session from inside the host function. This may need a reference to the LLM client stored in the host services. The exact wiring depends on how `self.services` exposes LLM capabilities. Start with the no-op callback and wire the real LLM call as a follow-up within this task.

**Step 3: Run tests**

Run: `cargo test -p nevoflux-daemon agent_host -- --nocapture 2>&1 | tail -20`
Run: `cargo test -p nevoflux-daemon code_mode -- --nocapture 2>&1 | tail -20`
Expected: All tests pass.

**Step 4: Commit**

```bash
git add crates/daemon/src/agent_host.rs crates/daemon/src/agent/code_mode/executor.rs
git commit -m "feat(code-mode): wire orchestrate execution with LLM rewrite callback support"
```

---

## Task 9: Deprecate `AgentMode::Code` and Clean Up

**Files:**
- Modify: `crates/builtin-wasm/src/types.rs:35-36` (deprecation comment)
- Modify: `crates/daemon/src/server.rs:1436,2834` (map Code -> Agent)
- Modify: `crates/daemon/src/agent_host.rs:3807` (already maps Code -> Agent)
- Modify: `crates/builtin-wasm/src/agent.rs:491` (remove CODE_MODE_PROMPT)

**Step 1: Mark `AgentMode::Code` as deprecated**

In `crates/builtin-wasm/src/types.rs`, update the Code variant:
```rust
    /// Code mode - DEPRECATED. Maps to Agent mode.
    /// Kept for protocol backward compatibility. Will be removed in next major version.
    Code,
```

**Step 2: Map Code -> Agent in server**

In `crates/daemon/src/server.rs`, at lines 1436 and 2834, change:
```rust
"code" => AgentMode::Code,
```
to:
```rust
"code" => AgentMode::Agent, // Code mode deprecated, maps to Agent
```

**Step 3: Remove `CODE_MODE_PROMPT`**

In `crates/builtin-wasm/src/agent.rs`, remove the `CODE_MODE_PROMPT` constant (line 491). Update `base_prompt_for_mode` to map `Code` to `AGENT_PROMPT` (already done in Task 7).

**Step 4: Run full test suite**

Run: `cargo test --workspace 2>&1 | tail -5`
Expected: All tests pass. Some tests that explicitly use `AgentMode::Code` may need updating.

Check for test files that use `AgentMode::Code`:
- `tests/e2e_tests.rs:85`
- `tests/agent_runner_tests.rs:55`
- `tests/agent_loop_tests.rs:167,192,218`

Update these tests: `AgentMode::Code` should now behave identically to `AgentMode::Agent`.

**Step 5: Commit**

```bash
git add crates/builtin-wasm/src/types.rs crates/daemon/src/server.rs \
       crates/builtin-wasm/src/agent.rs tests/
git commit -m "refactor(code-mode): deprecate AgentMode::Code, map to Agent mode"
```

---

## Task 10: Integration Test — End-to-End Orchestrate

**Files:**
- Modify: `crates/daemon/src/agent/code_mode/executor.rs` (add integration test)

**Step 1: Write an integration test that exercises the full pipeline**

```rust
    #[tokio::test]
    async fn test_orchestrate_full_pipeline() {
        // Simulate a multi-tool orchestration: fetch two URLs and combine results
        let executor = CodeModeExecutor::new();
        let result = executor
            .execute(
                r#"
a = fetch("https://a.com")
b = fetch("https://b.com")
combined = a + " | " + b
print(combined)
combined
"#,
                &["fetch".to_string()],
                |name, args| {
                    let args = args.clone();
                    Box::pin(async move {
                        let url = args.as_array()
                            .and_then(|a| a.first())
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        Ok(serde_json::json!(format!("content from {}", url)))
                    })
                },
                |_prompt| Box::pin(async { Err("no rewrite".to_string()) }),
            )
            .await;

        assert!(result.success, "Expected success, got: {:?}", result.error);
        assert_eq!(result.tool_results.len(), 2);
        assert!(result.output.contains("content from"));
    }

    #[tokio::test]
    async fn test_orchestrate_auto_fix_import() {
        // Verify auto-fixer strips imports before Monty execution
        let executor = CodeModeExecutor::new();
        let result = executor
            .execute(
                "import json\nx = 42\nprint(x)",
                &[],
                |_name, _args| Box::pin(async { Ok(serde_json::json!("ok")) }),
                |_prompt| Box::pin(async { Err("no rewrite".to_string()) }),
            )
            .await;

        assert!(result.success, "Expected success, got: {:?}", result.error);
        assert!(result.output.contains("42"));
    }
```

**Step 2: Run integration tests**

Run: `cargo test -p nevoflux-daemon test_orchestrate -- --nocapture`
Expected: Both tests pass.

**Step 3: Run full CI check**

Run: `just ci`
Expected: fmt + clippy + all tests pass.

**Step 4: Commit**

```bash
git add crates/daemon/src/agent/code_mode/executor.rs
git commit -m "test(code-mode): add end-to-end orchestrate integration tests"
```

---

## Summary

| Task | Description | Key Files |
|------|-------------|-----------|
| 1 | Upgrade Monty v0.0.4 -> v0.0.7 | `Cargo.toml`, `Cargo.lock` |
| 2 | Type mapping pipeline (`json_schema_to_python_type`) | `signature.rs` (new) |
| 3 | Compact & full signature generation | `signature.rs` |
| 4 | `SignatureCache` + `build_param_mapping` | `signature.rs` |
| 5 | Handle `ResolveFutures` (fake async) | `executor.rs` |
| 6 | Replace hardcoded `positional_to_named` | `executor.rs` |
| 7 | Dynamic orchestrate tool description | `agent.rs` (builtin-wasm) |
| 8 | Wire LLM rewrite callback | `agent_host.rs`, `executor.rs` |
| 9 | Deprecate `AgentMode::Code` | `types.rs`, `server.rs`, `agent.rs` |
| 10 | End-to-end integration test | `executor.rs` |
