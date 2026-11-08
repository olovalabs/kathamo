pub mod auth;
pub mod catalog;
pub mod data;
pub mod models;
pub mod providers;
pub mod types;

// Re-export common types and functions for ergonomic access
pub use auth::storage::{disconnect_provider, restore_stored_credentials, save_api_key};
pub use auth::{check_all_auth, check_auth};
pub use catalog::{
    all_models, all_providers, api_key_providers, default_model_for, get_provider,
    oauth_providers, provider_ids, provider_models, search_models, summary, CatalogSummary,
};
pub use models::format_model_label;
pub use types::{
    AuthStatus, AuthType, LoginConfig, ModelSpec, ProviderSpec, RefreshConfig, StoredCredential,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_103_providers_loaded() {
        let providers = all_providers();
        assert_eq!(
            providers.len(),
            103,
            "Must load all 103 providers from oh-my-pi"
        );

        let sum = summary();
        assert_eq!(sum.total_providers, 103);
        assert!(sum.oauth_providers >= 20, "Should have 20+ OAuth providers");
        assert!(sum.api_key_providers >= 60, "Should have 60+ API key providers");
        assert!(sum.total_models > 1000, "Should have thousands of models");
    }

    #[test]
    fn test_core_providers_exist() {
        let expected = [
            "openai",
            "openai-codex",
            "anthropic",
            "google",
            "google-antigravity",
            "google-gemini-cli",
            "github-copilot",
            "deepseek",
            "groq",
            "xai",
            "xai-oauth",
            "mistral",
            "openrouter",
            "cerebras",
            "together",
            "cursor",
            "devin",
            "kimi-code",
            "zai",
            "alibaba-coding-plan",
            "xiaomi",
            "minimax",
            "ollama",
        ];

        for id in expected {
            let p = get_provider(id);
            assert!(p.is_some(), "Provider '{id}' must exist in catalog");
        }
    }

    #[test]
    fn test_oauth_providers() {
        let oauth_list = oauth_providers();
        let codex = oauth_list.iter().find(|p| p.id == "openai-codex");
        assert!(codex.is_some());
        let codex = codex.unwrap();
        assert!(codex.login.is_some());
        assert_eq!(codex.login.as_ref().unwrap().kind, "oauth-code");

        let anthropic = oauth_list.iter().find(|p| p.id == "anthropic");
        assert!(anthropic.is_some());
        assert!(anthropic.unwrap().supports_oauth());
    }

    #[test]
    fn test_api_key_resolution() {
        let deepseek = get_provider("deepseek").unwrap();
        assert!(deepseek.supports_api_key());
        assert!(deepseek.env_vars.contains(&"DEEPSEEK_API_KEY".to_string()));
    }

    #[test]
    fn test_model_search() {
        let results = search_models("claude-sonnet");
        assert!(!results.is_empty(), "Search should find Claude Sonnet models");

        let gpt_results = search_models("gpt-5");
        assert!(!gpt_results.is_empty(), "Search should find GPT-5 models");
    }

    #[test]
    fn test_api_key_persistence_and_restore() {
        let provider = "anthropic";
        let test_key = "test-sk-anthropic-persist-12345";

        // 1. Save API key
        save_api_key(provider, test_key).expect("save_api_key should succeed");

        // 2. Clear env var to simulate application close
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        assert!(std::env::var("ANTHROPIC_API_KEY").is_err());

        // 3. Restore credentials on simulated restart
        restore_stored_credentials();
        assert_eq!(std::env::var("ANTHROPIC_API_KEY").unwrap(), test_key);

        // 4. Check auth status
        let status = check_auth(provider);
        assert!(status.ready, "Provider should be ready after restore");

        // 5. Clean up
        disconnect_provider(provider).expect("disconnect should succeed");
        assert!(std::env::var("ANTHROPIC_API_KEY").is_err());
    }
}
