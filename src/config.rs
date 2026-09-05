//! Runtime configuration.
//!
//! For M0 this is a thin layer over environment variables with sensible
//! defaults pointing at our test server. Later milestones overlay an
//! `atelier.toml` (project + user global) on top of these — see the roadmap's
//! "Config precedence" open decision.

/// Default OpenAI-compatible endpoint (our test server).
pub const DEFAULT_BASE_URL: &str = "http://192.168.0.50:11400/v1";
/// Default model — thinking- and vision-capable.
pub const DEFAULT_MODEL: &str = "qwen3.8-unc:q4";

#[derive(Debug, Clone)]
pub struct Config {
    /// Base URL including the `/v1` suffix; endpoints are appended to it.
    pub base_url: String,
    /// Model id to request.
    pub model: String,
    /// Optional bearer token. Many local servers ignore auth entirely.
    pub api_key: Option<String>,
}

impl Config {
    /// Build config from the environment, falling back to defaults.
    ///
    /// - `ATELIER_BASE_URL` — endpoint base (default [`DEFAULT_BASE_URL`])
    /// - `ATELIER_MODEL`    — model id   (default [`DEFAULT_MODEL`])
    /// - `ATELIER_API_KEY`  — bearer token (optional)
    pub fn from_env() -> Self {
        let base_url =
            std::env::var("ATELIER_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let model = std::env::var("ATELIER_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let api_key = std::env::var("ATELIER_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        Self {
            base_url,
            model,
            api_key,
        }
    }

    /// Full URL for an endpoint path like `"chat/completions"`.
    pub fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }
}
