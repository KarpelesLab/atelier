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
    /// The model run on this machine (see [`crate::local`]). Honoured in
    /// the user-global file only: what to download and load is not a
    /// project's to choose.
    #[serde(default, skip_serializing_if = "LocalSettings::is_unset")]
    pub local: LocalSettings,
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
    /// Hosts the guest may reach, by name (`crates.io`, `*.crates.io`) or
    /// address; none when empty. The user-global list and the project's
    /// add up, the project's only once the project is trusted.
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

/// `[local]`: the model `atelier --local` runs with oxinfer.
///
/// ```toml
/// [local]
/// model = "google/gemma-4-E4B-it,quant=q8"
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LocalSettings {
    /// `SOURCE[,KEY=VALUE...]` as `oxinfer serve --model` takes it: a
    /// Hugging Face repository, a folder or a file, and oxinfer's options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl LocalSettings {
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
    /// The context window of the models served here, in tokens, for the
    /// compaction threshold to follow (see `agent::limit`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
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
    /// Whether the reviewer runs (unset: off). An `Option`, so that a
    /// project's file that does not say leaves the user-global one's say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Model for the reviewer (defaults to the main model when unset).
    #[serde(default)]
    pub model: Option<String>,
    /// Whether a `bash` command complex enough to be hard to read at a
    /// glance gets a model's plain-language summary at its approval prompt
    /// (see [`crate::preflight`]). Off by default: it is one more request,
    /// and a wait of up to a few seconds before such a prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight: Option<bool>,
}

/// `[defaults]`, of either file. Each is overridden by its env var
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
    /// `$XDG_CONFIG_HOME/atelier`, else `~/.config/atelier`. Every table of
    /// it is read, under the project's, except `[[diagnostics]]` (see
    /// [`layered`](Self::layered) for how the two combine).
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

    /// The settings in force: `project`'s laid over `user`'s (the
    /// user-global file's). Environment variables and flags are laid over
    /// the result by whoever reads each setting.
    ///
    /// `project` must already be what may be acted on — for a session,
    /// [`Trust::effective`](crate::trust::Trust::effective) of the project's
    /// file — so that whatever an untrusted project is not allowed to say
    /// leaves the user's value standing rather than taking it away. The
    /// rules, table by table (`docs/configuration.md` has them too):
    ///
    /// - **Scalar keys** (`[defaults]`, `[review]`, `[models]`, `[goals]`,
    ///   `[agents]`, `[loops]`, `[coordinator]`, `[sandbox] enabled` and
    ///   `memory_mb`) override key by key: the project's where it says,
    ///   else the user's.
    /// - **The permission mode** (`[defaults] mode`, and `approve`, its
    ///   older spelling) is one setting per file, and a project's counts
    ///   only where it is at least as careful as the user's: as with deny
    ///   rules, a project can tighten what the user chose, never loosen it.
    /// - **Lists that only add up** — `[permissions] allow`/`ask`/`deny`,
    ///   `[[hooks]]`, `[instructions] files`, `[sandbox] network` — are the
    ///   user's entries followed by the project's.
    /// - **Named entries** — `[[mcp]]` and `[[mcp_http]]` (one namespace:
    ///   a server's tools are `mcp__<name>__*` whichever kind it is),
    ///   `[[provider]]`, `[variant.*]`, `[[lsp]]` — are both files'
    ///   together, a project's entry replacing the user's of the same
    ///   name. `[[price]]` is the project's entries, then the user's (the
    ///   first entry for a model counts).
    /// - **Tables taken whole**, because their keys only mean something
    ///   together: `[search]` (a provider, its endpoint and the name of its
    ///   key — the user's key must not follow a project's choice of
    ///   provider) and `[permissions.reviewer]` (its `policy` path is
    ///   relative to the file it is in). The project's, when it has one.
    /// - **The user's file only**: `[local]` and `[sandbox] root` — what to
    ///   download and run, and what of the host the sandbox guest can read,
    ///   are not a repository's to choose.
    /// - **The project's file only**: `[[diagnostics]]`. Checkers run the
    ///   project's own build (`cargo check` runs build scripts), and a list
    ///   of them replaces detection: one list for every project would be
    ///   wrong for most of them.
    ///
    /// Some readers keep the two files apart because they need to know
    /// which file a value came from — the permission rules
    /// ([`Policy::load`](crate::permissions::Policy::load), for where
    /// "always" writes), the hooks, the instruction files (paths relative
    /// to their own file), the model tables
    /// ([`Config::apply_settings`](crate::config::Config::apply_settings))
    /// — and follow these same rules themselves; the merged tables here are
    /// for everything else, and for reporting.
    pub fn layered(user: &Settings, project: &Settings) -> Settings {
        let (u, p) = (user, project);
        let pick = |p: &Option<String>, u: &Option<String>| p.clone().or_else(|| u.clone());
        let both = |u: &[String], p: &[String]| u.iter().chain(p).cloned().collect::<Vec<_>>();

        // Servers, by name across both kinds.
        let project_servers: Vec<&str> = (p.mcp.iter().map(|m| m.name.as_str()))
            .chain(p.mcp_http.iter().map(|m| m.name.as_str()))
            .collect();
        let users = |name: &str| !project_servers.contains(&name);
        let mcp = (u.mcp.iter().filter(|m| users(&m.name)))
            .chain(&p.mcp)
            .cloned()
            .collect();
        let mcp_http = (u.mcp_http.iter().filter(|m| users(&m.name)))
            .chain(&p.mcp_http)
            .cloned()
            .collect();

        let mut provider: Vec<ProviderConfig> = Vec::new();
        for table in u.provider.iter().chain(&p.provider) {
            match provider.iter_mut().find(|t| t.name == table.name) {
                Some(slot) => *slot = table.clone(),
                None => provider.push(table.clone()),
            }
        }
        let mut variant = u.variant.clone();
        variant.extend(p.variant.clone());
        // The first entry listing an extension serves it.
        let lsp = (p.lsp.iter())
            .chain(
                u.lsp
                    .iter()
                    .filter(|l| p.lsp.iter().all(|q| q.name != l.name)),
            )
            .cloned()
            .collect();

        let (pm, um) = (&p.models, &u.models);
        Settings {
            mcp,
            mcp_http,
            permissions: Permissions {
                allow: both(&u.permissions.allow, &p.permissions.allow),
                ask: both(&u.permissions.ask, &p.permissions.ask),
                deny: both(&u.permissions.deny, &p.permissions.deny),
                reviewer: (p.permissions.reviewer.clone())
                    .or_else(|| u.permissions.reviewer.clone()),
            },
            defaults: Defaults {
                model: pick(&p.defaults.model, &u.defaults.model),
                provider: pick(&p.defaults.provider, &u.defaults.provider),
                // Folded into `mode`.
                approve: None,
                mode: layered_mode(&u.defaults, &p.defaults),
                context_limit: p.defaults.context_limit.or(u.defaults.context_limit),
                mcp_tool_budget: p.defaults.mcp_tool_budget.or(u.defaults.mcp_tool_budget),
                parallel_tools: p.defaults.parallel_tools.or(u.defaults.parallel_tools),
            },
            review: ReviewSettings {
                enabled: p.review.enabled.or(u.review.enabled),
                model: pick(&p.review.model, &u.review.model),
                preflight: p.review.preflight.or(u.review.preflight),
            },
            diagnostics: p.diagnostics.clone(),
            lsp,
            search: match p.search.is_unset() {
                true => u.search.clone(),
                false => p.search.clone(),
            },
            hooks: u.hooks.iter().chain(&p.hooks).cloned().collect(),
            provider,
            models: ModelRoles {
                main: pick(&pm.main, &um.main),
                small: pick(&pm.small, &um.small),
                subagent: pick(&pm.subagent, &um.subagent),
                review: pick(&pm.review, &um.review),
                summary: pick(&pm.summary, &um.summary),
            },
            variant,
            price: p.price.iter().chain(&u.price).cloned().collect(),
            goals: GoalSettings {
                max_rounds: p.goals.max_rounds.or(u.goals.max_rounds),
                max_tokens: p.goals.max_tokens.or(u.goals.max_tokens),
            },
            sandbox: SandboxSettings {
                enabled: p.sandbox.enabled.or(u.sandbox.enabled),
                root: u.sandbox.root.clone(),
                network: both(&u.sandbox.network, &p.sandbox.network),
                memory_mb: p.sandbox.memory_mb.or(u.sandbox.memory_mb),
            },
            instructions: InstructionSettings {
                files: both(&u.instructions.files, &p.instructions.files),
            },
            agents: AgentSettings {
                max_background: p.agents.max_background.or(u.agents.max_background),
            },
            local: u.local.clone(),
            coordinator: CoordinatorSettings {
                wake: p.coordinator.wake.or(u.coordinator.wake),
                wake_max: p.coordinator.wake_max.or(u.coordinator.wake_max),
            },
            loops: LoopSettings {
                max_runs: p.loops.max_runs.or(u.loops.max_runs),
            },
        }
    }

    /// Write settings back to `<root>/atelier.toml`.
    pub fn save(&self, root: &Path) -> Result<()> {
        let path = Self::path(root);
        let text = tomlproc::serde::to_string_pretty(self).context("serializing settings")?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
    }
}

/// The permission mode one file's `[defaults]` asks for: `mode`, else what
/// `approve` meant before there were modes (as
/// [`Mode::configured`](crate::permissions::Mode::configured) reads it).
pub fn file_mode(defaults: &Defaults) -> Option<crate::permissions::Mode> {
    use crate::permissions::Mode;
    (defaults.mode).or(defaults.approve.map(|yes| match yes {
        true => Mode::Yolo,
        false => Mode::AutoEdit,
    }))
}

/// Whether the mode `project`'s `[defaults]` asks for is laid over
/// `user`'s: it is there, and where the user's file names a mode too, it
/// is at least as careful.
pub fn project_mode_counts(user: &Defaults, project: &Defaults) -> bool {
    match (file_mode(user), file_mode(project)) {
        (Some(user), Some(project)) => project.strictness() <= user.strictness(),
        (_, project) => project.is_some(),
    }
}

/// The mode of [`Settings::layered`].
fn layered_mode(user: &Defaults, project: &Defaults) -> Option<crate::permissions::Mode> {
    match project_mode_counts(user, project) {
        true => file_mode(project),
        false => file_mode(user),
    }
}

impl SearchSettings {
    /// Nothing set: as a file without the table.
    pub fn is_unset(&self) -> bool {
        self.provider.is_none() && self.url.is_none() && self.key_env.is_none()
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
context_window = 32768

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
            assert_eq!(local.context_window, Some(32768));
            assert_eq!(hosted.context_window, None);
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

    fn parse(text: &str) -> Settings {
        tomlproc::serde::from_str(text).unwrap()
    }

    fn names<'a>(items: impl IntoIterator<Item = &'a String>) -> Vec<&'a str> {
        items.into_iter().map(String::as_str).collect()
    }

    #[test]
    fn layered_scalars_override_key_by_key_not_table_by_table() {
        let user = parse(
            r#"
[defaults]
model = "user-model"
provider = "home"
context_limit = 9000
mcp_tool_budget = 100
parallel_tools = false

[review]
enabled = true
model = "user-judge"

[goals]
max_rounds = 3
max_tokens = 1000

[agents]
max_background = 2

[coordinator]
wake = "questions"
wake_max = 7

[loops]
max_runs = 9

[models]
small = "user-small"
summary = "user-summary"

[sandbox]
enabled = true
memory_mb = 512
"#,
        );
        let project = parse(
            r#"
[defaults]
context_limit = 4000

[review]
preflight = true

[goals]
max_tokens = 50

[coordinator]
wake_max = 1

[models]
small = "project-small"

[sandbox]
memory_mb = 2048
"#,
        );
        let s = Settings::layered(&user, &project);
        // The project's where it says…
        assert_eq!(s.defaults.context_limit, Some(4000));
        assert_eq!(s.review.preflight, Some(true));
        assert_eq!(s.goals.max_tokens, Some(50));
        assert_eq!(s.coordinator.wake_max, Some(1));
        assert_eq!(s.models.small.as_deref(), Some("project-small"));
        assert_eq!(s.sandbox.memory_mb, Some(2048));
        // …the user's for the other keys of the same tables.
        assert_eq!(s.defaults.model.as_deref(), Some("user-model"));
        assert_eq!(s.defaults.provider.as_deref(), Some("home"));
        assert_eq!(s.defaults.mcp_tool_budget, Some(100));
        assert_eq!(s.defaults.parallel_tools, Some(false));
        assert_eq!(s.review.enabled, Some(true));
        assert_eq!(s.review.model.as_deref(), Some("user-judge"));
        assert_eq!(s.goals.max_rounds, Some(3));
        assert_eq!(s.agents.max_background, Some(2));
        assert_eq!(s.coordinator.wake, Some(crate::coord::Wake::Questions));
        assert_eq!(s.loops.max_runs, Some(9));
        assert_eq!(s.models.summary.as_deref(), Some("user-summary"));
        assert_eq!(s.sandbox.enabled, Some(true));

        // A project's `false` is a say of its own, not "unset".
        let off = parse("[review]\nenabled = false\n[sandbox]\nenabled = false\n");
        let s = Settings::layered(&user, &off);
        assert_eq!(s.review.enabled, Some(false));
        assert_eq!(s.sandbox.enabled, Some(false));

        // Neither file: nothing, for the built-in default to fill.
        let none = Settings::layered(&Settings::default(), &Settings::default());
        assert_eq!(none.review.enabled, None);
        assert_eq!(none.defaults.mode, None);
        assert_eq!(none.defaults.context_limit, None);
    }

    #[test]
    fn a_review_table_without_enabled_says_nothing_about_it() {
        // Written back by `/config` before `enabled` could be unset, the
        // key is there; written now, a table that never set it stays so.
        let mut settings = parse("[review]\nmodel = \"m\"\n");
        assert_eq!(settings.review.enabled, None);
        let text = tomlproc::serde::to_string_pretty(&settings).unwrap();
        assert!(!text.contains("enabled"), "{text}");
        assert!(!text.contains("preflight"), "{text}");
        settings.review.enabled = Some(false);
        let text = tomlproc::serde::to_string_pretty(&settings).unwrap();
        assert_eq!(parse(&text).review.enabled, Some(false));
    }

    #[test]
    fn layered_lists_that_only_add_are_the_users_then_the_projects() {
        let user = parse(
            r#"
[permissions]
allow = ["read"]
ask = ["bash(git push *)"]
deny = ["bash(rm -rf *)"]

[instructions]
files = ["~/notes.md"]

[sandbox]
network = ["crates.io"]

[[hooks]]
event = "turn_end"
command = "user-hook"
"#,
        );
        let project = parse(
            r#"
[permissions]
allow = ["bash(cargo *)"]
deny = ["web_fetch"]

[instructions]
files = ["docs/style.md"]

[sandbox]
network = ["pypi.org"]

[[hooks]]
event = "turn_end"
command = "project-hook"
"#,
        );
        let s = Settings::layered(&user, &project);
        assert_eq!(names(&s.permissions.allow), ["read", "bash(cargo *)"]);
        assert_eq!(names(&s.permissions.ask), ["bash(git push *)"]);
        assert_eq!(names(&s.permissions.deny), ["bash(rm -rf *)", "web_fetch"]);
        assert_eq!(
            names(&s.instructions.files),
            ["~/notes.md", "docs/style.md"]
        );
        assert_eq!(names(&s.sandbox.network), ["crates.io", "pypi.org"]);
        let hooks: Vec<&str> = s.hooks.iter().map(|h| h.command.as_str()).collect();
        assert_eq!(hooks, ["user-hook", "project-hook"]);

        // A project with no rules of its own takes none of the user's away.
        let s = Settings::layered(&user, &Settings::default());
        assert_eq!(names(&s.permissions.deny), ["bash(rm -rf *)"]);
        assert_eq!(names(&s.permissions.allow), ["read"]);
    }

    #[test]
    fn layered_named_entries_are_both_files_with_the_project_replacing_by_name() {
        let user = parse(
            r#"
[[mcp]]
name = "github"
command = "user-github"

[[mcp]]
name = "notes"
command = "user-notes"

[[mcp_http]]
name = "search"
url = "https://user.invalid/mcp"

[[provider]]
name = "home"
base_url = "http://user.invalid/v1"

[[provider]]
name = "shared"
base_url = "http://user-shared.invalid/v1"

[variant.low]
reasoning_effort = "low"

[variant.high]
reasoning_effort = "high"

[[lsp]]
name = "pyright"
command = "pyright-langserver --stdio"
extensions = ["py"]

[[lsp]]
name = "clangd"
command = "clangd"
extensions = ["c", "h"]

[[price]]
model = "m"
input = 1.0
output = 2.0
"#,
        );
        let project = parse(
            r#"
[[mcp]]
name = "search"
command = "project-search"

[[mcp_http]]
name = "github"
url = "https://project.invalid/mcp"

[[provider]]
name = "shared"
base_url = "http://project-shared.invalid/v1"

[variant.high]
reasoning_effort = "max"

[[lsp]]
name = "pylsp"
command = "pylsp"
extensions = ["py"]

[[lsp]]
name = "clangd"
command = "clangd --project"
extensions = ["c"]

[[price]]
model = "m"
input = 5.0
output = 6.0
"#,
        );
        let s = Settings::layered(&user, &project);
        // One namespace across both kinds: the project's HTTP `github`
        // replaces the user's stdio one, its stdio `search` the user's
        // HTTP one. The user's first, then the project's.
        let stdio: Vec<(&str, &str)> = (s.mcp.iter())
            .map(|m| (m.name.as_str(), m.command.as_str()))
            .collect();
        assert_eq!(
            stdio,
            [("notes", "user-notes"), ("search", "project-search")]
        );
        let http: Vec<(&str, &str)> = (s.mcp_http.iter())
            .map(|m| (m.name.as_str(), m.url.as_str()))
            .collect();
        assert_eq!(http, [("github", "https://project.invalid/mcp")]);

        let providers: Vec<(&str, &str)> = (s.provider.iter())
            .map(|p| (p.name.as_str(), p.base_url.as_str()))
            .collect();
        assert_eq!(
            providers,
            [
                ("home", "http://user.invalid/v1"),
                ("shared", "http://project-shared.invalid/v1")
            ]
        );
        assert_eq!(s.variant["low"].reasoning_effort.as_deref(), Some("low"));
        assert_eq!(s.variant["high"].reasoning_effort.as_deref(), Some("max"));

        // The project's servers first — the first listing an extension
        // serves it — and the user's `clangd` gives way to the project's.
        let lsp: Vec<(&str, &str)> = (s.lsp.iter())
            .map(|l| (l.name.as_str(), l.command.as_str()))
            .collect();
        assert_eq!(
            lsp,
            [
                ("pylsp", "pylsp"),
                ("clangd", "clangd --project"),
                ("pyright", "pyright-langserver --stdio")
            ]
        );
        let py = s.lsp.iter().find(|l| l.serves("py")).unwrap();
        assert_eq!(py.name, "pylsp");
        assert!(s.lsp.iter().all(|l| !l.serves("h") || l.name != "clangd"));

        // The project's price first: the first for a model counts.
        let prices: Vec<f64> = s.price.iter().map(|p| p.input).collect();
        assert_eq!(prices, [5.0, 1.0]);
    }

    #[test]
    fn layered_search_and_reviewer_tables_are_taken_whole() {
        let user = parse(
            r#"
[search]
provider = "tavily"
key_env = "TAVILY_KEY"

[permissions.reviewer]
enabled = true
model = "judge"
policy = "policy.md"
"#,
        );
        // A provider of the project's own: the user's key does not follow.
        let project = parse("[search]\nprovider = \"brave\"\n");
        let s = Settings::layered(&user, &project);
        assert_eq!(s.search.provider.as_deref(), Some("brave"));
        assert_eq!(s.search.key_env, None);
        // No table of the project's: the user's, whole.
        let s = Settings::layered(&user, &Settings::default());
        assert_eq!(s.search.provider.as_deref(), Some("tavily"));
        assert_eq!(s.search.key_env.as_deref(), Some("TAVILY_KEY"));
        let reviewer = s.permissions.reviewer.unwrap();
        assert_eq!(reviewer.policy.as_deref(), Some("policy.md"));

        // The project's reviewer table, without a policy of its own, does
        // not borrow the user's (a path relative to the user's file).
        let project = parse("[permissions.reviewer]\nenabled = true\nmodel = \"other\"\n");
        let s = Settings::layered(&user, &project);
        let reviewer = s.permissions.reviewer.unwrap();
        assert_eq!(reviewer.model.as_deref(), Some("other"));
        assert_eq!(reviewer.policy, None);
    }

    #[test]
    fn layered_keeps_user_only_and_project_only_tables_where_they_belong() {
        let user = parse(
            r#"
[local]
model = "user/weights"

[sandbox]
root = "/srv/rootfs"

[[diagnostics]]
name = "everywhere"
command = "make lint"
"#,
        );
        let project = parse(
            r#"
[local]
model = "evil/weights"

[sandbox]
root = "/"

[[diagnostics]]
name = "clippy"
command = "cargo clippy"
"#,
        );
        let s = Settings::layered(&user, &project);
        assert_eq!(s.local.model.as_deref(), Some("user/weights"));
        assert_eq!(s.sandbox.root.as_deref(), Some("/srv/rootfs"));
        let checks: Vec<&str> = s.diagnostics.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(checks, ["clippy"]);
        // The user's checkers never apply: none of the project's means
        // "detect them", not "use the user's".
        let s = Settings::layered(&user, &Settings::default());
        assert!(s.diagnostics.is_empty());
        assert_eq!(s.local.model.as_deref(), Some("user/weights"));
    }

    #[test]
    fn layered_mode_lets_a_project_tighten_the_users_never_loosen_it() {
        use crate::permissions::Mode;
        let defaults = |text: &str| parse(&format!("[defaults]\n{text}\n"));
        let mode = |user: &str, project: &str| {
            Settings::layered(&defaults(user), &defaults(project))
                .defaults
                .mode
        };
        // Only one file says: that one.
        assert_eq!(mode("", ""), None);
        assert_eq!(mode("mode = \"plan\"", ""), Some(Mode::Plan));
        assert_eq!(mode("", "mode = \"ask-edits\""), Some(Mode::AskEdits));
        assert_eq!(mode("", "mode = \"yolo\""), Some(Mode::Yolo));
        // Both: the project's when at least as careful…
        assert_eq!(
            mode("mode = \"auto-edit\"", "mode = \"plan\""),
            Some(Mode::Plan)
        );
        assert_eq!(
            mode("mode = \"yolo\"", "mode = \"auto-edit\""),
            Some(Mode::AutoEdit)
        );
        assert_eq!(mode("mode = \"plan\"", "mode = \"plan\""), Some(Mode::Plan));
        // …else the user's.
        assert_eq!(
            mode("mode = \"plan\"", "mode = \"auto-edit\""),
            Some(Mode::Plan)
        );
        assert_eq!(
            mode("mode = \"ask-edits\"", "mode = \"yolo\""),
            Some(Mode::AskEdits)
        );
        // `approve` is the older spelling, on either side.
        assert_eq!(mode("approve = true", ""), Some(Mode::Yolo));
        assert_eq!(mode("approve = true", "mode = \"plan\""), Some(Mode::Plan));
        assert_eq!(mode("mode = \"plan\"", "approve = true"), Some(Mode::Plan));
        assert_eq!(
            mode("approve = false", "approve = true"),
            Some(Mode::AutoEdit)
        );
        assert_eq!(
            mode("mode = \"yolo\"", "approve = false"),
            Some(Mode::AutoEdit)
        );
        // Within a file, `mode` wins over `approve`, as it always has.
        assert_eq!(
            mode("", "mode = \"plan\"\napprove = true"),
            Some(Mode::Plan)
        );
        // The merged table says it with `mode` alone, so that
        // `Mode::configured` reads it as it is meant.
        let s = Settings::layered(&defaults("approve = true"), &Settings::default());
        assert_eq!(s.defaults.approve, None);
        assert_eq!(s.defaults.mode, Some(Mode::Yolo));
    }
}
