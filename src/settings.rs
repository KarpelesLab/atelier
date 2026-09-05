//! Persisted, user-editable project settings (`atelier.toml`).
//!
//! Distinct from [`Config`](crate::config::Config), which is ephemeral
//! connection info from the environment. `Settings` is durable state the user
//! manages — today, the list of MCP servers — read at startup and written back
//! when changed interactively (e.g. `/mcp add`).
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
    /// Configured MCP servers, launched at startup.
    #[serde(default)]
    pub mcp: Vec<McpServerConfig>,
    /// Tool-approval policy.
    #[serde(default)]
    pub permissions: Permissions,
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
