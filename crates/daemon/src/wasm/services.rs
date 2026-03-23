//! Services available to host functions.
//!
//! This module provides the `HostServices` container that holds
//! dependencies needed by Wasm host functions to interact with
//! the NevoFlux system.

use crate::agent::roles::AgentRoleRegistry;
use crate::learning::retriever::KnowledgeRetriever;
use crate::wasm::subagent::SubagentExecutor;
use nevoflux_computer::ComputerController;
use nevoflux_llm::{EmbeddingProvider, ProviderType};
use nevoflux_mcp::{McpManager, ToolSearchIndex};
use nevoflux_protocol::{BrowserToolAction, BrowserToolError};
use nevoflux_skills::SkillRegistry;
use nevoflux_storage::{Database, SimpleVectorIndex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, RwLock};

/// Shared embedding provider that can be lazily initialized in the background.
///
/// Initially `None`; set by the background embedding init task after the ONNX
/// model finishes loading.  All consumers (HostServices, KnowledgeRetriever,
/// LearningPipeline) share the same `Arc` and read-lock briefly to clone the
/// inner provider.
pub type SharedEmbedding = Arc<std::sync::RwLock<Option<Arc<dyn EmbeddingProvider>>>>;

/// Helper: read the current embedding provider from a [`SharedEmbedding`].
///
/// Returns `Some(Arc<dyn EmbeddingProvider>)` when the background init has
/// completed, `None` otherwise.
pub fn get_embedding(shared: &SharedEmbedding) -> Option<Arc<dyn EmbeddingProvider>> {
    shared.read().ok().and_then(|guard| guard.clone())
}

/// LLM configuration for host functions.
///
/// This struct holds the configuration needed to make LLM API calls
/// from Wasm guest modules.
#[derive(Clone, Debug)]
pub struct LlmConfig {
    /// The type of LLM provider to use.
    pub provider: ProviderType,
    /// The API key for authentication.
    pub api_key: String,
    /// The model name to use.
    pub model: String,
    /// Optional base URL override for the API endpoint.
    pub base_url: Option<String>,
}

impl LlmConfig {
    /// Create a new LLM configuration.
    ///
    /// # Arguments
    ///
    /// * `provider` - The type of LLM provider.
    /// * `api_key` - The API key for authentication.
    /// * `model` - The model name to use.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use nevoflux_daemon::wasm::LlmConfig;
    /// use nevoflux_llm::ProviderType;
    ///
    /// let config = LlmConfig::new(
    ///     ProviderType::Qwen,
    ///     "your-api-key",
    ///     "qwen-turbo"
    /// );
    /// ```
    pub fn new(
        provider: ProviderType,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            api_key: api_key.into(),
            model: model.into(),
            base_url: None,
        }
    }
}

/// Shared context for browser tool execution in Code Mode.
///
/// Bundles the sender channel with routing information needed to deliver
/// browser requests back to the correct proxy/sidebar. Shared via `Arc`
/// among all BrowserTool instances in a ToolRegistry.
#[derive(Debug, Clone)]
pub struct BrowserContext {
    /// Channel to send browser requests.
    pub sender: BrowserSender,
    /// Proxy ID for routing responses.
    pub proxy_id: String,
    /// Client identity bytes for routing responses.
    pub client_identity: Vec<u8>,
}

/// Browser tool request for the browser sender channel.
#[derive(Debug, Clone)]
pub struct BrowserRequest {
    /// Unique request ID.
    pub request_id: String,
    /// Session ID.
    pub session_id: String,
    /// Tab ID (None for active tab).
    pub tab_id: Option<i64>,
    /// Browser action to perform.
    pub action: BrowserToolAction,
    /// Action parameters.
    pub params: serde_json::Value,
    /// Timeout in milliseconds.
    pub timeout_ms: u64,
    /// Client identity for routing response back.
    pub client_identity: Vec<u8>,
    /// Proxy ID for the response envelope.
    pub proxy_id: String,
}

/// Browser tool response.
#[derive(Debug, Clone)]
pub struct BrowserResponse {
    /// Request ID this is responding to.
    pub request_id: String,
    /// Whether the operation succeeded.
    pub success: bool,
    /// Result data.
    pub result: Option<serde_json::Value>,
    /// Error information.
    pub error: Option<BrowserToolError>,
}

/// Type alias for browser request sender.
pub type BrowserSender = mpsc::Sender<(BrowserRequest, oneshot::Sender<BrowserResponse>)>;

/// Services container for host functions.
///
/// This struct holds shared references to services that Wasm guest modules
/// can access through host functions. It is designed to be cheaply cloneable
/// using `Arc` internally.
#[derive(Clone)]
pub struct HostServices {
    /// Database connection.
    pub database: Arc<Database>,
    /// Skills registry.
    pub skills: Arc<RwLock<SkillRegistry>>,
    /// LLM configuration for AI-powered features.
    pub llm_config: Option<LlmConfig>,
    /// Tool search index for keyword-based tool discovery.
    pub tool_search: Option<Arc<RwLock<ToolSearchIndex>>>,
    /// MCP Manager for calling dynamic tools.
    pub mcp_manager: Option<Arc<McpManager>>,
    /// Browser tool request sender.
    pub browser_sender: Option<BrowserSender>,
    /// Interrupt flag for stopping agent execution.
    ///
    /// Set to `true` when the user requests to stop the agent (e.g., clicks stop button).
    /// The agent loop checks this flag and gracefully exits when set.
    pub interrupt_flag: Arc<AtomicBool>,
    /// Subagent executor for spawning sandboxed sub-agents.
    ///
    /// When set, enables the subagent_spawn host function to create
    /// isolated WASM instances for sub-agent execution.
    pub subagent_executor: Option<Arc<SubagentExecutor>>,
    /// Agent role registry for subagent role definitions.
    pub role_registry: Option<Arc<AgentRoleRegistry>>,
    /// Current client identity for routing browser tool responses.
    pub client_identity: Vec<u8>,
    /// Current proxy ID for the response envelope.
    pub proxy_id: String,
    /// Current session ID for artifact creation and other session-scoped operations.
    pub session_id: String,
    /// Tools that user has approved "Always Allow" (shared across requests in the same daemon).
    pub always_allowed_tools: Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    /// Knowledge retriever for injecting learned context into agent execution.
    ///
    /// When set, enables the agent to retrieve relevant knowledge entries
    /// and site adaptations from the learning system.
    pub knowledge_retriever: Option<Arc<KnowledgeRetriever>>,
    /// Computer controller for screenshot/mouse/keyboard operations.
    pub computer_controller: Option<Arc<dyn ComputerController>>,
    /// Shared embedding provider (lazily initialized in background).
    pub embedding: SharedEmbedding,
    /// In-memory vector index for semantic similarity search.
    pub vector_index: Arc<std::sync::RwLock<SimpleVectorIndex>>,
}

impl HostServices {
    /// Create new services with the given database.
    ///
    /// Initializes a new `SkillRegistry` for the skills service.
    ///
    /// # Arguments
    ///
    /// * `database` - Shared database connection.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use nevoflux_daemon::wasm::HostServices;
    /// use nevoflux_storage::Database;
    /// use std::sync::Arc;
    ///
    /// let db = Arc::new(Database::open_in_memory().unwrap());
    /// let services = HostServices::new(db);
    /// ```
    pub fn new(database: Arc<Database>) -> Self {
        // Create skill registry and load skills from default directories
        let mut registry = SkillRegistry::new();
        if let Err(e) = registry.load() {
            tracing::warn!("Failed to load skills: {}", e);
        } else {
            tracing::info!("Loaded {} skills into registry", registry.len());
        }
        let skills = Arc::new(RwLock::new(registry));

        Self {
            database,
            skills,
            llm_config: None,
            tool_search: None,
            mcp_manager: None,
            browser_sender: None,
            interrupt_flag: Arc::new(AtomicBool::new(false)),
            subagent_executor: None,
            role_registry: None,
            client_identity: Vec::new(),
            proxy_id: String::new(),
            session_id: String::new(),
            always_allowed_tools: Arc::new(std::sync::RwLock::new(std::collections::HashSet::new())),
            knowledge_retriever: None,
            computer_controller: None,
            embedding: Arc::new(std::sync::RwLock::new(None)),
            vector_index: Arc::new(std::sync::RwLock::new(SimpleVectorIndex::new())),
        }
    }

    /// Create new services with an existing skills registry.
    ///
    /// # Arguments
    ///
    /// * `database` - Shared database connection.
    /// * `skills` - Shared skills registry.
    pub fn with_skills(database: Arc<Database>, skills: Arc<RwLock<SkillRegistry>>) -> Self {
        Self {
            database,
            skills,
            llm_config: None,
            tool_search: None,
            mcp_manager: None,
            browser_sender: None,
            interrupt_flag: Arc::new(AtomicBool::new(false)),
            subagent_executor: None,
            role_registry: None,
            client_identity: Vec::new(),
            proxy_id: String::new(),
            session_id: String::new(),
            always_allowed_tools: Arc::new(std::sync::RwLock::new(std::collections::HashSet::new())),
            knowledge_retriever: None,
            computer_controller: None,
            embedding: Arc::new(std::sync::RwLock::new(None)),
            vector_index: Arc::new(std::sync::RwLock::new(SimpleVectorIndex::new())),
        }
    }

    /// Build a `BrowserContext` from this service's browser_sender and routing info.
    ///
    /// Returns `None` if no browser_sender is configured.
    pub fn browser_context(&self) -> Option<BrowserContext> {
        self.browser_sender.clone().map(|sender| BrowserContext {
            sender,
            proxy_id: self.proxy_id.clone(),
            client_identity: self.client_identity.clone(),
        })
    }

    /// Add tool search index to the services.
    ///
    /// This enables the `tool_search` host function for keyword-based tool discovery.
    ///
    /// # Arguments
    ///
    /// * `index` - The tool search index to use.
    ///
    /// # Returns
    ///
    /// Returns self for method chaining.
    pub fn with_tool_search(mut self, index: ToolSearchIndex) -> Self {
        self.tool_search = Some(Arc::new(RwLock::new(index)));
        self
    }

    /// Add a shared tool search index (already wrapped in Arc<RwLock>).
    pub fn with_shared_tool_search(mut self, index: Arc<RwLock<ToolSearchIndex>>) -> Self {
        self.tool_search = Some(index);
        self
    }

    /// Add LLM configuration to the services.
    ///
    /// This enables the `llm_chat` host function to make LLM API calls.
    ///
    /// # Arguments
    ///
    /// * `config` - The LLM configuration to use.
    ///
    /// # Returns
    ///
    /// Returns self for method chaining.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use nevoflux_daemon::wasm::{HostServices, LlmConfig};
    /// use nevoflux_llm::ProviderType;
    /// use nevoflux_storage::Database;
    /// use std::sync::Arc;
    ///
    /// let db = Arc::new(Database::open_in_memory().unwrap());
    /// let services = HostServices::new(db)
    ///     .with_llm(LlmConfig::new(ProviderType::Qwen, "api-key", "qwen-turbo"));
    /// ```
    pub fn with_llm(mut self, config: LlmConfig) -> Self {
        self.llm_config = Some(config);
        self
    }

    /// Add MCP manager to the services.
    ///
    /// This enables the `tool_call_dynamic` host function to call tools
    /// discovered via tool search.
    ///
    /// # Arguments
    ///
    /// * `manager` - The MCP manager to use.
    ///
    /// # Returns
    ///
    /// Returns self for method chaining.
    pub fn with_mcp_manager(mut self, manager: Arc<McpManager>) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    /// Add browser sender to the services.
    ///
    /// This enables browser tool host functions to send requests to the
    /// browser extension via the proxy/bridge.
    ///
    /// # Arguments
    ///
    /// * `sender` - The browser request sender channel.
    ///
    /// # Returns
    ///
    /// Returns self for method chaining.
    pub fn with_browser_sender(mut self, sender: BrowserSender) -> Self {
        self.browser_sender = Some(sender);
        self
    }

    /// Add subagent executor to the services.
    ///
    /// This enables the `subagent_spawn` host function to create
    /// isolated WASM instances for sub-agent execution.
    ///
    /// # Arguments
    ///
    /// * `executor` - The subagent executor to use.
    ///
    /// # Returns
    ///
    /// Returns self for method chaining.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use nevoflux_daemon::wasm::{HostServices, SubagentExecutor};
    /// use nevoflux_daemon::config::SubagentConfig;
    /// use nevoflux_storage::Database;
    /// use std::sync::Arc;
    ///
    /// let db = Arc::new(Database::open_in_memory().unwrap());
    /// let rt = tokio::runtime::Handle::current();
    /// let executor = Arc::new(SubagentExecutor::new(SubagentConfig::default(), rt));
    /// let services = HostServices::new(db).with_subagent_executor(executor);
    /// ```
    pub fn with_subagent_executor(mut self, executor: Arc<SubagentExecutor>) -> Self {
        self.subagent_executor = Some(executor);
        self
    }

    /// Set the agent role registry.
    pub fn with_role_registry(mut self, registry: Arc<AgentRoleRegistry>) -> Self {
        self.role_registry = Some(registry);
        self
    }

    /// Get the agent role registry.
    pub fn role_registry(&self) -> Option<&Arc<AgentRoleRegistry>> {
        self.role_registry.as_ref()
    }

    /// Set the client context for routing browser tool responses.
    ///
    /// This stores the client identity and proxy ID so browser tool requests
    /// can be routed back to the correct client.
    ///
    /// # Arguments
    ///
    /// * `identity` - The identity bytes of the client connection (proxy_id as UTF-8).
    /// * `proxy_id` - The proxy ID for the response envelope.
    ///
    /// # Returns
    ///
    /// Returns self for method chaining.
    pub fn with_client_context(mut self, identity: Vec<u8>, proxy_id: String) -> Self {
        self.client_identity = identity;
        self.proxy_id = proxy_id;
        self
    }

    /// Set session ID for session-scoped operations (e.g. artifact creation).
    pub fn with_session_id(mut self, session_id: String) -> Self {
        self.session_id = session_id;
        self
    }

    /// Add a knowledge retriever to the services.
    ///
    /// This enables the agent to retrieve relevant knowledge entries
    /// and site adaptations from the learning system during execution.
    ///
    /// # Arguments
    ///
    /// * `retriever` - The knowledge retriever to use.
    ///
    /// # Returns
    ///
    /// Returns self for method chaining.
    pub fn with_knowledge_retriever(mut self, retriever: Arc<KnowledgeRetriever>) -> Self {
        self.knowledge_retriever = Some(retriever);
        self
    }

    /// Add a computer controller to the services.
    ///
    /// This enables computer control host functions (screenshot, mouse, keyboard).
    pub fn with_computer_controller(mut self, controller: Arc<dyn ComputerController>) -> Self {
        self.computer_controller = Some(controller);
        self
    }

    /// Add an embedding provider to the services.
    ///
    /// This enables generating vector embeddings for memory chunks,
    /// allowing hybrid FTS5+vector semantic search.
    ///
    /// # Arguments
    ///
    /// * `provider` - The embedding provider to use.
    ///
    /// # Returns
    ///
    /// Returns self for method chaining.
    pub fn with_embedding(mut self, shared: SharedEmbedding) -> Self {
        self.embedding = shared;
        self
    }

    /// Add a pre-existing vector index to the services.
    ///
    /// This replaces the default empty vector index with one that may
    /// already contain vectors (e.g., loaded from storage at startup).
    ///
    /// # Arguments
    ///
    /// * `index` - The vector index to use.
    ///
    /// # Returns
    ///
    /// Returns self for method chaining.
    pub fn with_vector_index(mut self, index: Arc<std::sync::RwLock<SimpleVectorIndex>>) -> Self {
        self.vector_index = index;
        self
    }

    /// Check if subagent execution is available.
    pub fn has_subagent_executor(&self) -> bool {
        self.subagent_executor.is_some()
    }

    /// Set the interrupt flag.
    ///
    /// When set to `true`, the agent loop will check this flag and
    /// gracefully stop execution at the next opportunity.
    ///
    /// # Arguments
    ///
    /// * `interrupted` - Whether to mark the session as interrupted.
    pub fn set_interrupted(&self, interrupted: bool) {
        self.interrupt_flag.store(interrupted, Ordering::Relaxed);
    }

    /// Check if the session has been interrupted.
    ///
    /// Returns `true` if the interrupt flag has been set.
    pub fn is_interrupted(&self) -> bool {
        self.interrupt_flag.load(Ordering::Relaxed)
    }

    /// Reset the interrupt flag.
    ///
    /// Call this at the start of a new agent run to clear any previous interrupt state.
    pub fn reset_interrupt(&self) {
        self.interrupt_flag.store(false, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for HostServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostServices")
            .field("database", &"Arc<Database>")
            .field("skills", &"Arc<RwLock<SkillRegistry>>")
            .field("llm_config", &self.llm_config.as_ref().map(|_| "Some(...)"))
            .field(
                "tool_search",
                &self.tool_search.as_ref().map(|_| "Some(...)"),
            )
            .field(
                "mcp_manager",
                &self.mcp_manager.as_ref().map(|_| "Some(...)"),
            )
            .field(
                "browser_sender",
                &self.browser_sender.as_ref().map(|_| "Some(...)"),
            )
            .field("interrupt_flag", &self.is_interrupted())
            .field(
                "subagent_executor",
                &self.subagent_executor.as_ref().map(|_| "Some(...)"),
            )
            .field(
                "role_registry",
                &self.role_registry.as_ref().map(|_| "Some(...)"),
            )
            .field(
                "knowledge_retriever",
                &self.knowledge_retriever.as_ref().map(|_| "Some(...)"),
            )
            .field(
                "computer_controller",
                &self.computer_controller.as_ref().map(|_| "Some(...)"),
            )
            .field(
                "embedding",
                &if get_embedding(&self.embedding).is_some() {
                    "Some(...)"
                } else {
                    "None (pending)"
                },
            )
            .field("vector_index", &"Arc<RwLock<SimpleVectorIndex>>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_services_creation() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services = HostServices::new(db);

        // Verify services are accessible
        assert!(Arc::strong_count(&services.database) >= 1);
        assert!(Arc::strong_count(&services.skills) >= 1);
    }

    #[test]
    fn test_host_services_with_skills() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let skills = Arc::new(RwLock::new(SkillRegistry::new()));
        let services = HostServices::with_skills(db, skills.clone());

        // Verify the same skills registry is used
        assert!(Arc::ptr_eq(&services.skills, &skills));
    }

    #[test]
    fn test_host_services_clone() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services = HostServices::new(db);
        let cloned = services.clone();

        // Verify both point to the same underlying data
        assert!(Arc::ptr_eq(&services.database, &cloned.database));
        assert!(Arc::ptr_eq(&services.skills, &cloned.skills));
    }

    #[test]
    fn test_host_services_debug() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services = HostServices::new(db);
        let debug_str = format!("{:?}", services);

        assert!(debug_str.contains("HostServices"));
        assert!(debug_str.contains("database"));
        assert!(debug_str.contains("skills"));
        assert!(debug_str.contains("llm_config"));
    }

    #[test]
    fn test_llm_config_new() {
        let config = LlmConfig::new(ProviderType::Qwen, "test-key", "qwen-turbo");

        assert_eq!(config.provider, ProviderType::Qwen);
        assert_eq!(config.api_key, "test-key");
        assert_eq!(config.model, "qwen-turbo");
    }

    #[test]
    fn test_llm_config_clone() {
        let config = LlmConfig::new(ProviderType::Qwen, "api-key", "qwen-plus");
        let cloned = config.clone();

        assert_eq!(cloned.provider, config.provider);
        assert_eq!(cloned.api_key, config.api_key);
        assert_eq!(cloned.model, config.model);
    }

    #[test]
    fn test_llm_config_debug() {
        let config = LlmConfig::new(ProviderType::Qwen, "secret-key", "qwen-max");
        let debug_str = format!("{:?}", config);

        assert!(debug_str.contains("LlmConfig"));
        assert!(debug_str.contains("Qwen"));
        assert!(debug_str.contains("qwen-max"));
    }

    #[test]
    fn test_host_services_with_llm() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let config = LlmConfig::new(ProviderType::Qwen, "test-key", "qwen-turbo");
        let services = HostServices::new(db).with_llm(config);

        assert!(services.llm_config.is_some());
        let llm_config = services.llm_config.unwrap();
        assert_eq!(llm_config.provider, ProviderType::Qwen);
        assert_eq!(llm_config.api_key, "test-key");
        assert_eq!(llm_config.model, "qwen-turbo");
    }

    #[test]
    fn test_host_services_without_llm() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services = HostServices::new(db);

        assert!(services.llm_config.is_none());
    }

    #[test]
    fn test_host_services_with_mcp_manager() {
        use nevoflux_mcp::ManagerConfig;

        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let manager = Arc::new(McpManager::new(ManagerConfig::default()));
        let services = HostServices::new(db).with_mcp_manager(manager);

        assert!(services.mcp_manager.is_some());
    }

    #[test]
    fn test_host_services_without_mcp_manager() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services = HostServices::new(db);

        assert!(services.mcp_manager.is_none());
    }

    #[test]
    fn test_host_services_with_llm_debug() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let config = LlmConfig::new(ProviderType::Qwen, "key", "model");
        let services = HostServices::new(db).with_llm(config);
        let debug_str = format!("{:?}", services);

        assert!(debug_str.contains("llm_config"));
        assert!(debug_str.contains("Some(...)"));
    }

    #[test]
    fn test_host_services_with_browser_sender() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let (tx, _rx) = mpsc::channel(10);
        let services = HostServices::new(db).with_browser_sender(tx);

        assert!(services.browser_sender.is_some());
    }

    #[test]
    fn test_host_services_without_browser_sender() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services = HostServices::new(db);

        assert!(services.browser_sender.is_none());
    }

    #[test]
    fn test_browser_request_creation() {
        let request = BrowserRequest {
            request_id: "req-001".into(),
            session_id: "sess-001".into(),
            tab_id: Some(123),
            action: BrowserToolAction::Navigate,
            params: serde_json::json!({"url": "https://example.com"}),
            timeout_ms: 30000,
            client_identity: vec![1, 2, 3],
            proxy_id: "proxy-001".into(),
        };

        assert_eq!(request.request_id, "req-001");
        assert_eq!(request.action, BrowserToolAction::Navigate);
        assert_eq!(request.client_identity, vec![1, 2, 3]);
        assert_eq!(request.proxy_id, "proxy-001");
    }

    #[test]
    fn test_browser_response_success() {
        let response = BrowserResponse {
            request_id: "req-001".into(),
            success: true,
            result: Some(serde_json::json!({"url": "https://example.com"})),
            error: None,
        };

        assert!(response.success);
        assert!(response.result.is_some());
        assert!(response.error.is_none());
    }

    #[test]
    fn test_browser_response_error() {
        let response = BrowserResponse {
            request_id: "req-001".into(),
            success: false,
            result: None,
            error: Some(BrowserToolError {
                code: 404,
                message: "Element not found".into(),
                recoverable: true,
            }),
        };

        assert!(!response.success);
        assert!(response.error.is_some());
    }

    #[test]
    fn test_host_services_interrupt_flag_default() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services = HostServices::new(db);

        // Default should be not interrupted
        assert!(!services.is_interrupted());
    }

    #[test]
    fn test_host_services_set_interrupted() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services = HostServices::new(db);

        // Set interrupted
        services.set_interrupted(true);
        assert!(services.is_interrupted());

        // Reset
        services.set_interrupted(false);
        assert!(!services.is_interrupted());
    }

    #[test]
    fn test_host_services_reset_interrupt() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services = HostServices::new(db);

        // Set interrupted and then reset
        services.set_interrupted(true);
        assert!(services.is_interrupted());

        services.reset_interrupt();
        assert!(!services.is_interrupted());
    }

    #[test]
    fn test_host_services_interrupt_flag_shared() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services = HostServices::new(db);
        let cloned = services.clone();

        // Set on one, should be visible on the other
        services.set_interrupted(true);
        assert!(cloned.is_interrupted());

        // Reset on clone, should be visible on original
        cloned.reset_interrupt();
        assert!(!services.is_interrupted());
    }

    #[test]
    fn test_host_services_debug_shows_interrupt_flag() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services = HostServices::new(db);
        let debug_str = format!("{:?}", services);

        assert!(debug_str.contains("interrupt_flag"));
        assert!(debug_str.contains("false"));
    }

    #[test]
    fn test_host_services_without_subagent_executor() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services = HostServices::new(db);

        assert!(!services.has_subagent_executor());
        assert!(services.subagent_executor.is_none());
    }

    #[test]
    fn test_host_services_with_subagent_executor() {
        use crate::config::SubagentConfig;
        use crate::wasm::subagent::SubagentExecutor;

        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));

        // Create a runtime for the executor
        let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
        let executor = Arc::new(SubagentExecutor::new(
            SubagentConfig::default(),
            rt.handle().clone(),
        ));

        let services = HostServices::new(db).with_subagent_executor(executor);

        assert!(services.has_subagent_executor());
        assert!(services.subagent_executor.is_some());
    }

    #[test]
    fn test_host_services_subagent_executor_debug() {
        use crate::config::SubagentConfig;
        use crate::wasm::subagent::SubagentExecutor;

        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
        let executor = Arc::new(SubagentExecutor::new(
            SubagentConfig::default(),
            rt.handle().clone(),
        ));

        let services = HostServices::new(db).with_subagent_executor(executor);
        let debug_str = format!("{:?}", services);

        assert!(debug_str.contains("subagent_executor"));
        assert!(debug_str.contains("Some(...)"));
    }

    #[test]
    fn test_host_services_without_knowledge_retriever() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let services = HostServices::new(db);

        assert!(services.knowledge_retriever.is_none());
    }

    #[test]
    fn test_host_services_with_knowledge_retriever() {
        use crate::learning::retriever::KnowledgeRetriever;
        use crate::learning::soul::manager::FiveDocCache;
        use nevoflux_storage::Storage;

        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let cache = Arc::new(FiveDocCache {
            identity_raw: String::new(),
            soul_raw: String::new(),
            user_raw: String::new(),
            tools_raw: String::new(),
            agents_raw: String::new(),
            last_parsed_at: chrono::Utc::now(),
        });
        let retriever = Arc::new(KnowledgeRetriever::new(cache, storage));

        let services = HostServices::new(db).with_knowledge_retriever(retriever.clone());

        assert!(services.knowledge_retriever.is_some());
        assert!(Arc::ptr_eq(
            services.knowledge_retriever.as_ref().unwrap(),
            &retriever
        ));
    }

    #[test]
    fn test_host_services_knowledge_retriever_clone_shares_arc() {
        use crate::learning::retriever::KnowledgeRetriever;
        use crate::learning::soul::manager::FiveDocCache;
        use nevoflux_storage::Storage;

        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let cache = Arc::new(FiveDocCache {
            identity_raw: String::new(),
            soul_raw: String::new(),
            user_raw: String::new(),
            tools_raw: String::new(),
            agents_raw: String::new(),
            last_parsed_at: chrono::Utc::now(),
        });
        let retriever = Arc::new(KnowledgeRetriever::new(cache, storage));

        let services = HostServices::new(db).with_knowledge_retriever(retriever);
        let cloned = services.clone();

        // Both should point to the same Arc
        assert!(Arc::ptr_eq(
            services.knowledge_retriever.as_ref().unwrap(),
            cloned.knowledge_retriever.as_ref().unwrap(),
        ));
    }

    #[test]
    fn test_host_services_knowledge_retriever_debug() {
        use crate::learning::retriever::KnowledgeRetriever;
        use crate::learning::soul::manager::FiveDocCache;
        use nevoflux_storage::Storage;

        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let cache = Arc::new(FiveDocCache {
            identity_raw: String::new(),
            soul_raw: String::new(),
            user_raw: String::new(),
            tools_raw: String::new(),
            agents_raw: String::new(),
            last_parsed_at: chrono::Utc::now(),
        });
        let retriever = Arc::new(KnowledgeRetriever::new(cache, storage));

        let services = HostServices::new(db).with_knowledge_retriever(retriever);
        let debug_str = format!("{:?}", services);

        assert!(debug_str.contains("knowledge_retriever"));
        assert!(debug_str.contains("Some(...)"));
    }
}
