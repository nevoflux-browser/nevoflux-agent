//! Services available to host functions.
//!
//! This module provides the `HostServices` container that holds
//! dependencies needed by Wasm host functions to interact with
//! the NevoFlux system.

use nevoflux_llm::ProviderType;
use nevoflux_skills::SkillRegistry;
use nevoflux_storage::Database;
use std::sync::Arc;
use tokio::sync::RwLock;

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
        }
    }
}

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
        let skills = Arc::new(RwLock::new(SkillRegistry::new()));
        Self {
            database,
            skills,
            llm_config: None,
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
        }
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
}

impl std::fmt::Debug for HostServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostServices")
            .field("database", &"Arc<Database>")
            .field("skills", &"Arc<RwLock<SkillRegistry>>")
            .field("llm_config", &self.llm_config.as_ref().map(|_| "Some(...)"))
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
    fn test_host_services_with_llm_debug() {
        let db = Arc::new(Database::open_in_memory().expect("Failed to open in-memory database"));
        let config = LlmConfig::new(ProviderType::Qwen, "key", "model");
        let services = HostServices::new(db).with_llm(config);
        let debug_str = format!("{:?}", services);

        assert!(debug_str.contains("llm_config"));
        assert!(debug_str.contains("Some(...)"));
    }
}
