//! Persisted, user-editable project settings (`atelier.toml`).
//!
//! Distinct from [`Config`](crate::config::Config), which is ephemeral
//! connection info from the environment. `Settings` is durable state the user
//! manages — MCP servers, permissions, defaults, diagnostics checkers — read
//! at startup and written back when changed interactively (e.g. `/mcp add`).
//!
//! Example `atelier.toml`:
//! ```toml
//! [[mcp]]
//! name = "filesystem"
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The settings file name, resolved against the project root.
pub const FILE_NAME: &str = "atelier.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    /// Configured stdio MCP servers, launched at startup.
    #[serde(default)]
    pub mcp: Vec<McpServerConfig>,
    /// Configured HTTP (Streamable) MCP servers, connected at startup.
    #[serde(default)]
    pub mcp_http: Vec<HttpServerConfig>,
    /// Tool-approval policy.
    #[serde(default)]
    pub permissions: Permissions,
    /// Project defaults, overridden by the matching environment variables.
    #[serde(default)]
    pub defaults: Defaults,
    /// The parallel "subconscious" reviewer.
    #[serde(default)]
    pub review: ReviewSettings,
    /// Lint/typecheck commands whose output is fed back as per-turn
    /// diagnostics. Empty means "auto-detect from the project's files".
    #[serde(default)]
    pub diagnostics: Vec<DiagnosticCheck>,
}

/// One `[[diagnostics]]` checker: a read-only lint or typecheck command run
/// from the project root, whose output lines become the diagnostics the
/// model sees (see `context::diagnostics`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticCheck {
    /// Label the diagnostics are reported under.
    pub name: String,
    /// The command line, split like a shell would (quotes honored) and run
    /// directly — no shell, so no pipes or redirections.
    pub command: String,
    /// Globs, relative to the project root: the checker re-runs only when a
    /// matching file changed since its last run. Empty means every turn.
    #[serde(default)]
    pub when: Vec<String>,
    /// Seconds before the command is killed.
    #[serde(default = "default_diagnostic_timeout")]
    pub timeout_secs: u64,
}

/// A minute: what `cargo check` was always given.
fn default_diagnostic_timeout() -> u64 {
    60
}

/// The background reviewer ("subconscious"): a parallel, read-only pass that
/// watches the exchange and its edits and posts short notes into the main
/// dialog after tool calls. It can do nothing but observe and comment.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReviewSettings {
    /// Whether the reviewer runs.
    #[serde(default)]
    pub enabled: bool,
    /// Model for the reviewer (defaults to the main model when unset).
    #[serde(default)]
    pub model: Option<String>,
}

/// Per-project defaults from `[defaults]`. Each is overridden by its env var
/// (`ATELIER_MODEL`, `ATELIER_APPROVE`, `ATELIER_CONTEXT_LIMIT`) when that's set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Defaults {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub approve: Option<bool>,
    #[serde(default)]
    pub context_limit: Option<u32>,
}

/// Persisted tool-approval state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Permissions {
    /// Tool names the user has approved for all future runs ("always allow").
    #[serde(default)]
    pub allow: Vec<String>,
}

/// One stdio MCP server the user has configured.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// One HTTP (Streamable) MCP server the user has configured. `headers` are
/// `"Name: Value"` strings sent on every request (e.g. an auth token).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpServerConfig {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<String>,
}

impl Settings {
    /// The settings file path for a given project root.
    pub fn path(root: &Path) -> PathBuf {
        root.join(FILE_NAME)
    }

    /// Load settings from `<root>/atelier.toml`, or defaults if absent.
    pub fn load(root: &Path) -> Result<Self> {
        let path = Self::path(root);
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        tomlproc::serde::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Write settings back to `<root>/atelier.toml`.
    pub fn save(&self, root: &Path) -> Result<()> {
        let path = Self::path(root);
        let text = tomlproc::serde::to_string_pretty(self).context("serializing settings")?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_checkers_parse_with_defaults() {
        let settings: Settings = tomlproc::serde::from_str(
            r#"
[[diagnostics]]
name = "tsc"
command = "npx tsc --noEmit --pretty false"
when = ["**/*.ts", "**/*.tsx"]
timeout_secs = 90

[[diagnostics]]
name = "lint"
command = "make lint"
"#,
        )
        .unwrap();
        let [tsc, lint] = settings.diagnostics.as_slice() else {
            panic!("expected two checkers: {:?}", settings.diagnostics);
        };
        assert_eq!(tsc.when, ["**/*.ts", "**/*.tsx"]);
        assert_eq!(tsc.timeout_secs, 90);
        // Omitted: every turn, a minute.
        assert!(lint.when.is_empty());
        assert_eq!(lint.timeout_secs, 60);

        // And they survive the rewrite `/mcp add` and friends do.
        let text = tomlproc::serde::to_string_pretty(&settings).unwrap();
        let again: Settings = tomlproc::serde::from_str(&text).unwrap();
        assert_eq!(again.diagnostics.len(), 2);
        assert_eq!(again.diagnostics[0].command, tsc.command);
    }
}
