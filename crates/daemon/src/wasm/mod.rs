//! Wasm module for WebAssembly runtime support.
//!
//! This module provides the infrastructure for loading and executing
//! WebAssembly modules within the NevoFlux daemon.

pub mod instance;
pub mod linker;
pub mod runtime;

pub use instance::WasmInstance;
pub use linker::{create_linker, HostState};
pub use runtime::{WasmConfig, WasmRuntime};
