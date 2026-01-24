//! Wasm runtime module for executing WebAssembly modules.
//!
//! This module provides the core runtime for loading and executing
//! WebAssembly modules using Wasmtime.

use std::path::Path;

use wasmtime::{Engine, Module};

use crate::error::{DaemonError, Result};

/// Configuration for the Wasm runtime.
#[derive(Debug, Clone)]
pub struct WasmConfig {
    /// Maximum memory pages for Wasm modules (64KB per page).
    /// Default: 1024 pages (64MB).
    pub max_memory_pages: u32,

    /// Enable WASI Preview 2 support.
    /// Default: false.
    pub wasi_preview2: bool,
}

impl Default for WasmConfig {
    fn default() -> Self {
        Self {
            max_memory_pages: 1024,
            wasi_preview2: false,
        }
    }
}

impl WasmConfig {
    /// Create a new WasmConfig with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the maximum memory pages.
    pub fn with_max_memory_pages(mut self, pages: u32) -> Self {
        self.max_memory_pages = pages;
        self
    }

    /// Enable or disable WASI Preview 2.
    pub fn with_wasi_preview2(mut self, enabled: bool) -> Self {
        self.wasi_preview2 = enabled;
        self
    }
}

/// Wasm runtime for loading and executing WebAssembly modules.
///
/// The runtime wraps a Wasmtime engine and module, providing a high-level
/// interface for Wasm execution.
pub struct WasmRuntime {
    /// The Wasmtime engine.
    engine: Engine,

    /// The compiled Wasm module.
    module: Module,

    /// Runtime configuration.
    config: WasmConfig,
}

impl std::fmt::Debug for WasmRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmRuntime")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl WasmRuntime {
    /// Load a Wasm module from a file path using default configuration.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the .wasm file
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the module is invalid.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::from_file_with_config(path, WasmConfig::default())
    }

    /// Load a Wasm module from a file path with custom configuration.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the .wasm file
    /// * `config` - Runtime configuration
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the module is invalid.
    pub fn from_file_with_config<P: AsRef<Path>>(path: P, config: WasmConfig) -> Result<Self> {
        let engine = Engine::default();
        let module = Module::from_file(&engine, path).map_err(|e| {
            DaemonError::InternalError(format!("Failed to load Wasm module from file: {}", e))
        })?;

        Ok(Self {
            engine,
            module,
            config,
        })
    }

    /// Load a Wasm module from raw bytes using default configuration.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Raw Wasm binary data
    ///
    /// # Errors
    ///
    /// Returns an error if the bytes are not valid Wasm.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Self::from_bytes_with_config(bytes, WasmConfig::default())
    }

    /// Load a Wasm module from raw bytes with custom configuration.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Raw Wasm binary data
    /// * `config` - Runtime configuration
    ///
    /// # Errors
    ///
    /// Returns an error if the bytes are not valid Wasm.
    pub fn from_bytes_with_config(bytes: &[u8], config: WasmConfig) -> Result<Self> {
        let engine = Engine::default();
        let module = Module::new(&engine, bytes).map_err(|e| {
            DaemonError::InternalError(format!("Failed to load Wasm module from bytes: {}", e))
        })?;

        Ok(Self {
            engine,
            module,
            config,
        })
    }

    /// Get a reference to the Wasmtime engine.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Get a reference to the compiled module.
    pub fn module(&self) -> &Module {
        &self.module
    }

    /// Get a reference to the runtime configuration.
    pub fn config(&self) -> &WasmConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_wasm() -> Vec<u8> {
        wat::parse_str("(module)").expect("Failed to parse WAT")
    }

    #[test]
    fn test_wasm_config_defaults() {
        let config = WasmConfig::default();

        assert_eq!(config.max_memory_pages, 1024);
        assert!(!config.wasi_preview2);
    }

    #[test]
    fn test_wasm_config_builder() {
        let config = WasmConfig::new()
            .with_max_memory_pages(2048)
            .with_wasi_preview2(true);

        assert_eq!(config.max_memory_pages, 2048);
        assert!(config.wasi_preview2);
    }

    #[test]
    fn test_wasm_runtime_from_bytes() {
        let wasm_bytes = minimal_wasm();
        let runtime = WasmRuntime::from_bytes(&wasm_bytes).expect("Failed to create runtime");

        // Verify engine and module are accessible
        let _engine = runtime.engine();
        assert!(runtime.module().name().is_none());
    }

    #[test]
    fn test_wasm_runtime_from_bytes_with_config() {
        let wasm_bytes = minimal_wasm();
        let config = WasmConfig::new().with_max_memory_pages(512);

        let runtime = WasmRuntime::from_bytes_with_config(&wasm_bytes, config)
            .expect("Failed to create runtime");

        assert_eq!(runtime.config().max_memory_pages, 512);
    }

    #[test]
    fn test_wasm_runtime_invalid_bytes() {
        let invalid_bytes = b"not valid wasm";
        let result = WasmRuntime::from_bytes(invalid_bytes);

        assert!(result.is_err());
    }

    #[test]
    fn test_wasm_runtime_accessors() {
        let wasm_bytes = minimal_wasm();
        let runtime = WasmRuntime::from_bytes(&wasm_bytes).expect("Failed to create runtime");

        // Verify we can access engine and module
        let _engine = runtime.engine();
        let _module = runtime.module();
        let config = runtime.config();

        assert_eq!(config.max_memory_pages, 1024);
    }
}
