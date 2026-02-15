//! CodeModeExecutor - Monty execution loop with external function routing.
//! Runs auto-fix -> lint -> execute -> retry pipeline.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use monty::{
    CollectStringPrint, LimitedTracker, MontyObject, MontyRun, ResourceLimits, RunProgress,
};

use super::auto_fixer::MontyAutoFixer;
use super::linter::MontyLinter;
use super::repair_prompt::RepairPrompt;

/// Maximum number of retries (rewrite attempts) before giving up.
const MAX_RETRIES: u32 = 2;

/// Result of a Code Mode execution.
#[derive(Debug)]
pub struct CodeModeResult {
    /// Final output from print() statements during execution.
    pub output: String,
    /// Tool call results collected during execution.
    pub tool_results: Vec<ToolCallResult>,
    /// Whether execution completed successfully.
    pub success: bool,
    /// Error message if execution failed.
    pub error: Option<String>,
    /// Number of retries used (0 = first attempt succeeded).
    pub retries: u32,
}

/// A tool call made during Python execution.
#[derive(Debug, Clone)]
pub struct ToolCallResult {
    pub tool_name: String,
    pub arguments: serde_json::Value,
    pub result: serde_json::Value,
}

/// Convert a `MontyObject` to a `serde_json::Value`.
fn monty_object_to_json(obj: &MontyObject) -> serde_json::Value {
    match obj {
        MontyObject::None => serde_json::Value::Null,
        MontyObject::Bool(b) => serde_json::Value::Bool(*b),
        MontyObject::Int(i) => serde_json::json!(*i),
        MontyObject::BigInt(bi) => {
            // Try to fit into i64 first, otherwise use string representation
            if let Ok(i) = i64::try_from(bi) {
                serde_json::json!(i)
            } else {
                serde_json::Value::String(bi.to_string())
            }
        }
        MontyObject::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        MontyObject::String(s) => serde_json::Value::String(s.clone()),
        MontyObject::List(items) => {
            serde_json::Value::Array(items.iter().map(monty_object_to_json).collect())
        }
        MontyObject::Tuple(items) => {
            serde_json::Value::Array(items.iter().map(monty_object_to_json).collect())
        }
        MontyObject::Dict(pairs) => {
            let mut map = serde_json::Map::new();
            for (k, v) in pairs {
                let key = match k {
                    MontyObject::String(s) => s.clone(),
                    other => other.to_string(),
                };
                map.insert(key, monty_object_to_json(v));
            }
            serde_json::Value::Object(map)
        }
        MontyObject::Bytes(b) => {
            serde_json::Value::Array(b.iter().map(|byte| serde_json::json!(*byte)).collect())
        }
        MontyObject::Set(items) | MontyObject::FrozenSet(items) => {
            serde_json::Value::Array(items.iter().map(monty_object_to_json).collect())
        }
        MontyObject::NamedTuple { values, .. } => {
            serde_json::Value::Array(values.iter().map(monty_object_to_json).collect())
        }
        MontyObject::Path(p) => serde_json::Value::String(p.clone()),
        // For all other variants, use debug/repr formatting
        other => serde_json::Value::String(format!("{other}")),
    }
}

/// Convert a `serde_json::Value` to a `MontyObject`.
fn json_to_monty_object(val: &serde_json::Value) -> MontyObject {
    match val {
        serde_json::Value::Null => MontyObject::None,
        serde_json::Value::Bool(b) => MontyObject::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                MontyObject::Int(i)
            } else if let Some(f) = n.as_f64() {
                MontyObject::Float(f)
            } else {
                // u64 that doesn't fit in i64
                MontyObject::String(n.to_string())
            }
        }
        serde_json::Value::String(s) => MontyObject::String(s.clone()),
        serde_json::Value::Array(arr) => {
            MontyObject::List(arr.iter().map(json_to_monty_object).collect())
        }
        serde_json::Value::Object(_) => {
            // Serialize the object as a JSON string for simplicity,
            // since MontyObject::Dict requires DictPairs which is complex
            // to construct from the public API.
            MontyObject::String(serde_json::to_string(val).unwrap_or_default())
        }
    }
}

/// Core execution engine for Agent Code Mode.
///
/// Orchestrates the four-layer constraint pipeline:
/// 1. Prompt constraint (handled externally by system prompt)
/// 2. `MontyAutoFixer` - mechanical text transforms
/// 3. `MontyLinter` - regex-based violation detection
/// 4. Monty interpreter execution with external function routing
///
/// On lint violations or runtime errors, the executor can request the LLM
/// to rewrite the code, up to `MAX_RETRIES` times.
#[derive(Default)]
pub struct CodeModeExecutor;

impl CodeModeExecutor {
    pub fn new() -> Self {
        Self
    }

    /// Execute Python code through the full pipeline.
    ///
    /// Pipeline: auto-fix -> lint -> monty execute -> retry on error
    ///
    /// # Arguments
    /// * `code` - Raw Python code from LLM
    /// * `external_function_names` - Names of tool functions available to the code
    /// * `tool_executor` - Async callback to execute tool calls via ToolRegistry
    /// * `llm_rewrite` - Async callback to ask LLM to rewrite code given a repair prompt
    pub async fn execute<F, R>(
        &self,
        code: &str,
        external_function_names: &[String],
        tool_executor: F,
        llm_rewrite: R,
    ) -> CodeModeResult
    where
        F: Fn(
                &str,
                serde_json::Value,
            )
                -> Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send>>
            + Send
            + Sync,
        R: Fn(&str) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> + Send + Sync,
    {
        let mut current_code = code.to_string();
        let mut retries: u32 = 0;

        loop {
            // Layer 2: Auto-fix mechanical transforms
            let auto_fixed = MontyAutoFixer::fix(&current_code);

            // Layer 3: Lint for unsupported constructs
            let violations = MontyLinter::check(&auto_fixed);
            if !violations.is_empty() {
                if retries >= MAX_RETRIES {
                    let violation_msgs: Vec<String> = violations
                        .iter()
                        .map(|v| format!("Line {}: `{}` - {}", v.line, v.construct, v.suggestion))
                        .collect();
                    return CodeModeResult {
                        output: String::new(),
                        tool_results: Vec::new(),
                        success: false,
                        error: Some(format!(
                            "Code has unsupported constructs after {} retries: {}",
                            retries,
                            violation_msgs.join("; ")
                        )),
                        retries,
                    };
                }

                let repair_prompt = RepairPrompt::from_violations(&auto_fixed, &violations);
                match llm_rewrite(&repair_prompt).await {
                    Ok(rewritten) => {
                        current_code = rewritten;
                        retries += 1;
                        continue;
                    }
                    Err(e) => {
                        return CodeModeResult {
                            output: String::new(),
                            tool_results: Vec::new(),
                            success: false,
                            error: Some(format!("LLM rewrite failed for lint violations: {e}")),
                            retries,
                        };
                    }
                }
            }

            // Layer 4: Execute with Monty interpreter
            let runner = match MontyRun::new(
                auto_fixed.clone(),
                "code_mode.py",
                vec![],
                external_function_names.to_vec(),
            ) {
                Ok(runner) => runner,
                Err(exc) => {
                    let error_msg = exc.message().unwrap_or("parse error").to_string();
                    let error_type = format!("{}", exc.exc_type());
                    tracing::warn!(
                        "Code Mode: parse error (retry {}/{}): {}: {}",
                        retries,
                        MAX_RETRIES,
                        error_type,
                        error_msg
                    );

                    if retries < MAX_RETRIES {
                        // Extract line number from traceback if available
                        let line = exc.traceback().last().map(|f| f.start.line as usize);
                        let repair_prompt = RepairPrompt::from_runtime_error(
                            &auto_fixed,
                            &error_type,
                            &error_msg,
                            line,
                        );
                        tracing::info!("Code Mode: requesting LLM rewrite for parse error");
                        match llm_rewrite(&repair_prompt).await {
                            Ok(rewritten) => {
                                tracing::info!(
                                    "Code Mode: LLM rewrite succeeded ({} bytes)",
                                    rewritten.len()
                                );
                                current_code = rewritten;
                                retries += 1;
                                continue;
                            }
                            Err(e) => {
                                tracing::error!("Code Mode: LLM rewrite failed: {}", e);
                                return CodeModeResult {
                                    output: String::new(),
                                    tool_results: Vec::new(),
                                    success: false,
                                    error: Some(format!(
                                        "Parse error and LLM rewrite failed: {error_type}: {error_msg} (rewrite error: {e})"
                                    )),
                                    retries,
                                };
                            }
                        }
                    }

                    return CodeModeResult {
                        output: String::new(),
                        tool_results: Vec::new(),
                        success: false,
                        error: Some(format!("{error_type}: {error_msg}")),
                        retries,
                    };
                }
            };

            let limits = ResourceLimits {
                max_allocations: Some(100_000),
                max_duration: Some(Duration::from_secs(30)),
                max_memory: Some(64 * 1024 * 1024), // 64MB
                gc_interval: Some(10_000),
                max_recursion_depth: Some(100),
            };
            let resource_tracker = LimitedTracker::new(limits);
            let mut print_writer = CollectStringPrint::new();
            let mut tool_results: Vec<ToolCallResult> = Vec::new();

            let start_result = runner.start(vec![], resource_tracker, &mut print_writer);

            let mut progress = match start_result {
                Ok(p) => p,
                Err(exc) => {
                    let error_msg = exc.message().unwrap_or("runtime error").to_string();
                    let error_type = format!("{}", exc.exc_type());

                    if retries < MAX_RETRIES {
                        let line = exc.traceback().last().map(|f| f.start.line as usize);
                        let repair_prompt = RepairPrompt::from_runtime_error(
                            &auto_fixed,
                            &error_type,
                            &error_msg,
                            line,
                        );
                        match llm_rewrite(&repair_prompt).await {
                            Ok(rewritten) => {
                                current_code = rewritten;
                                retries += 1;
                                continue;
                            }
                            Err(e) => {
                                return CodeModeResult {
                                    output: print_writer.into_output(),
                                    tool_results,
                                    success: false,
                                    error: Some(format!(
                                        "Runtime error and LLM rewrite failed: {error_type}: {error_msg} (rewrite error: {e})"
                                    )),
                                    retries,
                                };
                            }
                        }
                    }

                    return CodeModeResult {
                        output: print_writer.into_output(),
                        tool_results,
                        success: false,
                        error: Some(format!("{error_type}: {error_msg}")),
                        retries,
                    };
                }
            };

            // Execution loop: handle function calls until completion
            loop {
                match progress {
                    RunProgress::FunctionCall {
                        function_name,
                        args,
                        kwargs: _,
                        call_id: _,
                        state,
                    } => {
                        // Convert args to JSON
                        let json_args: Vec<serde_json::Value> =
                            args.iter().map(monty_object_to_json).collect();
                        let arguments = serde_json::Value::Array(json_args);

                        // Execute the tool
                        let tool_result = tool_executor(&function_name, arguments.clone()).await;

                        let (result_json, return_obj) = match tool_result {
                            Ok(result_val) => {
                                let return_obj = json_to_monty_object(&result_val);
                                (result_val, return_obj)
                            }
                            Err(e) => {
                                let error_val = serde_json::json!({
                                    "error": e
                                });
                                let return_obj = MontyObject::String(format!("Error: {e}"));
                                (error_val, return_obj)
                            }
                        };

                        tool_results.push(ToolCallResult {
                            tool_name: function_name,
                            arguments,
                            result: result_json,
                        });

                        // Resume execution with the result
                        match state.run(return_obj, &mut print_writer) {
                            Ok(next) => {
                                progress = next;
                            }
                            Err(exc) => {
                                let error_msg =
                                    exc.message().unwrap_or("runtime error").to_string();
                                let error_type = format!("{}", exc.exc_type());

                                if retries < MAX_RETRIES {
                                    let line =
                                        exc.traceback().last().map(|f| f.start.line as usize);
                                    let repair_prompt = RepairPrompt::from_runtime_error(
                                        &auto_fixed,
                                        &error_type,
                                        &error_msg,
                                        line,
                                    );
                                    match llm_rewrite(&repair_prompt).await {
                                        Ok(rewritten) => {
                                            current_code = rewritten;
                                            retries += 1;
                                            break; // break inner loop to restart outer loop
                                        }
                                        Err(e) => {
                                            return CodeModeResult {
                                                output: print_writer.into_output(),
                                                tool_results,
                                                success: false,
                                                error: Some(format!(
                                                    "Runtime error after tool call and LLM rewrite failed: {error_type}: {error_msg} (rewrite error: {e})"
                                                )),
                                                retries,
                                            };
                                        }
                                    }
                                }

                                return CodeModeResult {
                                    output: print_writer.into_output(),
                                    tool_results,
                                    success: false,
                                    error: Some(format!("{error_type}: {error_msg}")),
                                    retries,
                                };
                            }
                        }
                    }
                    RunProgress::Complete(_value) => {
                        return CodeModeResult {
                            output: print_writer.into_output(),
                            tool_results,
                            success: true,
                            error: None,
                            retries,
                        };
                    }
                    RunProgress::OsCall { .. } => {
                        return CodeModeResult {
                            output: print_writer.into_output(),
                            tool_results,
                            success: false,
                            error: Some(
                                "OS calls are not permitted in sandboxed execution".to_string(),
                            ),
                            retries,
                        };
                    }
                    RunProgress::ResolveFutures(_) => {
                        return CodeModeResult {
                            output: print_writer.into_output(),
                            tool_results,
                            success: false,
                            error: Some("Async futures are not supported in Code Mode".to_string()),
                            retries,
                        };
                    }
                }
            }
            // If we broke out of the inner loop (retry after tool call error),
            // continue the outer loop.
        }
    }
}

// ============================================================================
// Integration function: wires CodeModeExecutor into the daemon response path
// ============================================================================

use crate::agent::tools::ToolRegistry;
use crate::wasm::llm::{execute_llm_chat, LlmChatRequest, LlmMessage};
use nevoflux_llm::ProviderType;
use std::str::FromStr;

/// Execute Code Mode: extract Python from text, run through 4-layer pipeline.
///
/// Returns `Some(CodeModeResult)` if a Python block was found and executed,
/// or `None` if no Python block was found in the text.
pub async fn execute_code_mode(
    text: &str,
    config: &crate::config::AgentConfig,
) -> Option<CodeModeResult> {
    // Extract Python block from LLM response
    let python_code = crate::agent::runner::extract_python_block(text)?;
    tracing::info!(
        "Code Mode: extracted python block ({} bytes), first 200 chars: {:?}",
        python_code.len(),
        &python_code[..python_code.len().min(200)]
    );

    let registry = ToolRegistry::new();
    let external_names: Vec<String> = registry
        .tool_names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    let executor = CodeModeExecutor::new();

    // Tool executor callback: dispatches to ToolRegistry
    let tool_executor =
        |name: &str,
         args: serde_json::Value|
         -> Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send>> {
            let name = name.to_string();
            let args = args.clone();
            Box::pin(async move {
                let registry = ToolRegistry::new();
                let named_args = positional_to_named(&name, &args);
                let call = crate::agent::abi::PendingToolCall {
                    id: format!("code-mode-{}", uuid_simple()),
                    name: name.clone(),
                    arguments: named_args,
                };
                let result = registry.execute(&call).await;
                if let Some(error) = result.error {
                    Err(error)
                } else {
                    let content = result.content.unwrap_or_default();
                    match serde_json::from_str::<serde_json::Value>(&content) {
                        Ok(val) => Ok(val),
                        Err(_) => Ok(serde_json::Value::String(content)),
                    }
                }
            })
        };

    // LLM rewrite callback: sends repair prompt to the LLM
    let provider_name = config
        .llm
        .active_provider()
        .unwrap_or("anthropic")
        .to_string();
    let api_key = config.llm.active_api_key().unwrap_or("").to_string();
    let model = config
        .llm
        .active_model()
        .unwrap_or("gpt-4o-mini")
        .to_string();

    let llm_rewrite =
        move |prompt: &str| -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
            let prompt = prompt.to_string();
            let provider_name = provider_name.clone();
            let api_key = api_key.clone();
            let model = model.clone();
            Box::pin(async move {
                if api_key.is_empty() {
                    return Err("No API key configured for LLM rewrite".to_string());
                }
                let provider = ProviderType::from_str(&provider_name)
                    .map_err(|_| format!("Invalid provider: {provider_name}"))?;
                let request = LlmChatRequest {
                    messages: vec![LlmMessage::user(&prompt)],
                    system: None,
                    temperature: Some(0.0),
                    max_tokens: Some(4096),
                    tools: None,
                };
                let response = execute_llm_chat(provider, &api_key, &model, request)
                    .await
                    .map_err(|e| format!("LLM rewrite failed: {e}"))?;
                // Extract Python code from the rewrite response, or use raw content
                let code = crate::agent::runner::extract_python_block(&response.content)
                    .unwrap_or(response.content);
                Ok(code)
            })
        };

    let result = executor
        .execute(&python_code, &external_names, tool_executor, llm_rewrite)
        .await;
    tracing::info!(
        "Code Mode: execution complete. success={}, retries={}, output_len={}, error={:?}",
        result.success,
        result.retries,
        result.output.len(),
        result.error
    );
    Some(result)
}

/// Convert positional args (JSON array from Monty) to named args (JSON object for ToolRegistry).
fn positional_to_named(tool_name: &str, args: &serde_json::Value) -> serde_json::Value {
    if args.is_object() {
        return args.clone();
    }
    let arr = match args.as_array() {
        Some(a) => a,
        None => return serde_json::json!({}),
    };
    let param_names: &[&str] = match tool_name {
        "read_file" => &["path"],
        "write_file" => &["path", "content"],
        "list_files" => &["path"],
        "canvas_render" => &["files", "entry"],
        "web_search" => &["query"],
        "fetch_page" => &["url"],
        "run_command" => &["command"],
        "get_code_mode_context" => &[],
        _ => &[],
    };
    let mut obj = serde_json::Map::new();
    for (i, val) in arr.iter().enumerate() {
        let key = if i < param_names.len() {
            param_names[i].to_string()
        } else {
            format!("arg{}", i)
        };
        obj.insert(key, val.clone());
    }
    serde_json::Value::Object(obj)
}

/// Generate a simple unique ID (timestamp + nanos).
fn uuid_simple() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}-{:x}", now.as_millis(), now.subsec_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_monty_object_to_json() {
        assert_eq!(
            monty_object_to_json(&MontyObject::None),
            serde_json::Value::Null
        );
        assert_eq!(
            monty_object_to_json(&MontyObject::Int(42)),
            serde_json::json!(42)
        );
        assert_eq!(
            monty_object_to_json(&MontyObject::Bool(true)),
            serde_json::json!(true)
        );
        assert_eq!(
            monty_object_to_json(&MontyObject::String("hello".to_string())),
            serde_json::json!("hello")
        );
        assert_eq!(
            monty_object_to_json(&MontyObject::Float(3.14)),
            serde_json::json!(3.14)
        );
        assert_eq!(
            monty_object_to_json(&MontyObject::List(vec![
                MontyObject::Int(1),
                MontyObject::Int(2),
            ])),
            serde_json::json!([1, 2])
        );
    }

    #[test]
    fn test_json_to_monty_object() {
        assert!(matches!(
            json_to_monty_object(&serde_json::json!(null)),
            MontyObject::None
        ));
        assert!(matches!(
            json_to_monty_object(&serde_json::json!(42)),
            MontyObject::Int(42)
        ));
        assert!(matches!(
            json_to_monty_object(&serde_json::json!(true)),
            MontyObject::Bool(true)
        ));
        match json_to_monty_object(&serde_json::json!("hello")) {
            MontyObject::String(s) => assert_eq!(s, "hello"),
            other => panic!("Expected String, got {:?}", other),
        }
        match json_to_monty_object(&serde_json::json!(3.14)) {
            MontyObject::Float(f) => assert!((f - 3.14).abs() < f64::EPSILON),
            other => panic!("Expected Float, got {:?}", other),
        }
    }

    #[test]
    fn test_monty_object_to_json_tuple() {
        let tuple = MontyObject::Tuple(vec![MontyObject::Int(1), MontyObject::String("a".into())]);
        assert_eq!(monty_object_to_json(&tuple), serde_json::json!([1, "a"]));
    }

    #[test]
    fn test_monty_object_to_json_nan() {
        // NaN cannot be represented in JSON, should map to null
        assert_eq!(
            monty_object_to_json(&MontyObject::Float(f64::NAN)),
            serde_json::Value::Null
        );
    }

    #[test]
    fn test_auto_fix_applied() {
        // Verify that auto-fixer runs: code with `import os` should have it stripped
        let code = "import os\nx = 1 + 2\nprint(x)";
        let fixed = MontyAutoFixer::fix(code);
        assert!(!fixed.contains("import os"));
        assert!(fixed.contains("x = 1 + 2"));
    }

    #[tokio::test]
    async fn test_simple_execution() {
        let executor = CodeModeExecutor::new();
        let result = executor
            .execute(
                "x = 1 + 2\nprint(x)",
                &[],
                |_name, _args| Box::pin(async { Ok(serde_json::json!("ok")) }),
                |_prompt| Box::pin(async { Err("no rewrite".to_string()) }),
            )
            .await;
        assert!(
            result.success,
            "Expected success, got error: {:?}",
            result.error
        );
        assert!(
            result.output.contains('3'),
            "Expected output to contain '3', got: {:?}",
            result.output
        );
        assert_eq!(result.retries, 0);
    }

    #[tokio::test]
    async fn test_lint_violation_triggers_rewrite() {
        let executor = CodeModeExecutor::new();
        let result = executor
            .execute(
                "class Foo:\n    pass",
                &[],
                |_name, _args| Box::pin(async { Ok(serde_json::json!("ok")) }),
                |_prompt| {
                    Box::pin(async {
                        // Return valid code without class
                        Ok("x = {\"type\": \"Foo\"}\nprint(x)".to_string())
                    })
                },
            )
            .await;
        assert!(
            result.success,
            "Expected success, got error: {:?}",
            result.error
        );
        assert!(result.retries >= 1);
    }

    #[tokio::test]
    async fn test_external_function_call() {
        let executor = CodeModeExecutor::new();
        let result = executor
            .execute(
                "result = web_search(\"test\")\nprint(result)",
                &["web_search".to_string()],
                |name, _args| {
                    let name = name.to_string();
                    Box::pin(async move {
                        assert_eq!(name, "web_search");
                        Ok(serde_json::json!("search results"))
                    })
                },
                |_prompt| Box::pin(async { Err("no rewrite".to_string()) }),
            )
            .await;
        assert!(
            result.success,
            "Expected success, got error: {:?}",
            result.error
        );
        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0].tool_name, "web_search");
    }

    #[tokio::test]
    async fn test_max_retries_exceeded() {
        let executor = CodeModeExecutor::new();
        let result = executor
            .execute(
                "class Foo:\n    pass",
                &[],
                |_name, _args| Box::pin(async { Ok(serde_json::json!("ok")) }),
                |_prompt| {
                    Box::pin(async {
                        // Always return code with class (never fixes it)
                        Ok("class Bar:\n    pass".to_string())
                    })
                },
            )
            .await;
        assert!(!result.success);
        assert!(result.error.is_some());
        assert_eq!(result.retries, MAX_RETRIES);
    }

    #[tokio::test]
    async fn test_import_auto_stripped_before_execution() {
        let executor = CodeModeExecutor::new();
        let result = executor
            .execute(
                "import os\nimport sys\nx = 10\nprint(x)",
                &[],
                |_name, _args| Box::pin(async { Ok(serde_json::json!("ok")) }),
                |_prompt| Box::pin(async { Err("no rewrite".to_string()) }),
            )
            .await;
        assert!(
            result.success,
            "Expected success, got error: {:?}",
            result.error
        );
        assert!(result.output.contains("10"));
        assert_eq!(result.retries, 0);
    }

    #[tokio::test]
    async fn test_multiple_tool_calls() {
        let executor = CodeModeExecutor::new();
        let result = executor
            .execute(
                "a = tool_a(\"x\")\nb = tool_b(\"y\")\nprint(a, b)",
                &["tool_a".to_string(), "tool_b".to_string()],
                |name, _args| {
                    let name = name.to_string();
                    Box::pin(async move { Ok(serde_json::json!(format!("result_{}", name))) })
                },
                |_prompt| Box::pin(async { Err("no rewrite".to_string()) }),
            )
            .await;
        assert!(
            result.success,
            "Expected success, got error: {:?}",
            result.error
        );
        assert_eq!(result.tool_results.len(), 2);
        assert_eq!(result.tool_results[0].tool_name, "tool_a");
        assert_eq!(result.tool_results[1].tool_name, "tool_b");
    }

    #[tokio::test]
    async fn test_empty_code() {
        let executor = CodeModeExecutor::new();
        let result = executor
            .execute(
                "",
                &[],
                |_name, _args| Box::pin(async { Ok(serde_json::json!("ok")) }),
                |_prompt| Box::pin(async { Err("no rewrite".to_string()) }),
            )
            .await;
        // Empty code should either succeed with no output or fail gracefully
        // (depends on Monty behavior with empty input)
        // Just verify it doesn't panic
        assert!(result.output.is_empty() || result.success || result.error.is_some());
    }

    #[test]
    fn test_positional_to_named_read_file() {
        let args = serde_json::json!(["/tmp/test.txt"]);
        let named = positional_to_named("read_file", &args);
        assert_eq!(named, serde_json::json!({"path": "/tmp/test.txt"}));
    }

    #[test]
    fn test_positional_to_named_write_file() {
        let args = serde_json::json!(["/tmp/out.txt", "hello"]);
        let named = positional_to_named("write_file", &args);
        assert_eq!(
            named,
            serde_json::json!({"path": "/tmp/out.txt", "content": "hello"})
        );
    }

    #[test]
    fn test_positional_to_named_object_passthrough() {
        let args = serde_json::json!({"path": "/test"});
        let named = positional_to_named("read_file", &args);
        assert_eq!(named, serde_json::json!({"path": "/test"}));
    }

    #[test]
    fn test_positional_to_named_canvas_render() {
        let files = serde_json::json!({"index.tsx": "code"});
        let args = serde_json::json!([files, "index.tsx"]);
        let named = positional_to_named("canvas_render", &args);
        assert_eq!(named["files"], serde_json::json!({"index.tsx": "code"}));
        assert_eq!(named["entry"], serde_json::json!("index.tsx"));
    }

    #[test]
    fn test_positional_to_named_unknown_tool() {
        let args = serde_json::json!(["a", "b"]);
        let named = positional_to_named("unknown_tool", &args);
        assert_eq!(named, serde_json::json!({"arg0": "a", "arg1": "b"}));
    }
}
