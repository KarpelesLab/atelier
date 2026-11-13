//! Persisted, user-editable project settings (`atelier.toml`).
//!
//! Distinct from [`Config`](crate::config::Config), which is ephemeral
//! connection info from the environment. `Settings` is durable state the user
//! manages — MCP servers, permissions, defaults, diagnostics checkers, hooks —
//! read at startup and written back when changed interactively (e.g. `/mcp add`).
//!
//! Example `atelier.toml`:
//! ```toml
//! [[mcp]]
//! name = "filesystem"
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
//! ```

use std::collections::BTreeMap;
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
    /// Language servers for the `symbols` tool, by file extension. They
    /// replace the auto-detected server for the extensions they list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lsp: Vec<LspServerConfig>,
    /// Which provider the `web_search` tool queries.
    #[serde(default)]
    pub search: SearchSettings,
    /// Commands run on lifecycle events (see [`crate::hooks`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<HookConfig>,
    /// Named model endpoints (see [`crate::config`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider: Vec<ProviderConfig>,
    /// Which model serves which role.
    #[serde(default, skip_serializing_if = "ModelRoles::is_empty")]
    pub models: ModelRoles,
    /// `[variant.<name>]`: request options a model is addressed with as
    /// `model#name`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variant: BTreeMap<String, VariantConfig>,
    /// Token prices, for the cost column of `atelier stats` and `/usage`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub price: Vec<PriceConfig>,
    /// Bounds on the pursuit of a `/goal`.
    #[serde(default, skip_serializing_if = "GoalSettings::is_unset")]
    pub goals: GoalSettings,
    /// Running `bash` commands in a sandbox (see [`crate::sandbox`]).
    #[serde(default, skip_serializing_if = "SandboxSettings::is_unset")]
    pub sandbox: SandboxSettings,
    /// More instruction files to load.
    #[serde(default, skip_serializing_if = "InstructionSettings::is_unset")]
    pub instructions: InstructionSettings,
    /// Limits on subagents (see `agent::subagent`).
    #[serde(default, skip_serializing_if = "AgentSettings::is_empty")]
    pub agents: AgentSettings,
    /// This instance among the others (see [`crate::coord`]).
    #[serde(default, skip_serializing_if = "CoordinatorSettings::is_unset")]
    pub coordinator: CoordinatorSettings,
    /// Bounds on `/loop`.
    #[serde(default, skip_serializing_if = "LoopSettings::is_unset")]
    pub loops: LoopSettings,
}

/// `[loops]`: how long a `/loop` may go on (see `agent::loops`).
///
/// ```toml
/// [loops]
/// max_runs = 20
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LoopSettings {
    /// Times one loop fires before it stops by itself (default 50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<u32>,
}

impl LoopSettings {
    fn is_unset(&self) -> bool {
        *self == Self::default()
    }
}

/// `[coordinator]`: whether other atelier instances may set this one to
/// work while it is idle (see `agent::idle` and `docs/coordinator.md`).
/// `ATELIER_COORDINATOR_WAKE` overrides `wake`.
///
/// ```toml
/// [coordinator]
/// wake = "questions"
/// wake_max = 5
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorSettings {
    /// What starts a turn in an idle instance: `never` (default),
    /// `questions` or `always`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake: Option<crate::coord::Wake>,
    /// Turns the instance may start by itself before its user types again
    /// (default 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_max: Option<u32>,
}

impl CoordinatorSettings {
    fn is_unset(&self) -> bool {
        *self == Self::default()
    }
}

/// `[sandbox]`: run the `bash` tool's commands inside nixvm (see
/// [`crate::sandbox`] and `docs/sandbox.md`). `ATELIER_SANDBOX` overrides
/// `enabled`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SandboxSettings {
    /// Route foreground `bash` commands through the sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// The guest's root filesystem, an extracted Linux userland. Honoured
    /// only in the user-global file: a project must not choose what the
    /// guest can read of the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Hosts the guest may reach. Not implemented yet: the guest has no
    /// network, and a list here is reported as ignored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network: Vec<String>,
    /// Guest RAM ceiling in MiB (default 4096).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u64>,
}

impl SandboxSettings {
    fn is_unset(&self) -> bool {
        *self == Self::default()
    }
}

/// `[instructions]`: instruction files beyond the `AGENTS.md` found by name
/// (see `context::instructions`).
///
/// ```toml
/// [instructions]
/// files = ["docs/conventions.md", "team/*.md"]
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InstructionSettings {
    /// Paths or globs, loaded in this order (a glob's matches sorted). In a
    /// project's file they are relative to the project root and confined to
    /// the repository; in the user-global file they may be absolute or
    /// start with `~/`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

impl InstructionSettings {
    /// Nothing set: the table is left out when the settings are written.
    fn is_unset(&self) -> bool {
        self.files.is_empty()
    }
}

/// `[goals]`: how far one prompt may go in pursuit of the session's goal
/// (`/goal`, `--goal` — see `agent::goal`) before the turn ends with the
/// goal unmet.
///
/// ```toml
/// [goals]
/// max_rounds = 5
/// max_tokens = 50000
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GoalSettings {
    /// Times the agent is sent back to work for one prompt (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
    /// Output tokens one prompt may generate while the goal is unmet
    /// (default: no limit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
}

impl GoalSettings {
    /// Nothing set: the table is left out when the settings are written.
    fn is_unset(&self) -> bool {
        *self == Self::default()
    }
}

/// `[agents]`: limits on the subagents the `task` tool runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentSettings {
    /// The most background agents running at once; a `task` call past it
    /// is refused. Unset means the built-in default (4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_background: Option<usize>,
}

impl AgentSettings {
    fn is_empty(&self) -> bool {
        self.max_background.is_none()
    }
}

/// One `[[provider]]`: an OpenAI-compatible endpoint with a name, addressed
/// as `name/model`. The key is named, never held: this file gets committed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    /// Base URL including the `/v1` suffix; request paths are appended.
    pub base_url: String,
    /// Name of the environment variable holding the bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Shell command (`sh -c`) whose standard output is the bearer token —
    /// for a password manager or credentials that rotate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_command: Option<String>,
    /// Extra headers sent with every request to this provider.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Connect timeout in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// `[models]`: a model per role, each `provider/model` or a bare model name
/// on the default provider, optionally with a `#variant`. Unset roles fall
/// back as [`crate::config::Role`] describes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelRoles {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl ModelRoles {
    fn is_empty(&self) -> bool {
        [
            &self.main,
            &self.small,
            &self.subagent,
            &self.review,
            &self.summary,
        ]
        .iter()
        .all(|r| r.is_none())
    }
}

/// One `[variant.<name>]`: options merged into the request body of a model
/// addressed as `model#name`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VariantConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Further body fields, merged as they are (for what a particular
    /// server understands: `top_k`, `seed`, …).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// One `[[price]]`: what a model costs per million tokens, in whatever
/// currency the figures are in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceConfig {
    /// `provider/model`, or a bare model name for that model on any provider.
    pub model: String,
    /// Per million prompt tokens.
    pub input: f64,
    /// Per million completion tokens.
    pub output: f64,
    /// Per million prompt tokens the provider served from its cache
    /// (default: the `input` price).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached: Option<f64>,
}

/// `[search]`: the `web_search` tool's provider. Each field is overridden by
/// its environment variable (`ATELIER_SEARCH_PROVIDER`, `ATELIER_SEARCH_URL`,
/// `ATELIER_SEARCH_KEY`). All unset means DuckDuckGo, which needs no key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchSettings {
    /// `searxng`, `brave`, `tavily` or `duckduckgo`.
    #[serde(default)]
    pub provider: Option<String>,
    /// The SearXNG instance's base URL; for the other providers, an override
    /// of their endpoint.
    #[serde(default)]
    pub url: Option<String>,
    /// Name of the environment variable holding the API key. The key itself
    /// is never written here: this file gets committed.
    #[serde(default)]
    pub key_env: Option<String>,
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

/// One `[[lsp]]` table: a language server the `symbols` tool starts for
/// files with one of `extensions` (see [`crate::lsp`]).
///
/// ```toml
/// [[lsp]]
/// name = "rust"
/// command = "rust-analyzer"
/// extensions = ["rs"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspServerConfig {
    /// What the server is called in results.
    pub name: String,
    /// The command line, split like a shell would (quotes honored) and run
    /// directly in the project root — no shell. It must speak LSP on its
    /// stdin and stdout.
    pub command: String,
    /// File extensions it serves, with or without the dot.
    pub extensions: Vec<String>,
    /// Seconds one `symbols` call waits for it before falling back to
    /// textual search.
    #[serde(default = "default_lsp_timeout")]
    pub timeout_secs: u64,
}

impl LspServerConfig {
    /// Whether files with extension `ext` (no dot) are this server's.
    pub fn serves(&self, ext: &str) -> bool {
        let listed = self.extensions.iter().map(|e| e.trim_start_matches('.'));
        listed.into_iter().any(|e| e.eq_ignore_ascii_case(ext))
    }
}

fn default_lsp_timeout() -> u64 {
    crate::lsp::DEFAULT_TIMEOUT.as_secs()
}

/// One `[[hooks]]` table: a command to run when `event` happens. Kept as
/// written — the event name is checked by the hook engine, so a typo there
/// costs that one hook a warning rather than the whole file its settings.
///
/// ```toml
/// [[hooks]]
/// event = "post_tool"
/// match = "edit|write|multiedit"
/// command = "rustfmt --edition 2024 \"$ATELIER_FILE\""
/// timeout_secs = 30
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    /// The event's name (`pre_tool`, `turn_end`, …).
    pub event: String,
    /// Regex the tool name must match in full (tool events only).
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    /// Run with `sh -c` in the project root.
    pub command: String,
    /// Seconds before the command is killed (default 60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
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
/// (`ATELIER_MODEL`, `ATELIER_APPROVE`, `ATELIER_MODE`, `ATELIER_CONTEXT_LIMIT`,
/// `ATELIER_PARALLEL_TOOLS`) when that's set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Defaults {
    #[serde(default)]
    pub model: Option<String>,
    /// The provider a bare model name is on (default: `env`, the endpoint
    /// the environment describes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// `true` is the `yolo` mode, where `mode` does not say otherwise.
    #[serde(default)]
    pub approve: Option<bool>,
    /// The permission mode sessions start in (`plan`, `ask-edits`,
    /// `auto-edit`, `yolo` — see [`crate::permissions::Mode`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<crate::permissions::Mode>,
    #[serde(default)]
    pub context_limit: Option<u32>,
    /// Bytes of MCP tool specs advertised before the tools are deferred
    /// behind `mcp_search` / `mcp_call` (see [`crate::mcp::deferred`]);
    /// `0` never defers. Unset means the built-in default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_tool_budget: Option<usize>,
    /// `false` runs the read-only tool calls of one model step one after the
    /// other instead of at the same time (see `agent::batch`). Unset: on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tools: Option<bool>,
}

/// Tool-approval rules: `tool` or `tool(pattern)` strings, in three lists.
/// Deny wins, then ask, then allow — see [`crate::permissions`] for the
/// syntax and how each is matched.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Permissions {
    /// Calls that run without asking. "Always" at an approval prompt adds
    /// here.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Calls that prompt even when they would otherwise run unasked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ask: Vec<String>,
    /// Calls that are refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// `[permissions.reviewer]`: a model that answers approval prompts
    /// (see [`crate::permissions::reviewer`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<ReviewerSettings>,
}

/// `[permissions.reviewer]`: a model that judges a tool call against a
/// written policy before the user would be asked about it (see
/// [`crate::permissions::reviewer`]).
///
/// ```toml
/// [permissions.reviewer]
/// enabled = true
/// model = "local/judge#low"
/// policy = ".atelier/approval-policy.md"
/// when = "unattended"
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReviewerSettings {
    /// Whether the reviewer is consulted at all.
    #[serde(default)]
    pub enabled: bool,
    /// The model that judges, as `provider/model#variant` (default: the
    /// `review` role, then the main model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// A file holding the policy, in place of the built-in one. In a
    /// project's file it is relative to the project root; in the
    /// user-global file, to that file's directory (or absolute, or `~/…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    /// Which prompts the reviewer answers.
    #[serde(default)]
    pub when: ReviewerWhen,
    /// Seconds the reviewer's one request may take (default 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// Which approval prompts go to the reviewer first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewerWhen {
    /// Only where there is no user to ask: a call that would be refused
    /// for want of an answer is put to the reviewer instead.
    #[default]
    Unattended,
    /// Before interactive prompts too: the user is asked only when the
    /// reviewer leaves the call to them.
    Always,
}

/// One stdio MCP server the user has configured.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(flatten)]
    pub options: McpOptions,
}

/// One HTTP (Streamable) MCP server the user has configured. `headers` are
/// `"Name: Value"` strings sent on every request (e.g. an auth token).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpServerConfig {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<String>,
    /// A client id registered with the server's OAuth authorization server,
    /// used by `/mcp login`. Without one atelier registers itself (RFC 7591).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(flatten)]
    pub options: McpOptions,
}

/// What `[[mcp]]` and `[[mcp_http]]` tables have in common beyond how the
/// server is reached. Every field is left out of the file at its default, so
/// `/mcp add` keeps writing the three-line table it always did.
///
/// ```toml
/// [[mcp]]
/// name = "github"
/// command = "github-mcp"
/// disabled_tools = ["delete_*"]
/// tool_timeout_secs = 300
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpOptions {
    /// `false` keeps the server in the file without connecting it.
    #[serde(default = "enabled_by_default", skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// When non-empty, only the server's tools matching one of these names
    /// (`*` wildcards) are offered to the model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_tools: Vec<String>,
    /// Tools never offered, even when `enabled_tools` matches them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_tools: Vec<String>,
    /// Seconds each request of the connection handshake may take (default
    /// 10); past it the server is reported and skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_secs: Option<u64>,
    /// Seconds one tool call may take (default 120); past it the model gets
    /// an error and the connection stays usable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_secs: Option<u64>,
}

impl Default for McpOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            startup_timeout_secs: None,
            tool_timeout_secs: None,
        }
    }
}

fn enabled_by_default() -> bool {
    true
}

// serde's `skip_serializing_if` hands the field by reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_true(b: &bool) -> bool {
    *b
}

impl Settings {
    /// The settings file path for a given project root.
    pub fn path(root: &Path) -> PathBuf {
        root.join(FILE_NAME)
    }

    /// The directory of the user-global settings file:
    /// `$XDG_CONFIG_HOME/atelier`, else `~/.config/atelier`. Its
    /// `[permissions]`, `[[hooks]]` and model tables (`[[provider]]`,
    /// `[models]`, `[variant.*]`, `[[price]]`, `[defaults] model`/`provider`)
    /// are used, under the project's; its `[instructions] files` are loaded
    /// as well as the project's.
    pub fn user_dir() -> Option<PathBuf> {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
        var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|base| base.join("atelier"))
    }

    /// The user-global settings: defaults when there is no such file, and
    /// with the error when it can't be read.
    pub fn load_user() -> Result<Self> {
        match Self::user_dir() {
            Some(dir) => Self::load(&dir),
            None => Ok(Self::default()),
        }
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
    fn sandbox_table_parses_and_is_left_out_when_unset() {
        let settings: Settings = tomlproc::serde::from_str(
            "[sandbox]\nenabled = true\nmemory_mb = 512\nnetwork = [\"crates.io\"]\n",
        )
        .unwrap();
        assert_eq!(settings.sandbox.enabled, Some(true));
        assert_eq!(settings.sandbox.memory_mb, Some(512));
        assert_eq!(settings.sandbox.network, ["crates.io"]);
        assert!(settings.sandbox.root.is_none());
        // It survives the rewrite `/mcp add` and friends do…
        let text = tomlproc::serde::to_string_pretty(&settings).unwrap();
        let again: Settings = tomlproc::serde::from_str(&text).unwrap();
        assert_eq!(again.sandbox, settings.sandbox);
        // …and a file without it doesn't grow an empty table.
        let empty: Settings = tomlproc::serde::from_str("").unwrap();
        assert_eq!(empty.sandbox, SandboxSettings::default());
        let text = tomlproc::serde::to_string_pretty(&empty).unwrap();
        assert!(!text.contains("sandbox"), "{text}");
    }

    #[test]
    fn search_table_parses_and_defaults_to_unset() {
        let settings: Settings = tomlproc::serde::from_str(
            "[search]\nprovider = \"brave\"\nkey_env = \"BRAVE_API_KEY\"\n",
        )
        .unwrap();
        assert_eq!(settings.search.provider.as_deref(), Some("brave"));
        assert_eq!(settings.search.key_env.as_deref(), Some("BRAVE_API_KEY"));
        assert!(settings.search.url.is_none());

        let empty: Settings = tomlproc::serde::from_str("").unwrap();
        assert!(empty.search.provider.is_none());
    }

    #[test]
    fn model_tables_parse_and_survive_a_rewrite() {
        let text = r#"
[defaults]
provider = "local"

[[provider]]
name = "local"
base_url = "http://127.0.0.1:1/v1"
api_key_env = "LOCAL_KEY"
headers = { "X-Org" = "abc" }
timeout_secs = 60

[[provider]]
name = "hosted"
base_url = "https://example.com/v1"
api_key_command = "pass show llm"

[models]
main = "local/qwen3-coder:30b#high"
small = "qwen3:0.6b"

[variant.creative]
temperature = 1.1
max_tokens = 4096
extra = { top_k = 40, stop = ["x"] }

[[price]]
model = "hosted/big"
input = 3
output = 15.5
cached = 0.3
"#;
        let check = |s: &Settings| {
            assert_eq!(s.defaults.provider.as_deref(), Some("local"));
            let [local, hosted] = s.provider.as_slice() else {
                panic!("expected two providers: {:?}", s.provider);
            };
            assert_eq!(local.api_key_env.as_deref(), Some("LOCAL_KEY"));
            assert_eq!(local.headers["X-Org"], "abc");
            assert_eq!(local.timeout_secs, Some(60));
            assert_eq!(hosted.api_key_command.as_deref(), Some("pass show llm"));
            assert_eq!(s.models.main.as_deref(), Some("local/qwen3-coder:30b#high"));
            assert!(s.models.summary.is_none());
            let creative = &s.variant["creative"];
            assert_eq!(creative.temperature, Some(1.1));
            assert_eq!(creative.max_tokens, Some(4096));
            assert_eq!(creative.extra["top_k"], 40);
            assert_eq!(creative.extra["stop"][0], "x");
            assert_eq!(s.price[0].input, 3.0);
            assert_eq!(s.price[0].cached, Some(0.3));
        };
        let settings: Settings = tomlproc::serde::from_str(text).unwrap();
        check(&settings);
        // `/mcp add` and friends rewrite the whole file.
        let rewritten = tomlproc::serde::to_string_pretty(&settings).unwrap();
        check(&tomlproc::serde::from_str(&rewritten).unwrap());
        // And a file without these tables gains none of them.
        let plain = tomlproc::serde::to_string_pretty(&Settings::default()).unwrap();
        for table in ["provider", "models", "variant", "price"] {
            assert!(!plain.contains(table), "{plain}");
        }
    }

    #[test]
    fn goal_bounds_parse_and_stay_out_of_the_file_when_unset() {
        let settings: Settings =
            tomlproc::serde::from_str("[goals]\nmax_rounds = 3\nmax_tokens = 50000\n").unwrap();
        assert_eq!(settings.goals.max_rounds, Some(3));
        assert_eq!(settings.goals.max_tokens, Some(50_000));
        let text = tomlproc::serde::to_string_pretty(&settings).unwrap();
        let again: Settings = tomlproc::serde::from_str(&text).unwrap();
        assert_eq!(again.goals, settings.goals);

        let empty = tomlproc::serde::to_string_pretty(&Settings::default()).unwrap();
        assert!(!empty.contains("goals"), "{empty}");
    }

    #[test]
    fn instruction_files_parse_and_are_left_out_when_unset() {
        let settings: Settings = tomlproc::serde::from_str(
            "[instructions]\nfiles = [\"docs/conventions.md\", \"team/*.md\"]\n",
        )
        .unwrap();
        assert_eq!(
            settings.instructions.files,
            ["docs/conventions.md", "team/*.md"]
        );
        let rewritten = tomlproc::serde::to_string_pretty(&settings).unwrap();
        let again: Settings = tomlproc::serde::from_str(&rewritten).unwrap();
        assert_eq!(again.instructions, settings.instructions);

        let plain = tomlproc::serde::to_string_pretty(&Settings::default()).unwrap();
        assert!(!plain.contains("instructions"), "{plain}");
    }

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

    #[test]
    fn mcp_options_parse_default_and_stay_out_of_a_plain_table() {
        let settings: Settings = tomlproc::serde::from_str(
            r#"
[[mcp]]
name = "fs"
command = "fs-server"

[[mcp]]
name = "gh"
command = "gh-server"
enabled = false
enabled_tools = ["list_*", "get_issue"]
disabled_tools = ["list_secrets"]
startup_timeout_secs = 3
tool_timeout_secs = 300

[[mcp_http]]
name = "web"
url = "https://example.invalid/mcp"
disabled_tools = ["*"]
"#,
        )
        .unwrap();
        let [fs, gh] = settings.mcp.as_slice() else {
            panic!("expected two stdio servers: {:?}", settings.mcp);
        };
        assert!(fs.options.enabled);
        assert!(fs.options.enabled_tools.is_empty());
        assert_eq!(fs.options.startup_timeout_secs, None);
        assert!(!gh.options.enabled);
        assert_eq!(gh.options.enabled_tools, ["list_*", "get_issue"]);
        assert_eq!(gh.options.disabled_tools, ["list_secrets"]);
        assert_eq!(gh.options.startup_timeout_secs, Some(3));
        assert_eq!(gh.options.tool_timeout_secs, Some(300));
        assert_eq!(settings.mcp_http[0].options.disabled_tools, ["*"]);

        // The rewrite `/mcp add` does: options survive it, and a server
        // without any gets no extra keys.
        let text = tomlproc::serde::to_string_pretty(&settings).unwrap();
        let again: Settings = tomlproc::serde::from_str(&text).unwrap();
        assert!(!again.mcp[1].options.enabled);
        assert_eq!(again.mcp[1].options.tool_timeout_secs, Some(300));
        assert_eq!(again.mcp_http[0].options.disabled_tools, ["*"]);
        let fs_table = text.split("[[mcp]]").nth(1).unwrap();
        assert!(!fs_table.contains("enabled"), "{text}");
        assert!(!fs_table.contains("timeout"), "{text}");
    }
}
