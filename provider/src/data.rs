use std::sync::LazyLock;
use crate::types::ProviderSpec;

pub static PROVIDERS_JSON: &str = include_str!("data/providers.json");

pub static ALL_PROVIDERS: LazyLock<Vec<ProviderSpec>> = LazyLock::new(|| {
    serde_json::from_str(PROVIDERS_JSON)
        .expect("Embedded provider data must be valid JSON matching ProviderSpec")
});
