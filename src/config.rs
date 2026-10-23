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
/// Default Claude Code executable for the Claude backend.
pub const DEFAULT_CLAUDE_BIN: &str = "claude";

/// Where turns are sent.
#[derive(Debug, Clone, PartialEq)]
pub enum Backend {
    /// An OpenAI-compatible chat-completions endpoint, driven by atelier's own
    /// agent loop.
    OpenAi,
    /// The user's installed Claude Code CLI, driven as a subprocess (see
    /// `src/claude`).
    Claude {
        /// The `claude` executable.
        bin: String,
        /// `--model` to pass, if the user chose one.
        model: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Base URL including the `/v1` suffix; endpoints are appended to it.
    pub base_url: String,
    /// Model id to request (OpenAI backend), or the display label of the
    /// model serving the session (Claude backend).
    pub model: String,
    /// Optional bearer token. Many local servers ignore auth entirely.
    pub api_key: Option<String>,
    pub backend: Backend,
}

impl Config {
    /// Build config from the environment, falling back to defaults.
    ///
    /// - `ATELIER_BACKEND`    — `claude` for the Claude backend (also selected
    ///   by `claude`, e.g. from the `--claude` flag); anything else or unset
    ///   means OpenAI-compatible
    /// - `ATELIER_BASE_URL`   — endpoint base (default [`DEFAULT_BASE_URL`])
    /// - `ATELIER_MODEL`      — model id (default [`DEFAULT_MODEL`]); with the
    ///   Claude backend, passed as `--model` only when set
    /// - `ATELIER_API_KEY`    — bearer token (optional)
    /// - `ATELIER_CLAUDE_BIN` — Claude Code executable (default
    ///   [`DEFAULT_CLAUDE_BIN`])
    pub fn from_env(claude: bool) -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
        let base_url = env("ATELIER_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let api_key = env("ATELIER_API_KEY");
        let model_env = env("ATELIER_MODEL");
        let claude = claude || env("ATELIER_BACKEND").as_deref() == Some("claude");
        let (backend, model) = if claude {
            let bin = env("ATELIER_CLAUDE_BIN").unwrap_or_else(|| DEFAULT_CLAUDE_BIN.to_string());
            let label = model_env.clone().unwrap_or_else(|| "claude".to_string());
            (
                Backend::Claude {
                    bin,
                    model: model_env,
                },
                label,
            )
        } else {
            (
                Backend::OpenAi,
                model_env.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            )
        };
        Self {
            base_url,
            model,
            api_key,
            backend,
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
