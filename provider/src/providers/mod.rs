pub mod anthropic;
pub mod generic;
pub mod github_copilot;
pub mod google;
pub mod openai;

pub use generic::GenericProvider;

/// Get a provider interface for any of the 103 providers.
pub fn for_provider(id: &str) -> Option<GenericProvider> {
    GenericProvider::new(id)
}
