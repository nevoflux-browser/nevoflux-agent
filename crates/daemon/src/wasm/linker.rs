//! Host function linker for Wasm guest modules.
//!
//! This module provides the host functions that Wasm guest modules can call
//! to interact with the NevoFlux daemon.

use wasmtime::{Caller, Engine, Linker};

use crate::error::{DaemonError, Result};
use crate::wasm::services::HostServices;

/// Initial capacity for the memory buffer (1MB).
const MEMORY_BUFFER_CAPACITY: usize = 1024 * 1024;

/// Host state for Wasm guest modules.
///
/// This struct holds the state that is accessible to host functions
/// when called from Wasm guest modules.
pub struct HostState {
    /// Memory buffer for passing data between host and guest.
    pub memory_buffer: Vec<u8>,

    /// Last error message from host functions.
    last_error: Option<String>,

    /// Optional services for host functions.
    ///
    /// When set, host functions can access the database, skills registry,
    /// and other services provided by the daemon.
    pub services: Option<HostServices>,
}

impl Default for HostState {
    fn default() -> Self {
        Self::new()
    }
}

impl HostState {
    /// Create a new HostState with default values.
    pub fn new() -> Self {
        Self {
            memory_buffer: Vec::with_capacity(MEMORY_BUFFER_CAPACITY),
            last_error: None,
            services: None,
        }
    }

    /// Set the services for host functions.
    ///
    /// This enables host functions to access the database, skills registry,
    /// and other services provided by the daemon.
    ///
    /// # Arguments
    ///
    /// * `services` - The services container to attach.
    ///
    /// # Returns
    ///
    /// Returns self for method chaining.
    pub fn with_services(mut self, services: HostServices) -> Self {
        self.services = Some(services);
        self
    }

    /// Set the last error message.
    pub fn set_error(&mut self, error: impl Into<String>) {
        self.last_error = Some(error.into());
    }

    /// Take the last error message, clearing it.
    pub fn take_error(&mut self) -> Option<String> {
        self.last_error.take()
    }

    /// Check if services are available.
    pub fn has_services(&self) -> bool {
        self.services.is_some()
    }
}

impl std::fmt::Debug for HostState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostState")
            .field("memory_buffer_capacity", &self.memory_buffer.capacity())
            .field("memory_buffer_len", &self.memory_buffer.len())
            .field("last_error", &self.last_error)
            .field("services", &self.services.as_ref().map(|_| "Some(...)"))
            .finish()
    }
}

/// Create a Linker with host functions registered under the "nevoflux" namespace.
///
/// # Arguments
///
/// * `engine` - The Wasmtime engine to create the linker for.
///
/// # Returns
///
/// A configured Linker with all host functions registered.
///
/// # Errors
///
/// Returns an error if any host function fails to register.
pub fn create_linker(engine: &Engine) -> Result<Linker<HostState>> {
    let mut linker = Linker::new(engine);

    // Register get_error_len: returns the length of the last error message
    linker
        .func_wrap(
            "nevoflux",
            "get_error_len",
            |caller: Caller<'_, HostState>| -> i32 {
                caller
                    .data()
                    .last_error
                    .as_ref()
                    .map(|e| e.len() as i32)
                    .unwrap_or(0)
            },
        )
        .map_err(|e| {
            DaemonError::InternalError(format!("Failed to register get_error_len: {}", e))
        })?;

    // Register get_error: copies the error message to guest memory
    linker
        .func_wrap(
            "nevoflux",
            "get_error",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i32 {
                let error = match caller.data().last_error.as_ref() {
                    Some(e) => e.clone(),
                    None => return 0,
                };

                let error_bytes = error.as_bytes();
                let copy_len = std::cmp::min(error_bytes.len(), len as usize);

                // Get the memory export from the guest
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => {
                        tracing::error!("Guest module has no memory export");
                        return -1;
                    }
                };

                // Write error bytes to guest memory
                match memory.write(&mut caller, ptr as usize, &error_bytes[..copy_len]) {
                    Ok(()) => copy_len as i32,
                    Err(e) => {
                        tracing::error!("Failed to write error to guest memory: {}", e);
                        -1
                    }
                }
            },
        )
        .map_err(|e| DaemonError::InternalError(format!("Failed to register get_error: {}", e)))?;

    // Register llm_chat: performs LLM chat completion
    // llm_chat: request_ptr, request_len, response_ptr, response_len -> bytes written or -1
    linker
        .func_wrap(
            "nevoflux",
            "llm_chat",
            |mut caller: Caller<'_, HostState>,
             request_ptr: i32,
             request_len: i32,
             response_ptr: i32,
             response_len: i32|
             -> i32 {
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => {
                        tracing::error!("Guest module has no memory export");
                        return -1;
                    }
                };

                // Read JSON request from guest memory
                let mut request_buf = vec![0u8; request_len as usize];
                if memory
                    .read(&caller, request_ptr as usize, &mut request_buf)
                    .is_err()
                {
                    caller
                        .data_mut()
                        .set_error("Failed to read request from memory");
                    return -1;
                }

                // Parse the request JSON
                let request_str = match String::from_utf8(request_buf) {
                    Ok(s) => s,
                    Err(_) => {
                        caller.data_mut().set_error("Invalid UTF-8 in request");
                        return -1;
                    }
                };

                let request: crate::wasm::llm::LlmChatRequest =
                    match serde_json::from_str(&request_str) {
                        Ok(r) => r,
                        Err(e) => {
                            caller
                                .data_mut()
                                .set_error(format!("Failed to parse request: {}", e));
                            return -1;
                        }
                    };

                // Get LLM config from services
                let services = match &caller.data().services {
                    Some(s) => s.clone(),
                    None => {
                        caller.data_mut().set_error("Services not available");
                        return -1;
                    }
                };

                let llm_config = match &services.llm_config {
                    Some(c) => c.clone(),
                    None => {
                        caller.data_mut().set_error("LLM not configured");
                        return -1;
                    }
                };

                // Execute the LLM chat using the current tokio runtime
                let result = match tokio::runtime::Handle::try_current() {
                    Ok(handle) => std::thread::scope(|s| {
                        s.spawn(|| {
                            handle.block_on(crate::wasm::llm::execute_llm_chat(
                                llm_config.provider,
                                &llm_config.api_key,
                                &llm_config.model,
                                request,
                            ))
                        })
                        .join()
                        .expect("Thread panicked")
                    }),
                    Err(_) => {
                        caller.data_mut().set_error("No tokio runtime available");
                        return -1;
                    }
                };

                let response = match result {
                    Ok(r) => r,
                    Err(e) => {
                        caller
                            .data_mut()
                            .set_error(format!("LLM chat failed: {}", e));
                        return -1;
                    }
                };

                // Serialize response to JSON
                let response_json = match serde_json::to_string(&response) {
                    Ok(j) => j,
                    Err(e) => {
                        caller
                            .data_mut()
                            .set_error(format!("Failed to serialize response: {}", e));
                        return -1;
                    }
                };

                // Write response to guest memory
                let response_bytes = response_json.as_bytes();
                let write_len = std::cmp::min(response_bytes.len(), response_len as usize);

                if memory
                    .write(
                        &mut caller,
                        response_ptr as usize,
                        &response_bytes[..write_len],
                    )
                    .is_err()
                {
                    caller
                        .data_mut()
                        .set_error("Failed to write response to memory");
                    return -1;
                }

                write_len as i32
            },
        )
        .map_err(|e| DaemonError::InternalError(format!("Failed to register llm_chat: {}", e)))?;

    // Register memory_create: creates a memory chunk and returns the ID
    // memory_create: content_ptr, content_len, metadata_ptr, metadata_len, result_ptr, result_len -> id_len or -1
    linker
        .func_wrap(
            "nevoflux",
            "memory_create",
            |mut caller: Caller<'_, HostState>,
             content_ptr: i32,
             content_len: i32,
             _metadata_ptr: i32,
             _metadata_len: i32,
             result_ptr: i32,
             result_len: i32|
             -> i32 {
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => return -1,
                };

                // Read content from guest memory
                let mut content_buf = vec![0u8; content_len as usize];
                if memory
                    .read(&caller, content_ptr as usize, &mut content_buf)
                    .is_err()
                {
                    return -1;
                }

                // Generate a simple ID
                let id = format!("mem-{}", uuid::Uuid::new_v4());
                let id_bytes = id.as_bytes();
                let write_len = std::cmp::min(id_bytes.len(), result_len as usize);

                if memory
                    .write(&mut caller, result_ptr as usize, &id_bytes[..write_len])
                    .is_err()
                {
                    return -1;
                }

                write_len as i32
            },
        )
        .map_err(|e| {
            DaemonError::InternalError(format!("Failed to register memory_create: {}", e))
        })?;

    // Register memory_delete: deletes a memory chunk
    // memory_delete: id_ptr, id_len -> 1 for success, -1 for error
    linker
        .func_wrap(
            "nevoflux",
            "memory_delete",
            |mut caller: Caller<'_, HostState>, id_ptr: i32, id_len: i32| -> i32 {
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => return -1,
                };

                let mut id_buf = vec![0u8; id_len as usize];
                if memory.read(&caller, id_ptr as usize, &mut id_buf).is_err() {
                    return -1;
                }

                // Return success (actual deletion would use services)
                1
            },
        )
        .map_err(|e| {
            DaemonError::InternalError(format!("Failed to register memory_delete: {}", e))
        })?;

    // Register memory_search: searches memory and returns results
    // memory_search: query_ptr, query_len, limit, results_ptr, results_len -> bytes written or -1
    linker
        .func_wrap(
            "nevoflux",
            "memory_search",
            |mut caller: Caller<'_, HostState>,
             query_ptr: i32,
             query_len: i32,
             _limit: i32,
             results_ptr: i32,
             results_len: i32|
             -> i32 {
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => return -1,
                };

                // Read query (for logging/validation)
                let mut query_buf = vec![0u8; query_len as usize];
                if memory
                    .read(&caller, query_ptr as usize, &mut query_buf)
                    .is_err()
                {
                    return -1;
                }

                // Return empty JSON array for now
                let result = b"[]";
                let write_len = std::cmp::min(result.len(), results_len as usize);

                if memory
                    .write(&mut caller, results_ptr as usize, &result[..write_len])
                    .is_err()
                {
                    return -1;
                }

                write_len as i32
            },
        )
        .map_err(|e| {
            DaemonError::InternalError(format!("Failed to register memory_search: {}", e))
        })?;

    // Register permission_check: placeholder that returns 1 (always allowed)
    linker
        .func_wrap(
            "nevoflux",
            "permission_check",
            |_caller: Caller<'_, HostState>, _action_ptr: i32, _action_len: i32| -> i32 {
                // Placeholder: always allowed
                1
            },
        )
        .map_err(|e| {
            DaemonError::InternalError(format!("Failed to register permission_check: {}", e))
        })?;

    // Register skill_list: returns JSON array of skill summaries
    // skill_list: result_ptr, result_len -> bytes written or -1
    linker
        .func_wrap(
            "nevoflux",
            "skill_list",
            |mut caller: Caller<'_, HostState>, result_ptr: i32, result_len: i32| -> i32 {
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => return -1,
                };

                // Return empty JSON array for now (actual implementation would use services.skills)
                let result = b"[]";
                let write_len = std::cmp::min(result.len(), result_len as usize);

                if memory
                    .write(&mut caller, result_ptr as usize, &result[..write_len])
                    .is_err()
                {
                    return -1;
                }

                write_len as i32
            },
        )
        .map_err(|e| DaemonError::InternalError(format!("Failed to register skill_list: {}", e)))?;

    // Register skill_load: loads a skill by name
    // skill_load: name_ptr, name_len, result_ptr, result_len -> bytes written or -1 (not found) or -2 (error)
    linker
        .func_wrap(
            "nevoflux",
            "skill_load",
            |mut caller: Caller<'_, HostState>,
             name_ptr: i32,
             name_len: i32,
             _result_ptr: i32,
             _result_len: i32|
             -> i32 {
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => return -2,
                };

                // Read skill name from guest memory
                let mut name_buf = vec![0u8; name_len as usize];
                if memory
                    .read(&caller, name_ptr as usize, &mut name_buf)
                    .is_err()
                {
                    return -2;
                }
                let _name = match String::from_utf8(name_buf) {
                    Ok(s) => s,
                    Err(_) => return -2,
                };

                // Return -1 for "not found" (actual implementation would load from registry)
                -1
            },
        )
        .map_err(|e| DaemonError::InternalError(format!("Failed to register skill_load: {}", e)))?;

    // Register tool_read: reads file contents
    // tool_read: path_ptr, path_len, offset, limit, result_ptr, result_len -> bytes written or -1
    linker
        .func_wrap(
            "nevoflux",
            "tool_read",
            |mut caller: Caller<'_, HostState>,
             path_ptr: i32,
             path_len: i32,
             offset: i64,
             limit: i64,
             result_ptr: i32,
             result_len: i32|
             -> i32 {
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => return -1,
                };

                // Read path from guest memory
                let mut path_buf = vec![0u8; path_len as usize];
                if memory
                    .read(&caller, path_ptr as usize, &mut path_buf)
                    .is_err()
                {
                    return -1;
                }
                let path = match String::from_utf8(path_buf) {
                    Ok(s) => s,
                    Err(_) => return -1,
                };

                // Read file (permission check would be needed in production)
                let content = match std::fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        caller
                            .data_mut()
                            .set_error(format!("Failed to read {}: {}", path, e));
                        return -1;
                    }
                };

                // Apply offset and limit
                let start = (offset as usize).min(content.len());
                let end = if limit > 0 {
                    (start + limit as usize).min(content.len())
                } else {
                    content.len()
                };

                let slice = &content.as_bytes()[start..end];
                let write_len = slice.len().min(result_len as usize);

                if memory
                    .write(&mut caller, result_ptr as usize, &slice[..write_len])
                    .is_err()
                {
                    return -1;
                }

                write_len as i32
            },
        )
        .map_err(|e| DaemonError::InternalError(format!("Failed to register tool_read: {}", e)))?;

    // Register tool_glob: glob pattern matching
    // tool_glob: pattern_ptr, pattern_len, base_ptr, base_len, result_ptr, result_len -> bytes written or -1
    linker
        .func_wrap(
            "nevoflux",
            "tool_glob",
            |mut caller: Caller<'_, HostState>,
             pattern_ptr: i32,
             pattern_len: i32,
             base_ptr: i32,
             base_len: i32,
             result_ptr: i32,
             result_len: i32|
             -> i32 {
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => return -1,
                };

                // Read pattern
                let mut pattern_buf = vec![0u8; pattern_len as usize];
                if memory
                    .read(&caller, pattern_ptr as usize, &mut pattern_buf)
                    .is_err()
                {
                    return -1;
                }
                let pattern = match String::from_utf8(pattern_buf) {
                    Ok(s) => s,
                    Err(_) => return -1,
                };

                // Read base path (optional)
                let base = if base_len > 0 {
                    let mut base_buf = vec![0u8; base_len as usize];
                    if memory
                        .read(&caller, base_ptr as usize, &mut base_buf)
                        .is_err()
                    {
                        ".".to_string()
                    } else {
                        String::from_utf8(base_buf).unwrap_or_else(|_| ".".to_string())
                    }
                } else {
                    ".".to_string()
                };

                // Execute glob
                let full_pattern = if base == "." {
                    pattern
                } else {
                    format!("{}/{}", base, pattern)
                };

                let paths: Vec<String> = glob::glob(&full_pattern)
                    .map(|entries| {
                        entries
                            .filter_map(|e| e.ok())
                            .map(|p| p.display().to_string())
                            .collect()
                    })
                    .unwrap_or_default();

                let result = serde_json::to_string(&paths).unwrap_or_else(|_| "[]".to_string());
                let result_bytes = result.as_bytes();
                let write_len = result_bytes.len().min(result_len as usize);

                if memory
                    .write(&mut caller, result_ptr as usize, &result_bytes[..write_len])
                    .is_err()
                {
                    return -1;
                }

                write_len as i32
            },
        )
        .map_err(|e| DaemonError::InternalError(format!("Failed to register tool_glob: {}", e)))?;

    Ok(linker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nevoflux_storage::Database;
    use std::sync::Arc;
    use wasmtime::{Module, Store};

    #[test]
    fn test_host_state_default() {
        let state = HostState::default();

        assert_eq!(state.memory_buffer.capacity(), MEMORY_BUFFER_CAPACITY);
        assert!(state.memory_buffer.is_empty());
        assert!(state.last_error.is_none());
    }

    #[test]
    fn test_host_state_error() {
        let mut state = HostState::new();

        // Initially no error
        assert!(state.take_error().is_none());

        // Set an error
        state.set_error("Test error message");
        assert!(state.last_error.is_some());

        // Take the error
        let error = state.take_error();
        assert_eq!(error, Some("Test error message".to_string()));

        // Error should be cleared
        assert!(state.take_error().is_none());
    }

    #[test]
    fn test_create_linker() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        // Verify all functions are registered by creating a module that imports them
        let wat = r#"
            (module
                (import "nevoflux" "get_error_len" (func $get_error_len (result i32)))
                (import "nevoflux" "get_error" (func $get_error (param i32 i32) (result i32)))
                (import "nevoflux" "llm_chat" (func $llm_chat (param i32 i32 i32 i32) (result i32)))
                (import "nevoflux" "memory_create" (func $memory_create (param i32 i32 i32 i32 i32 i32) (result i32)))
                (import "nevoflux" "memory_delete" (func $memory_delete (param i32 i32) (result i32)))
                (import "nevoflux" "memory_search" (func $memory_search (param i32 i32 i32 i32 i32) (result i32)))
                (import "nevoflux" "permission_check" (func $permission_check (param i32 i32) (result i32)))
                (import "nevoflux" "skill_list" (func $skill_list (param i32 i32) (result i32)))
                (import "nevoflux" "skill_load" (func $skill_load (param i32 i32 i32 i32) (result i32)))
                (import "nevoflux" "tool_read" (func $tool_read (param i32 i32 i64 i64 i32 i32) (result i32)))
                (import "nevoflux" "tool_glob" (func $tool_glob (param i32 i32 i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let mut store = Store::new(&engine, HostState::new());

        // Instantiate should succeed if all imports are satisfied
        let instance = linker.instantiate(&mut store, &module);
        assert!(
            instance.is_ok(),
            "Failed to instantiate module with linker: {:?}",
            instance.err()
        );
    }

    #[test]
    fn test_get_error_len_no_error() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "get_error_len" (func $get_error_len (result i32)))
                (memory (export "memory") 1)
                (func (export "test") (result i32)
                    call $get_error_len
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let mut store = Store::new(&engine, HostState::new());
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        assert_eq!(result, 0);
    }

    #[test]
    fn test_get_error_len_with_error() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "get_error_len" (func $get_error_len (result i32)))
                (memory (export "memory") 1)
                (func (export "test") (result i32)
                    call $get_error_len
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let mut state = HostState::new();
        state.set_error("Test error");
        let mut store = Store::new(&engine, state);
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        assert_eq!(result, 10); // "Test error".len()
    }

    #[test]
    fn test_llm_chat_no_services() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "llm_chat" (func $llm_chat (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "{\"messages\":[{\"role\":\"user\",\"content\":\"Hello\"}]}")
                (func (export "test") (result i32)
                    i32.const 0   ;; request_ptr
                    i32.const 52  ;; request_len
                    i32.const 100 ;; response_ptr
                    i32.const 256 ;; response_len
                    call $llm_chat
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let mut store = Store::new(&engine, HostState::new());
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        // Returns -1 because no services are configured
        assert_eq!(result, -1);
    }

    #[test]
    fn test_llm_chat_invalid_json() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "llm_chat" (func $llm_chat (param i32 i32 i32 i32) (result i32)))
                (import "nevoflux" "get_error_len" (func $get_error_len (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "not valid json")
                (func (export "test") (result i32)
                    i32.const 0   ;; request_ptr
                    i32.const 14  ;; request_len
                    i32.const 100 ;; response_ptr
                    i32.const 256 ;; response_len
                    call $llm_chat
                )
                (func (export "error_len") (result i32)
                    call $get_error_len
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services = crate::wasm::services::HostServices::new(db);
        let state = HostState::new().with_services(services);
        let mut store = Store::new(&engine, state);
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        // Returns -1 because JSON is invalid
        assert_eq!(result, -1);

        // Error should be set
        let error_len_func = instance
            .get_typed_func::<(), i32>(&mut store, "error_len")
            .expect("Failed to get error_len function");
        let error_len = error_len_func
            .call(&mut store, ())
            .expect("Failed to call error_len");
        assert!(error_len > 0);
    }

    #[test]
    fn test_llm_chat_no_llm_config() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "llm_chat" (func $llm_chat (param i32 i32 i32 i32) (result i32)))
                (import "nevoflux" "get_error_len" (func $get_error_len (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "{\"messages\":[{\"role\":\"user\",\"content\":\"Hi\"}]}")
                (func (export "test") (result i32)
                    i32.const 0   ;; request_ptr
                    i32.const 48  ;; request_len
                    i32.const 100 ;; response_ptr
                    i32.const 256 ;; response_len
                    call $llm_chat
                )
                (func (export "error_len") (result i32)
                    call $get_error_len
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        // Create services without LLM config
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services = crate::wasm::services::HostServices::new(db);
        let state = HostState::new().with_services(services);
        let mut store = Store::new(&engine, state);
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        // Returns -1 because LLM is not configured
        assert_eq!(result, -1);

        // Error should mention LLM not configured
        let error_len_func = instance
            .get_typed_func::<(), i32>(&mut store, "error_len")
            .expect("Failed to get error_len function");
        let error_len = error_len_func
            .call(&mut store, ())
            .expect("Failed to call error_len");
        assert!(error_len > 0);
    }

    #[test]
    fn test_memory_search_empty() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "memory_search" (func $memory_search (param i32 i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "test") (result i32)
                    i32.const 0   ;; query_ptr
                    i32.const 0   ;; query_len
                    i32.const 10  ;; limit
                    i32.const 100 ;; results_ptr
                    i32.const 256 ;; results_len
                    call $memory_search
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let mut store = Store::new(&engine, HostState::new());
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        assert_eq!(result, 2); // Returns "[]" which is 2 bytes
    }

    #[test]
    fn test_permission_check_allowed() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "permission_check" (func $permission_check (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "test") (result i32)
                    i32.const 0  ;; action_ptr
                    i32.const 0  ;; action_len
                    call $permission_check
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let mut store = Store::new(&engine, HostState::new());
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        assert_eq!(result, 1); // Always allowed
    }

    #[test]
    fn test_memory_create() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        // WAT module that calls memory_create and returns the length of the generated ID
        let wat = r#"
            (module
                (import "nevoflux" "memory_create" (func $memory_create (param i32 i32 i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "test content")
                (func (export "test") (result i32)
                    i32.const 0    ;; content_ptr
                    i32.const 12   ;; content_len ("test content")
                    i32.const 0    ;; metadata_ptr (unused)
                    i32.const 0    ;; metadata_len (unused)
                    i32.const 100  ;; result_ptr
                    i32.const 256  ;; result_len
                    call $memory_create
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let mut store = Store::new(&engine, HostState::new());
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        // ID format is "mem-{uuid}" which is 4 + 36 = 40 characters
        assert_eq!(result, 40);
    }

    #[test]
    fn test_memory_delete() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "memory_delete" (func $memory_delete (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "mem-12345678-1234-1234-1234-123456789012")
                (func (export "test") (result i32)
                    i32.const 0   ;; id_ptr
                    i32.const 40  ;; id_len
                    call $memory_delete
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let mut store = Store::new(&engine, HostState::new());
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        assert_eq!(result, 1); // Success
    }

    #[test]
    fn test_skill_list_empty() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "skill_list" (func $skill_list (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "test") (result i32)
                    i32.const 100 ;; result_ptr
                    i32.const 256 ;; result_len
                    call $skill_list
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let mut store = Store::new(&engine, HostState::new());
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        assert_eq!(result, 2); // Returns "[]" which is 2 bytes
    }

    #[test]
    fn test_skill_load_not_found() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "skill_load" (func $skill_load (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "nonexistent-skill")
                (func (export "test") (result i32)
                    i32.const 0    ;; name_ptr
                    i32.const 17   ;; name_len ("nonexistent-skill")
                    i32.const 100  ;; result_ptr
                    i32.const 256  ;; result_len
                    call $skill_load
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let mut store = Store::new(&engine, HostState::new());
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        assert_eq!(result, -1); // Not found
    }

    #[test]
    fn test_tool_read_success() {
        use std::io::Write;

        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        // Create a temporary file to read
        let mut temp_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
        temp_file
            .write_all(b"Hello, World!")
            .expect("Failed to write temp file");
        let temp_path = temp_file.path().to_string_lossy().to_string();
        let path_len = temp_path.len();

        // Create WAT with the path embedded
        let wat = format!(
            r#"
            (module
                (import "nevoflux" "tool_read" (func $tool_read (param i32 i32 i64 i64 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "{}")
                (func (export "test") (result i32)
                    i32.const 0    ;; path_ptr
                    i32.const {}   ;; path_len
                    i64.const 0    ;; offset
                    i64.const 0    ;; limit (0 = no limit)
                    i32.const 200  ;; result_ptr
                    i32.const 256  ;; result_len
                    call $tool_read
                )
            )
        "#,
            temp_path, path_len
        );

        let module = Module::new(&engine, &wat).expect("Failed to compile test module");
        let mut store = Store::new(&engine, HostState::new());
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        assert_eq!(result, 13); // "Hello, World!".len()
    }

    #[test]
    fn test_tool_read_with_offset_and_limit() {
        use std::io::Write;

        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        // Create a temporary file to read
        let mut temp_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
        temp_file
            .write_all(b"Hello, World!")
            .expect("Failed to write temp file");
        let temp_path = temp_file.path().to_string_lossy().to_string();
        let path_len = temp_path.len();

        // Create WAT with offset=7 and limit=5 to read "World"
        let wat = format!(
            r#"
            (module
                (import "nevoflux" "tool_read" (func $tool_read (param i32 i32 i64 i64 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "{}")
                (func (export "test") (result i32)
                    i32.const 0    ;; path_ptr
                    i32.const {}   ;; path_len
                    i64.const 7    ;; offset (skip "Hello, ")
                    i64.const 5    ;; limit (read "World")
                    i32.const 200  ;; result_ptr
                    i32.const 256  ;; result_len
                    call $tool_read
                )
            )
        "#,
            temp_path, path_len
        );

        let module = Module::new(&engine, &wat).expect("Failed to compile test module");
        let mut store = Store::new(&engine, HostState::new());
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        assert_eq!(result, 5); // "World".len()
    }

    #[test]
    fn test_tool_read_file_not_found() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "tool_read" (func $tool_read (param i32 i32 i64 i64 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "/nonexistent/path/to/file.txt")
                (func (export "test") (result i32)
                    i32.const 0    ;; path_ptr
                    i32.const 29   ;; path_len
                    i64.const 0    ;; offset
                    i64.const 0    ;; limit
                    i32.const 200  ;; result_ptr
                    i32.const 256  ;; result_len
                    call $tool_read
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let mut store = Store::new(&engine, HostState::new());
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        assert_eq!(result, -1); // File not found
    }

    #[test]
    fn test_tool_glob_empty_pattern() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        // Use a pattern that won't match anything
        let wat = r#"
            (module
                (import "nevoflux" "tool_glob" (func $tool_glob (param i32 i32 i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "/nonexistent/*.xyz")
                (func (export "test") (result i32)
                    i32.const 0    ;; pattern_ptr
                    i32.const 18   ;; pattern_len
                    i32.const 0    ;; base_ptr (unused when base_len=0)
                    i32.const 0    ;; base_len
                    i32.const 200  ;; result_ptr
                    i32.const 256  ;; result_len
                    call $tool_glob
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let mut store = Store::new(&engine, HostState::new());
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        assert_eq!(result, 2); // Returns "[]" which is 2 bytes (no matches)
    }

    #[test]
    fn test_tool_glob_with_matches() {
        use std::io::Write;

        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        // Create a temporary directory with some files
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file1 = temp_dir.path().join("test1.txt");
        let file2 = temp_dir.path().join("test2.txt");

        std::fs::File::create(&file1)
            .expect("Failed to create file1")
            .write_all(b"content1")
            .expect("Failed to write file1");
        std::fs::File::create(&file2)
            .expect("Failed to create file2")
            .write_all(b"content2")
            .expect("Failed to write file2");

        let pattern = format!("{}/*.txt", temp_dir.path().display());
        let pattern_len = pattern.len();

        let wat = format!(
            r#"
            (module
                (import "nevoflux" "tool_glob" (func $tool_glob (param i32 i32 i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "{}")
                (func (export "test") (result i32)
                    i32.const 0    ;; pattern_ptr
                    i32.const {}   ;; pattern_len
                    i32.const 0    ;; base_ptr
                    i32.const 0    ;; base_len
                    i32.const 200  ;; result_ptr
                    i32.const 1024 ;; result_len
                    call $tool_glob
                )
            )
        "#,
            pattern, pattern_len
        );

        let module = Module::new(&engine, &wat).expect("Failed to compile test module");
        let mut store = Store::new(&engine, HostState::new());
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        // Should return more than 2 bytes (empty array) since we have matches
        assert!(
            result > 2,
            "Expected glob to find matches, got {} bytes",
            result
        );
    }

    #[test]
    fn test_tool_glob_with_base_path() {
        use std::io::Write;

        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        // Create a temporary directory with some files
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file1 = temp_dir.path().join("file.rs");
        std::fs::File::create(&file1)
            .expect("Failed to create file")
            .write_all(b"fn main() {}")
            .expect("Failed to write file");

        let base_path = temp_dir.path().display().to_string();
        let base_len = base_path.len();

        // WAT module with pattern "*.rs" and base path
        let wat = format!(
            r#"
            (module
                (import "nevoflux" "tool_glob" (func $tool_glob (param i32 i32 i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "*.rs")
                (data (i32.const 100) "{}")
                (func (export "test") (result i32)
                    i32.const 0    ;; pattern_ptr ("*.rs")
                    i32.const 4    ;; pattern_len
                    i32.const 100  ;; base_ptr
                    i32.const {}   ;; base_len
                    i32.const 300  ;; result_ptr
                    i32.const 1024 ;; result_len
                    call $tool_glob
                )
            )
        "#,
            base_path, base_len
        );

        let module = Module::new(&engine, &wat).expect("Failed to compile test module");
        let mut store = Store::new(&engine, HostState::new());
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        // Should return more than 2 bytes (empty array) since we have a match
        assert!(
            result > 2,
            "Expected glob to find file.rs, got {} bytes",
            result
        );
    }
}
