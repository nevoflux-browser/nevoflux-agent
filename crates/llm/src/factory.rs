//! Provider factory for easy provider instantiation.

/// Supported LLM provider types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderType {
    /// Anthropic Claude models
    Anthropic,
    /// OpenAI GPT models
    OpenAi,
    /// OpenRouter (multi-provider)
    OpenRouter,
    /// DeepSeek models
    DeepSeek,
    /// Alibaba Qwen models (via DashScope)
    Qwen,
}

/// Configuration for creating an LLM provider.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// The type of provider
    pub provider: ProviderType,
    /// API key for authentication
    pub api_key: String,
    /// Optional custom base URL
    pub base_url: Option<String>,
    /// Optional default model name
    pub default_model: Option<String>,
}

impl ProviderConfig {
    /// Create a new provider configuration.
    pub fn new(provider: ProviderType, api_key: impl Into<String>) -> Self {
        Self {
            provider,
            api_key: api_key.into(),
            base_url: None,
            default_model: None,
        }
    }

    /// Set a custom base URL.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// Set the default model name.
    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }
}

/// Get the default model name for a provider type.
pub fn default_model_for(provider: ProviderType) -> &'static str {
    match provider {
        ProviderType::Anthropic => "claude-sonnet-4-20250514",
        ProviderType::OpenAi => "gpt-4o-mini",
        ProviderType::OpenRouter => "anthropic/claude-3-haiku",
        ProviderType::DeepSeek => "deepseek-chat",
        ProviderType::Qwen => "qwen-turbo",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_type_equality() {
        assert_eq!(ProviderType::Anthropic, ProviderType::Anthropic);
        assert_ne!(ProviderType::Anthropic, ProviderType::OpenAi);
    }

    #[test]
    fn test_provider_config_new() {
        let config = ProviderConfig::new(ProviderType::Anthropic, "test-key");
        assert_eq!(config.provider, ProviderType::Anthropic);
        assert_eq!(config.api_key, "test-key");
        assert!(config.base_url.is_none());
        assert!(config.default_model.is_none());
    }

    #[test]
    fn test_provider_config_builder() {
        let config = ProviderConfig::new(ProviderType::Qwen, "qwen-key")
            .with_base_url("https://custom.example.com")
            .with_default_model("qwen-plus");

        assert_eq!(config.provider, ProviderType::Qwen);
        assert_eq!(
            config.base_url,
            Some("https://custom.example.com".to_string())
        );
        assert_eq!(config.default_model, Some("qwen-plus".to_string()));
    }

    #[test]
    fn test_default_model_for_anthropic() {
        assert_eq!(
            default_model_for(ProviderType::Anthropic),
            "claude-sonnet-4-20250514"
        );
    }

    #[test]
    fn test_default_model_for_openai() {
        assert_eq!(default_model_for(ProviderType::OpenAi), "gpt-4o-mini");
    }

    #[test]
    fn test_default_model_for_qwen() {
        assert_eq!(default_model_for(ProviderType::Qwen), "qwen-turbo");
    }

    #[test]
    fn test_default_model_for_openrouter() {
        assert_eq!(
            default_model_for(ProviderType::OpenRouter),
            "anthropic/claude-3-haiku"
        );
    }

    #[test]
    fn test_default_model_for_deepseek() {
        assert_eq!(default_model_for(ProviderType::DeepSeek), "deepseek-chat");
    }

    #[test]
    fn test_provider_type_debug() {
        assert_eq!(format!("{:?}", ProviderType::Anthropic), "Anthropic");
    }

    #[test]
    fn test_provider_config_clone() {
        let config = ProviderConfig::new(ProviderType::OpenAi, "key");
        let cloned = config.clone();
        assert_eq!(cloned.provider, ProviderType::OpenAi);
    }

    #[test]
    fn test_provider_type_copy() {
        let provider = ProviderType::DeepSeek;
        let copied = provider;
        assert_eq!(provider, copied);
    }

    #[test]
    fn test_provider_config_with_all_options() {
        let config = ProviderConfig::new(ProviderType::OpenRouter, "router-key")
            .with_base_url("https://api.openrouter.ai/api/v1")
            .with_default_model("anthropic/claude-3-opus");

        assert_eq!(config.provider, ProviderType::OpenRouter);
        assert_eq!(config.api_key, "router-key");
        assert_eq!(
            config.base_url,
            Some("https://api.openrouter.ai/api/v1".to_string())
        );
        assert_eq!(
            config.default_model,
            Some("anthropic/claude-3-opus".to_string())
        );
    }
}
