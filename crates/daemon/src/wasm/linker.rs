//! Host function linker for Wasm guest modules.
//!
//! This module provides the host functions that Wasm guest modules can call
//! to interact with the NevoFlux daemon.

use wasmtime::{Caller, Engine, Linker};

use crate::error::{DaemonError, Result};
use crate::wasm::services::HostServices;
use nevoflux_storage::{CheckPermissionParams, MemoryChunk, MemoryRepository, PermissionRepository};

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

                let content = match String::from_utf8(content_buf) {
                    Ok(s) => s,
                    Err(_) => return -1,
                };

                // Generate UUID for ID
                let id = format!("mem-{}", uuid::Uuid::new_v4());

                // Store in database if services available
                if let Some(services) = &caller.data().services {
                    let chunk = MemoryChunk::new(&content).with_id(&id);
                    let repo = MemoryRepository::new(&services.database);
                    if repo.create(&chunk).is_err() {
                        // Still return the ID even if storage fails
                        tracing::warn!("Failed to store memory chunk in database");
                    }
                }

                // Write ID to guest memory
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
    // memory_delete: id_ptr, id_len -> 1 for success, 0 for failure
    linker
        .func_wrap(
            "nevoflux",
            "memory_delete",
            |mut caller: Caller<'_, HostState>, id_ptr: i32, id_len: i32| -> i32 {
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => return 0,
                };

                // Read ID from guest memory
                let mut id_buf = vec![0u8; id_len as usize];
                if memory.read(&caller, id_ptr as usize, &mut id_buf).is_err() {
                    return 0;
                }

                let id = match String::from_utf8(id_buf) {
                    Ok(s) => s,
                    Err(_) => return 0,
                };

                // Delete from database if services available
                match &caller.data().services {
                    Some(services) => {
                        let repo = MemoryRepository::new(&services.database);
                        match repo.delete(&id) {
                            Ok(true) => 1,  // Successfully deleted
                            Ok(false) => 0, // Not found
                            Err(_) => 0,    // Error
                        }
                    }
                    None => 1, // No services, return success (no-op)
                }
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
             limit: i32,
             results_ptr: i32,
             results_len: i32|
             -> i32 {
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => return -1,
                };

                // Read query from guest memory
                let mut query_buf = vec![0u8; query_len as usize];
                if memory
                    .read(&caller, query_ptr as usize, &mut query_buf)
                    .is_err()
                {
                    return -1;
                }

                let query = match String::from_utf8(query_buf) {
                    Ok(s) => s,
                    Err(_) => return -1,
                };

                // Get database from services and search
                let result_json = match &caller.data().services {
                    Some(services) => {
                        let repo = MemoryRepository::new(&services.database);
                        let limit = if limit <= 0 { 10 } else { limit as usize };

                        match repo.search_fts(&query, limit) {
                            Ok(chunks) => {
                                // Serialize results to JSON with id, content, metadata fields
                                let results: Vec<serde_json::Value> = chunks
                                    .into_iter()
                                    .map(|chunk| {
                                        serde_json::json!({
                                            "id": chunk.id,
                                            "content": chunk.content,
                                            "metadata": chunk.metadata
                                        })
                                    })
                                    .collect();
                                serde_json::to_string(&results).unwrap_or_else(|_| "[]".to_string())
                            }
                            Err(_) => "[]".to_string(),
                        }
                    }
                    None => "[]".to_string(),
                };

                let result_bytes = result_json.as_bytes();
                let write_len = std::cmp::min(result_bytes.len(), results_len as usize);

                if memory
                    .write(
                        &mut caller,
                        results_ptr as usize,
                        &result_bytes[..write_len],
                    )
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

    // Register permission_check: checks permission against database
    // permission_check: resource_ptr, resource_len, action_ptr, action_len -> 1 (allowed), 0 (denied), -1 (error)
    linker
        .func_wrap(
            "nevoflux",
            "permission_check",
            |mut caller: Caller<'_, HostState>,
             resource_ptr: i32,
             resource_len: i32,
             action_ptr: i32,
             action_len: i32|
             -> i32 {
                // If no services available, allow in development mode
                let services = match &caller.data().services {
                    Some(s) => s.clone(),
                    None => return 1, // Allow in development mode
                };

                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => {
                        tracing::error!("Guest module has no memory export");
                        return -1;
                    }
                };

                // Read resource string from guest memory
                let mut resource_buf = vec![0u8; resource_len as usize];
                if memory
                    .read(&caller, resource_ptr as usize, &mut resource_buf)
                    .is_err()
                {
                    caller
                        .data_mut()
                        .set_error("Failed to read resource from memory");
                    return -1;
                }

                let resource = match String::from_utf8(resource_buf) {
                    Ok(s) => s,
                    Err(_) => {
                        caller.data_mut().set_error("Invalid UTF-8 in resource");
                        return -1;
                    }
                };

                // Read action string from guest memory
                let mut action_buf = vec![0u8; action_len as usize];
                if memory
                    .read(&caller, action_ptr as usize, &mut action_buf)
                    .is_err()
                {
                    caller
                        .data_mut()
                        .set_error("Failed to read action from memory");
                    return -1;
                }

                let action = match String::from_utf8(action_buf) {
                    Ok(s) => s,
                    Err(_) => {
                        caller.data_mut().set_error("Invalid UTF-8 in action");
                        return -1;
                    }
                };

                // Check permission using database
                // Use "default" as session_id for now
                let repo = PermissionRepository::new(&services.database);
                let params = CheckPermissionParams::new("resource", &action, &resource)
                    .with_session_id("default");

                match repo.check(params) {
                    Ok(Some(true)) => 1,  // Permission granted
                    Ok(Some(false)) => 0, // Permission denied
                    Ok(None) => 1,        // No permission found, allow by default
                    Err(e) => {
                        caller
                            .data_mut()
                            .set_error(format!("Permission check failed: {}", e));
                        -1
                    }
                }
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

                // Get skill summaries from registry
                let result_json = match &caller.data().services {
                    Some(services) => {
                        let skills = services.skills.clone();

                        // Use tokio runtime to access the async RwLock
                        match tokio::runtime::Handle::try_current() {
                            Ok(handle) => std::thread::scope(|s| {
                                s.spawn(|| {
                                    handle.block_on(async {
                                        let registry = skills.read().await;
                                        let summaries = registry.list();

                                        // Serialize to JSON array with name, description, tags
                                        let json_summaries: Vec<serde_json::Value> = summaries
                                            .into_iter()
                                            .map(|s| {
                                                serde_json::json!({
                                                    "name": s.name,
                                                    "description": s.description,
                                                    "tags": s.tags
                                                })
                                            })
                                            .collect();

                                        serde_json::to_string(&json_summaries)
                                            .unwrap_or_else(|_| "[]".to_string())
                                    })
                                })
                                .join()
                                .expect("Thread panicked")
                            }),
                            Err(_) => "[]".to_string(),
                        }
                    }
                    None => "[]".to_string(),
                };

                let result_bytes = result_json.as_bytes();
                let write_len = std::cmp::min(result_bytes.len(), result_len as usize);

                if memory
                    .write(&mut caller, result_ptr as usize, &result_bytes[..write_len])
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
             result_ptr: i32,
             result_len: i32|
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
                let name = match String::from_utf8(name_buf) {
                    Ok(s) => s,
                    Err(_) => return -2,
                };

                // Get skill from registry
                let skill_json = match &caller.data().services {
                    Some(services) => {
                        let skills = services.skills.clone();

                        // Use tokio runtime to access the async RwLock
                        match tokio::runtime::Handle::try_current() {
                            Ok(handle) => std::thread::scope(|s| {
                                s.spawn(|| {
                                    handle.block_on(async {
                                        let registry = skills.read().await;
                                        match registry.get(&name) {
                                            Some(skill) => {
                                                // Serialize skill to JSON with name, description, content, tags
                                                let json = serde_json::json!({
                                                    "name": skill.name(),
                                                    "description": skill.description(),
                                                    "content": skill.content,
                                                    "tags": skill.metadata.tags
                                                });
                                                Some(
                                                    serde_json::to_string(&json)
                                                        .unwrap_or_else(|_| "{}".to_string()),
                                                )
                                            }
                                            None => None,
                                        }
                                    })
                                })
                                .join()
                                .expect("Thread panicked")
                            }),
                            Err(_) => None,
                        }
                    }
                    None => None,
                };

                // Return -1 if skill not found
                let skill_json = match skill_json {
                    Some(json) => json,
                    None => return -1,
                };

                let result_bytes = skill_json.as_bytes();
                let write_len = std::cmp::min(result_bytes.len(), result_len as usize);

                if memory
                    .write(&mut caller, result_ptr as usize, &result_bytes[..write_len])
                    .is_err()
                {
                    return -2;
                }

                write_len as i32
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
    use nevoflux_skills::{Skill, SkillMetadata, SkillRegistry};
    use nevoflux_storage::Database;
    use std::sync::Arc;
    use tokio::sync::RwLock;
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
                (import "nevoflux" "permission_check" (func $permission_check (param i32 i32 i32 i32) (result i32)))
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
    fn test_permission_check_no_services() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        // Test with no services - should return 1 (allowed in development mode)
        let wat = r#"
            (module
                (import "nevoflux" "permission_check" (func $permission_check (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "/home/user/file.txt")
                (data (i32.const 50) "read")
                (func (export "test") (result i32)
                    i32.const 0   ;; resource_ptr
                    i32.const 19  ;; resource_len ("/home/user/file.txt")
                    i32.const 50  ;; action_ptr
                    i32.const 4   ;; action_len ("read")
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
        assert_eq!(result, 1); // Allowed in development mode (no services)
    }

    #[test]
    fn test_permission_check_with_database_no_permission() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        // Test with database but no permission set - should return 1 (allowed by default)
        let wat = r#"
            (module
                (import "nevoflux" "permission_check" (func $permission_check (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "/home/user/file.txt")
                (data (i32.const 50) "read")
                (func (export "test") (result i32)
                    i32.const 0   ;; resource_ptr
                    i32.const 19  ;; resource_len
                    i32.const 50  ;; action_ptr
                    i32.const 4   ;; action_len
                    call $permission_check
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let db = Arc::new(Database::open_in_memory().expect("Failed to open database"));
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
        assert_eq!(result, 1); // Allowed by default when no permission is set
    }

    #[test]
    fn test_permission_check_granted() {
        use nevoflux_storage::{CreatePermissionParams, PermissionScope};

        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "permission_check" (func $permission_check (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "/allowed/path")
                (data (i32.const 50) "read")
                (func (export "test") (result i32)
                    i32.const 0   ;; resource_ptr
                    i32.const 13  ;; resource_len ("/allowed/path")
                    i32.const 50  ;; action_ptr
                    i32.const 4   ;; action_len ("read")
                    call $permission_check
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let db = Arc::new(Database::open_in_memory().expect("Failed to open database"));

        // Create a permission that grants access
        let repo = PermissionRepository::new(&db);
        repo.create(
            CreatePermissionParams::new("resource", "read", "/allowed/path")
                .with_scope(PermissionScope::Session)
                .with_session_id("default")
                .with_granted(true),
        )
        .expect("Failed to create permission");

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
        assert_eq!(result, 1); // Permission granted
    }

    #[test]
    fn test_permission_check_denied() {
        use nevoflux_storage::{CreatePermissionParams, PermissionScope};

        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "permission_check" (func $permission_check (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "/denied/path")
                (data (i32.const 50) "write")
                (func (export "test") (result i32)
                    i32.const 0   ;; resource_ptr
                    i32.const 12  ;; resource_len ("/denied/path")
                    i32.const 50  ;; action_ptr
                    i32.const 5   ;; action_len ("write")
                    call $permission_check
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let db = Arc::new(Database::open_in_memory().expect("Failed to open database"));

        // Create a permission that denies access
        let repo = PermissionRepository::new(&db);
        repo.create(
            CreatePermissionParams::new("resource", "write", "/denied/path")
                .with_scope(PermissionScope::Session)
                .with_session_id("default")
                .with_granted(false),
        )
        .expect("Failed to create permission");

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
        assert_eq!(result, 0); // Permission denied
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

    #[test]
    fn test_memory_create_with_database() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "memory_create" (func $memory_create (param i32 i32 i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "test content for database")
                (func (export "test") (result i32)
                    i32.const 0    ;; content_ptr
                    i32.const 25   ;; content_len
                    i32.const 0    ;; metadata_ptr (unused)
                    i32.const 0    ;; metadata_len (unused)
                    i32.const 100  ;; result_ptr
                    i32.const 256  ;; result_len
                    call $memory_create
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services = crate::wasm::services::HostServices::new(db.clone());
        let state = HostState::new().with_services(services);
        let mut store = Store::new(&engine, state);
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        assert_eq!(result, 40); // ID format is "mem-{uuid}" which is 40 chars

        // Read the ID from guest memory
        let memory = instance
            .get_memory(&mut store, "memory")
            .expect("Failed to get memory");
        let mut id_buf = [0u8; 40];
        memory
            .read(&store, 100, &mut id_buf)
            .expect("Failed to read ID");
        let id = String::from_utf8(id_buf.to_vec()).expect("Invalid UTF-8");

        // Verify the chunk was stored in the database
        let repo = MemoryRepository::new(&db);
        let chunk = repo.get(&id).expect("Failed to get chunk from database");
        assert!(chunk.is_some());
        let chunk = chunk.unwrap();
        assert_eq!(chunk.content, "test content for database");
    }

    #[test]
    fn test_memory_delete_with_database() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        // First create a memory chunk in the database
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let chunk = MemoryChunk::new("content to delete").with_id("test-delete-id");
        let repo = MemoryRepository::new(&db);
        repo.create(&chunk).expect("Failed to create chunk");

        // Verify chunk exists
        assert!(repo.get("test-delete-id").unwrap().is_some());

        let wat = r#"
            (module
                (import "nevoflux" "memory_delete" (func $memory_delete (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "test-delete-id")
                (func (export "test") (result i32)
                    i32.const 0   ;; id_ptr
                    i32.const 14  ;; id_len
                    call $memory_delete
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let services = crate::wasm::services::HostServices::new(db.clone());
        let state = HostState::new().with_services(services);
        let mut store = Store::new(&engine, state);
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        assert_eq!(result, 1); // Success

        // Verify chunk was deleted from the database
        assert!(repo.get("test-delete-id").unwrap().is_none());
    }

    #[test]
    fn test_memory_search_with_database() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        // Create some memory chunks in the database
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let repo = MemoryRepository::new(&db);

        let chunk1 = MemoryChunk::new("The quick brown fox jumps").with_id("search-1");
        let chunk2 = MemoryChunk::new("A lazy dog sleeps").with_id("search-2");
        let chunk3 = MemoryChunk::new("The brown bear runs").with_id("search-3");

        repo.create(&chunk1).expect("Failed to create chunk1");
        repo.create(&chunk2).expect("Failed to create chunk2");
        repo.create(&chunk3).expect("Failed to create chunk3");

        let wat = r#"
            (module
                (import "nevoflux" "memory_search" (func $memory_search (param i32 i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "brown")
                (func (export "test") (result i32)
                    i32.const 0    ;; query_ptr
                    i32.const 5    ;; query_len ("brown")
                    i32.const 10   ;; limit
                    i32.const 100  ;; results_ptr
                    i32.const 1024 ;; results_len
                    call $memory_search
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
        let services = crate::wasm::services::HostServices::new(db.clone());
        let state = HostState::new().with_services(services);
        let mut store = Store::new(&engine, state);
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        assert!(result > 2, "Expected search results, got {} bytes", result);

        // Read and parse the results from guest memory
        let memory = instance
            .get_memory(&mut store, "memory")
            .expect("Failed to get memory");
        let mut result_buf = vec![0u8; result as usize];
        memory
            .read(&store, 100, &mut result_buf)
            .expect("Failed to read results");
        let result_json = String::from_utf8(result_buf).expect("Invalid UTF-8");

        let results: Vec<serde_json::Value> =
            serde_json::from_str(&result_json).expect("Failed to parse JSON");

        // Should find 2 chunks with "brown"
        assert_eq!(results.len(), 2);

        // Verify the result structure has id, content, and metadata
        for result in &results {
            assert!(result.get("id").is_some());
            assert!(result.get("content").is_some());
            assert!(result.get("metadata").is_some());
        }
    }

    #[test]
    fn test_memory_delete_nonexistent() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));

        let wat = r#"
            (module
                (import "nevoflux" "memory_delete" (func $memory_delete (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "nonexistent-id")
                (func (export "test") (result i32)
                    i32.const 0   ;; id_ptr
                    i32.const 14  ;; id_len
                    call $memory_delete
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");
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
        assert_eq!(result, 0); // Not found
    }

    #[tokio::test]
    async fn test_skill_list_with_registry() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "skill_list" (func $skill_list (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "test") (result i32)
                    i32.const 100  ;; result_ptr
                    i32.const 1024 ;; result_len
                    call $skill_list
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");

        // Create a registry with some skills
        let mut registry = SkillRegistry::new();
        let skill1 = Skill::new(
            SkillMetadata::new("code-review")
                .with_description("Review code for best practices")
                .with_tag("code")
                .with_tag("review"),
            "# Code Review\n\nReview guidelines...",
        );
        let skill2 = Skill::new(
            SkillMetadata::new("testing")
                .with_description("Write and run tests")
                .with_tag("testing"),
            "# Testing\n\nTesting best practices...",
        );
        registry
            .register(skill1)
            .expect("Failed to register skill1");
        registry
            .register(skill2)
            .expect("Failed to register skill2");

        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services =
            crate::wasm::services::HostServices::with_skills(db, Arc::new(RwLock::new(registry)));
        let state = HostState::new().with_services(services);
        let mut store = Store::new(&engine, state);
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        assert!(result > 2, "Expected skill list JSON, got {} bytes", result);

        // Read and parse the results from guest memory
        let memory = instance
            .get_memory(&mut store, "memory")
            .expect("Failed to get memory");
        let mut result_buf = vec![0u8; result as usize];
        memory
            .read(&store, 100, &mut result_buf)
            .expect("Failed to read results");
        let result_json = String::from_utf8(result_buf).expect("Invalid UTF-8");

        let summaries: Vec<serde_json::Value> =
            serde_json::from_str(&result_json).expect("Failed to parse JSON");

        // Should have 2 skills
        assert_eq!(summaries.len(), 2);

        // Verify structure has name, description, tags
        for summary in &summaries {
            assert!(summary.get("name").is_some());
            assert!(summary.get("description").is_some());
            assert!(summary.get("tags").is_some());
        }

        // Verify specific skills are present
        let names: Vec<&str> = summaries
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"code-review"));
        assert!(names.contains(&"testing"));
    }

    #[tokio::test]
    async fn test_skill_load_with_registry() {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");

        let wat = r#"
            (module
                (import "nevoflux" "skill_load" (func $skill_load (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "code-review")
                (func (export "test") (result i32)
                    i32.const 0    ;; name_ptr
                    i32.const 11   ;; name_len ("code-review")
                    i32.const 100  ;; result_ptr
                    i32.const 1024 ;; result_len
                    call $skill_load
                )
            )
        "#;

        let module = Module::new(&engine, wat).expect("Failed to compile test module");

        // Create a registry with a skill
        let mut registry = SkillRegistry::new();
        let skill = Skill::new(
            SkillMetadata::new("code-review")
                .with_description("Review code for best practices")
                .with_tag("code")
                .with_tag("review"),
            "# Code Review\n\nWhen reviewing code, check for:\n1. Logic errors\n2. Style issues",
        );
        registry.register(skill).expect("Failed to register skill");

        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services =
            crate::wasm::services::HostServices::with_skills(db, Arc::new(RwLock::new(registry)));
        let state = HostState::new().with_services(services);
        let mut store = Store::new(&engine, state);
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        assert!(result > 0, "Expected skill JSON, got {} bytes", result);

        // Read and parse the result from guest memory
        let memory = instance
            .get_memory(&mut store, "memory")
            .expect("Failed to get memory");
        let mut result_buf = vec![0u8; result as usize];
        memory
            .read(&store, 100, &mut result_buf)
            .expect("Failed to read result");
        let result_json = String::from_utf8(result_buf).expect("Invalid UTF-8");

        let skill: serde_json::Value =
            serde_json::from_str(&result_json).expect("Failed to parse JSON");

        // Verify structure has name, description, content, tags
        assert_eq!(skill["name"].as_str().unwrap(), "code-review");
        assert_eq!(
            skill["description"].as_str().unwrap(),
            "Review code for best practices"
        );
        assert!(skill["content"]
            .as_str()
            .unwrap()
            .contains("When reviewing code"));
        let tags: Vec<&str> = skill["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap())
            .collect();
        assert!(tags.contains(&"code"));
        assert!(tags.contains(&"review"));
    }

    #[tokio::test]
    async fn test_skill_load_not_found_with_registry() {
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

        // Create an empty registry
        let registry = SkillRegistry::new();
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services =
            crate::wasm::services::HostServices::with_skills(db, Arc::new(RwLock::new(registry)));
        let state = HostState::new().with_services(services);
        let mut store = Store::new(&engine, state);
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("Failed to instantiate");

        let test_func = instance
            .get_typed_func::<(), i32>(&mut store, "test")
            .expect("Failed to get test function");

        let result = test_func.call(&mut store, ()).expect("Failed to call test");
        assert_eq!(result, -1); // Not found
    }
}
