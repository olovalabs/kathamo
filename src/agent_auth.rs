//! Provider discovery and authentication through the embedded agent.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct Provider {
    pub id: String,
    pub ready: bool,
    pub source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelBackend {
    Rpi,
    #[serde(alias = "Omp")]
    Kathamo,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentModel {
    pub provider: String,
    pub id: String,
    pub backend: ModelBackend,
}

impl AgentModel {
    pub fn is_gemini_flash(&self) -> bool {
        matches!(self.provider.as_str(), "google" | "google-antigravity")
            && self.id == "gemini-3.8-flash"
    }

    /// Gemini 3.8 and every Antigravity route accept a per-turn thinking level.
    pub fn supports_reasoning_effort(&self) -> bool {
        self.provider == "google-antigravity" || self.is_gemini_flash()
    }
}

pub fn available_models(_cwd: &Path) -> Result<Vec<AgentModel>, String> {
    let mut models: Vec<_> = rpi_embedded::chat_catalog()?
        .into_iter()
        .map(|model| AgentModel {
            provider: model.provider,
            id: model.id,
            backend: ModelBackend::Rpi,
        })
        .collect();
    models.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
    models.dedup();
    Ok(models)
}

pub fn providers(cwd: &Path) -> Result<Vec<Provider>, String> {
    rpi_embedded::refresh_auth_methods(cwd)?;
    Ok(rpi_embedded::providers()?
        .into_iter()
        .map(|state| Provider {
            id: state.id,
            ready: state.ready,
            source: state.source,
        })
        .collect())
}

pub use rpi_embedded::{Login, LoginEvent, cached_auth_methods as auth_methods};

pub fn key_label(provider: &str) -> &'static str {
    if provider == "google" {
        "Gemini API key"
    } else {
        "API key"
    }
}

pub fn save_api_key(provider: &str, key: &str) -> Result<(), String> {
    rpi_embedded::save_api_key(provider, key)
}

pub fn disconnect_provider(provider: &str) -> Result<(), String> {
    rpi_embedded::disconnect_provider(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_gemini_and_antigravity_models_through_the_embedded_catalog() {
        let cwd = Path::new(".");
        let models = available_models(cwd).unwrap();
        assert!(
            models
                .iter()
                .any(|model| model.provider == "google" && model.is_gemini_flash())
        );
        for id in [
            "gemini-3.8-flash",
            "gemini-3.5-flash-lite",
            "gemini-3.1-flash-lite",
            "gemini-2.5-flash",
            "gemini-2.5-flash-lite",
            "claude-sonnet-4-6",
        ] {
            assert!(
                models
                    .iter()
                    .any(|model| model.provider == "google-antigravity" && model.id == id),
                "missing Antigravity model {id}"
            );
        }
        assert!(
            models
                .iter()
                .filter(|model| model.provider == "google-antigravity")
                .all(AgentModel::supports_reasoning_effort)
        );
        assert!(
            providers(cwd)
                .unwrap()
                .iter()
                .any(|provider| provider.id == "google")
        );
    }
}
