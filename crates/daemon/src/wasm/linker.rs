//! Host function linker for Wasm guest modules.
//!
//! This module provides the host functions that Wasm guest modules can call
//! to interact with the NevoFlux daemon.

use wasmtime::{Caller, Engine, Linker};

use crate::error::{DaemonError, Result};

/// Initial capacity for the memory buffer (1MB).
const MEMORY_BUFFER_CAPACITY: usize = 1024 * 1024;

/// Host state for Wasm guest modules.
///
/// This struct holds the state that is accessible to host functions
/// when called from Wasm guest modules.
#[derive(Debug)]
pub struct HostState {
    /// Memory buffer for passing data between host and guest.
    pub memory_buffer: Vec<u8>,

    /// Last error message from host functions.
    last_error: Option<String>,
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
        }
    }

    /// Set the last error message.
    pub fn set_error(&mut self, error: impl Into<String>) {
        self.last_error = Some(error.into());
    }

    /// Take the last error message, clearing it.
    pub fn take_error(&mut self) -> Option<String> {
        self.last_error.take()
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

    // Register llm_chat: placeholder that returns -1 (not implemented)
    linker
        .func_wrap(
            "nevoflux",
            "llm_chat",
            |_caller: Caller<'_, HostState>,
             _prompt_ptr: i32,
             _prompt_len: i32,
             _response_ptr: i32,
             _response_len: i32|
             -> i32 {
                // Placeholder: not implemented yet
                -1
            },
        )
        .map_err(|e| DaemonError::InternalError(format!("Failed to register llm_chat: {}", e)))?;

    // Register memory_search: placeholder that returns 0 (empty results)
    linker
        .func_wrap(
            "nevoflux",
            "memory_search",
            |_caller: Caller<'_, HostState>,
             _query_ptr: i32,
             _query_len: i32,
             _results_ptr: i32,
             _results_len: i32|
             -> i32 {
                // Placeholder: returns 0 (empty results)
                0
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

    Ok(linker)
}

#[cfg(test)]
mod tests {
    use super::*;
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
                (import "nevoflux" "memory_search" (func $memory_search (param i32 i32 i32 i32) (result i32)))
                (import "nevoflux" "permission_check" (func $permission_check (param i32 i32) (result i32)))
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
    fn test_llm_chat_not_implemented() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "llm_chat" (func $llm_chat (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "test") (result i32)
                    i32.const 0  ;; prompt_ptr
                    i32.const 0  ;; prompt_len
                    i32.const 0  ;; response_ptr
                    i32.const 0  ;; response_len
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
        assert_eq!(result, -1); // Not implemented
    }

    #[test]
    fn test_memory_search_empty() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "memory_search" (func $memory_search (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "test") (result i32)
                    i32.const 0  ;; query_ptr
                    i32.const 0  ;; query_len
                    i32.const 0  ;; results_ptr
                    i32.const 0  ;; results_len
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
        assert_eq!(result, 0); // Empty results
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
}
