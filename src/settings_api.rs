//! Settings for a client of `atelier serve`: `settings/get` and
//! `settings/set` (see `docs/protocol.md`), and the table of settings both
//! are built on — which is also where the `/config` screen ([`crate::tui`])
//! takes its rows from.
//!
//! # Reading
//!
//! [`get`] answers with one entry per setting of [`SPECS`]: what it is (key
//! path, type, allowed values, a line of description, whether and where
//! `settings/set` can write it), its value in force, and its *layers* — every
//! place that says something about it, the one that wins first:
//!
//! 1. `session` / `flag`: the running session differs from what the files
//!    and the environment yield (`/model`, `/mode`, `--mode`, …);
//! 2. `env`: the environment variable that overrides it;
//! 3. `project`: the project's `atelier.toml`;
//! 4. `user`: the user-global `atelier.toml`;
//! 5. `default`: the built-in value.
//!
//! A layer that is there but does not count carries `ignored` with the
//! reason: a project value withheld because the project is not trusted (see
//! [`crate::trust`]), a value in a file the setting is not read from.
//!
//! The files are read as they are on disk now, as TOML, so a layer exists
//! exactly when the file has the key. What the session does with them was
//! decided when it started (and by `/trust` since); where the two can
//! differ is listed in `docs/protocol.md`.
//!
//! No secret leaves through here. `atelier.toml` names keys rather than
//! holding them, and the places where one can sit anyway — header values, a
//! key command — are reduced to names by [`redact`]; the keys the
//! environment holds are reported as set or not.
//!
//! # Writing
//!
//! [`set`] changes what the `/config` screen changes, with the screen's own
//! means: the session's setters and [`Session::save_settings`] for the
//! project's file, [`Settings::save`] for the user-global one. Both rewrite
//! the file whole from the parsed settings, so comments in it are lost — as
//! they are when `/config` or `/mcp add` rewrite the project's.
//!
//! Only the screen's rows can be set, and of those not `defaults.approve`:
//! at project scope it is a gated setting (it would have the file switch
//! every approval prompt off), and the user-global file is not read for it.
//! Nothing that is gated by trust, or honoured in the user-global file only,
//! can be written to the project's file from here.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{Map, Value, json};

use crate::agent::Session;
use crate::permissions::Mode;
use crate::rpc::{self, Failure};
use crate::settings::Settings;

/// `--mode` as given on the command line, if it was: what tells a mode
/// chosen by flag from one chosen during the session.
static FLAG_MODE: OnceLock<Mode> = OnceLock::new();

/// Note that `--mode` chose the session's mode (called once, by `main`).
pub fn mode_from_flag(mode: Mode) {
    let _ = FLAG_MODE.set(mode);
}

/// What a setting holds.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bool,
    /// A positive whole number.
    Integer,
    Text,
    /// One of these.
    Choice(&'static [&'static str]),
    /// An array of strings.
    List,
    /// An array of tables (`[[name]]`).
    Tables,
    /// A table of tables (`[name.<key>]`), or one table taken whole.
    Table,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Bool => "boolean",
            Kind::Integer => "integer",
            Kind::Text | Kind::Choice(_) => "string",
            Kind::List => "list",
            Kind::Tables => "tables",
            Kind::Table => "table",
        }
    }
}

/// Which of the two files a setting is read from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Files {
    /// The project's only.
    Project,
    /// The user-global one only: not a repository's to choose.
    User,
    /// Both, the project's winning (or, for a [`Spec::combined`] setting,
    /// both together).
    Both,
}

/// A file `settings/set` can write.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    User,
    Project,
}

impl Scope {
    fn name(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
        }
    }
}

/// The value a setting has when nothing sets it.
#[derive(Clone, Copy)]
enum Builtin {
    /// None worth stating (unset means "off", "none", or depends).
    Unset,
    Bool(bool),
    Int(u64),
    Text(&'static str),
}

/// One setting: a key of `atelier.toml`, and what there is to know about it
/// without reading the code that uses it.
pub struct Spec {
    /// The path in `atelier.toml`: `table.key`, or the name of a table.
    pub key: &'static str,
    pub kind: Kind,
    /// One line on what it is.
    pub about: &'static str,
    /// The row's label on the `/config` screen, for the settings it shows.
    pub label: Option<&'static str>,
    files: Files,
    /// Every file's entries apply together rather than the first one's.
    combined: bool,
    /// The environment variable that overrides it.
    env: Option<&'static str>,
    default: Builtin,
    /// The project's value counts only in a trusted project, whatever it
    /// is (beyond what [`crate::trust::Trust::effective`] takes out).
    needs_trust: bool,
    /// Where `settings/set` may write it; empty for a read-only setting.
    scopes: &'static [Scope],
}

const fn spec(key: &'static str, kind: Kind, about: &'static str) -> Spec {
    Spec {
        key,
        kind,
        about,
        label: None,
        files: Files::Project,
        combined: false,
        env: None,
        default: Builtin::Unset,
        needs_trust: false,
        scopes: &[],
    }
}

impl Spec {
    const fn user_only(mut self) -> Self {
        self.files = Files::User;
        self
    }
    const fn both(mut self) -> Self {
        self.files = Files::Both;
        self
    }
    const fn combined(mut self) -> Self {
        self.combined = true;
        self
    }
    const fn env(mut self, name: &'static str) -> Self {
        self.env = Some(name);
        self
    }
    const fn default(mut self, value: Builtin) -> Self {
        self.default = value;
        self
    }
    const fn needs_trust(mut self) -> Self {
        self.needs_trust = true;
        self
    }
    /// A row of the `/config` screen, which `settings/set` may write to
    /// `scopes`.
    const fn screen(mut self, label: &'static str, scopes: &'static [Scope]) -> Self {
        self.label = Some(label);
        self.scopes = scopes;
        self
    }
}

const MODES: &[&str] = &["plan", "ask-edits", "auto-edit", "yolo"];
const WAKE: &[&str] = &["never", "questions", "always"];
const SEARCH: &[&str] = &["searxng", "brave", "tavily", "duckduckgo"];

/// Every setting `settings/get` reports, in the order of
/// `docs/configuration.md`. The four with a `screen` are the `/config`
/// screen's rows, in the screen's order.
pub const SPECS: &[Spec] = &[
    spec(
        "defaults.model",
        Kind::Text,
        "The main model, as `provider/model#variant` or any part of that; `[models] main` wins over it",
    )
    .both()
    .env("ATELIER_MODEL")
    .default(Builtin::Text(crate::config::DEFAULT_MODEL))
    .screen("model", &[Scope::User, Scope::Project]),
    spec(
        "defaults.approve",
        Kind::Bool,
        "The older spelling of `mode = \"yolo\"`: no approval prompts. Honoured in a trusted \
         project only, so not settable here: switch the session with `/mode`",
    )
    .env("ATELIER_APPROVE")
    .default(Builtin::Bool(false))
    .screen("auto-approve", &[]),
    spec(
        "defaults.context_limit",
        Kind::Integer,
        "Tokens in a request past which older history is compacted into a summary",
    )
    .env("ATELIER_CONTEXT_LIMIT")
    .default(Builtin::Int(crate::agent::DEFAULT_CONTEXT_LIMIT as u64))
    .screen("context limit", &[Scope::Project]),
    spec(
        "review.enabled",
        Kind::Bool,
        "Whether the parallel reviewer watches the exchange and comments on it",
    )
    .env("ATELIER_REVIEW")
    .default(Builtin::Bool(false))
    .screen("review mode", &[Scope::Project]),
    spec(
        "defaults.mode",
        Kind::Choice(MODES),
        "The permission mode sessions start in; `yolo` is honoured in a trusted project only",
    )
    .env("ATELIER_MODE")
    .default(Builtin::Text("auto-edit")),
    spec(
        "defaults.provider",
        Kind::Text,
        "The provider a model named without one is on",
    )
    .both()
    .default(Builtin::Text("env")),
    spec(
        "defaults.mcp_tool_budget",
        Kind::Integer,
        "Bytes of MCP tool specs advertised before the tools are deferred behind `mcp_search`; 0 never defers",
    )
    .default(Builtin::Int(8192)),
    spec(
        "defaults.parallel_tools",
        Kind::Bool,
        "Whether the read-only tool calls of one model step run at the same time",
    )
    .env("ATELIER_PARALLEL_TOOLS")
    .default(Builtin::Bool(true)),
    spec("models.main", Kind::Text, "The model of the conversation")
        .both()
        .env("ATELIER_MODEL"),
    spec(
        "models.small",
        Kind::Text,
        "The model for cheap upkeep work; unset, the main model",
    )
    .both(),
    spec(
        "models.subagent",
        Kind::Text,
        "The model of subagents; unset, the main model",
    )
    .both(),
    spec(
        "models.review",
        Kind::Text,
        "The model of the parallel reviewer; unset, `[review] model`, then the main model",
    )
    .both(),
    spec(
        "models.summary",
        Kind::Text,
        "The model that writes compaction summaries; unset, the small model",
    )
    .both(),
    spec(
        "review.model",
        Kind::Text,
        "The older spelling of `[models] review`",
    ),
    spec(
        "provider",
        Kind::Tables,
        "Named model endpoints; a project's entry replaces a user-global one of the same name",
    )
    .both()
    .combined(),
    spec(
        "variant",
        Kind::Table,
        "Request options a model is addressed with as `model#name`",
    )
    .both()
    .combined(),
    spec(
        "price",
        Kind::Tables,
        "Token prices per model, for the cost column of `/usage`",
    )
    .both()
    .combined(),
    spec(
        "permissions.allow",
        Kind::List,
        "Tool calls that run without asking; a project's count once it is trusted",
    )
    .both()
    .combined(),
    spec(
        "permissions.ask",
        Kind::List,
        "Tool calls that ask even where they would run unasked",
    )
    .both()
    .combined(),
    spec(
        "permissions.deny",
        Kind::List,
        "Tool calls that are refused",
    )
    .both()
    .combined(),
    spec(
        "permissions.reviewer",
        Kind::Table,
        "A model that answers approval prompts; a project's table replaces the user-global one, \
         and can switch it on only once the project is trusted",
    )
    .both(),
    spec(
        "mcp",
        Kind::Tables,
        "MCP servers started with the session (stdio)",
    ),
    spec(
        "mcp_http",
        Kind::Tables,
        "MCP servers contacted with the session (Streamable HTTP); header values are not reported",
    ),
    spec(
        "hooks",
        Kind::Tables,
        "Commands run on lifecycle events",
    )
    .both()
    .combined(),
    spec(
        "instructions.files",
        Kind::List,
        "Instruction files loaded besides the `AGENTS.md` found by name",
    )
    .both()
    .combined(),
    spec(
        "diagnostics",
        Kind::Tables,
        "Lint and typecheck commands whose output the model is shown each turn; none means auto-detect",
    ),
    spec(
        "lsp",
        Kind::Tables,
        "Language servers for the `symbols` tool, by file extension",
    ),
    spec(
        "search.provider",
        Kind::Choice(SEARCH),
        "The provider the `web_search` tool queries",
    )
    .env("ATELIER_SEARCH_PROVIDER")
    .default(Builtin::Text("duckduckgo")),
    spec(
        "search.url",
        Kind::Text,
        "The SearXNG instance, or another endpoint for the provider",
    )
    .env("ATELIER_SEARCH_URL"),
    spec(
        "search.key_env",
        Kind::Text,
        "The name of the environment variable holding the search API key (never the key)",
    ),
    spec(
        "agents.max_background",
        Kind::Integer,
        "Background subagents that may run at once",
    )
    .default(Builtin::Int(4)),
    spec(
        "goals.max_rounds",
        Kind::Integer,
        "Times the agent is sent back to work on one prompt in pursuit of a `/goal`",
    )
    .default(Builtin::Int(10)),
    spec(
        "goals.max_tokens",
        Kind::Integer,
        "Output tokens one prompt may generate while the goal is unmet; unset, no limit",
    ),
    spec(
        "loops.max_runs",
        Kind::Integer,
        "Times one `/loop` fires before it stops by itself",
    )
    .needs_trust()
    .default(Builtin::Int(50)),
    spec(
        "coordinator.wake",
        Kind::Choice(WAKE),
        "What from another instance starts a turn in this one while it is idle",
    )
    .needs_trust()
    .env("ATELIER_COORDINATOR_WAKE")
    .default(Builtin::Text("never")),
    spec(
        "coordinator.wake_max",
        Kind::Integer,
        "Turns the instance may start by itself before its user types again",
    )
    .needs_trust()
    .default(Builtin::Int(5)),
    spec(
        "sandbox.enabled",
        Kind::Bool,
        "Whether `bash` commands run in the sandbox; a project's say counts once it is trusted",
    )
    .both()
    .env("ATELIER_SANDBOX")
    .default(Builtin::Bool(false)),
    spec(
        "sandbox.root",
        Kind::Text,
        "The sandbox guest's root filesystem",
    )
    .user_only()
    .env("ATELIER_SANDBOX_ROOT"),
    spec(
        "sandbox.memory_mb",
        Kind::Integer,
        "The sandbox guest's RAM ceiling in MiB",
    )
    .both()
    .default(Builtin::Int(4096)),
    spec(
        "sandbox.network",
        Kind::List,
        "Reserved: hosts the sandbox guest may reach (not implemented, the guest has no network)",
    )
    .both()
    .combined(),
    spec(
        "local.model",
        Kind::Text,
        "The model `atelier --local` runs on this machine, as `SOURCE[,KEY=VALUE...]`",
    )
    .user_only()
    .env(crate::local::ENV_MODEL),
];

/// The setting at `key`.
pub fn find(key: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.key == key)
}

/// The rows of the `/config` screen, in its order.
#[cfg(test)]
pub fn screen() -> impl Iterator<Item = &'static Spec> {
    SPECS.iter().filter(|s| s.label.is_some())
}

/// One `atelier.toml`, as it is on disk now.
struct File {
    path: Option<PathBuf>,
    exists: bool,
    /// Why nothing of it is in force: it can't be read, or is not TOML, or
    /// not the settings' shape.
    error: Option<String>,
    /// The file as TOML says it, for which keys it has; `Null` when there
    /// is no file or it is faulty.
    raw: Value,
    /// The same as settings (defaults, when faulty: what a session gets).
    typed: Settings,
}

impl File {
    fn read(path: Option<PathBuf>) -> File {
        let mut file = File {
            exists: path.as_deref().is_some_and(Path::exists),
            path,
            error: None,
            raw: Value::Null,
            typed: Settings::default(),
        };
        let Some(path) = file.path.as_deref().filter(|_| file.exists) else {
            return file;
        };
        let parsed = std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|text| {
                let typed: Settings =
                    tomlproc::serde::from_str(&text).map_err(|e| e.to_string())?;
                let raw: Value = tomlproc::serde::from_str(&text).map_err(|e| e.to_string())?;
                Ok((typed, raw))
            });
        match parsed {
            Ok((typed, raw)) => (file.typed, file.raw) = (typed, raw),
            Err(e) => file.error = Some(e),
        }
        file
    }

    fn describe(&self) -> Value {
        let mut out = json!({
            "path": self.path.as_ref().map(|p| p.display().to_string()),
            "exists": self.exists,
        });
        if let Some(error) = &self.error {
            out["error"] = Value::from(error.as_str());
        }
        out
    }
}

/// One place that says something about a setting.
#[derive(Clone)]
struct Layer {
    /// `session`, `flag`, `env`, `project`, `user` or `default`.
    source: &'static str,
    value: Value,
    /// The key it is under, when that is another one than the setting's
    /// (`models.main` deciding `defaults.model`).
    key: Option<&'static str>,
    /// The environment variable or the flag.
    name: Option<&'static str>,
    /// Why it does not count.
    ignored: Option<String>,
}

impl Layer {
    fn new(source: &'static str, value: Value) -> Layer {
        Layer {
            source,
            value,
            key: None,
            name: None,
            ignored: None,
        }
    }

    fn json(&self) -> Value {
        let mut out = json!({ "source": self.source, "value": self.value });
        if let Some(key) = self.key {
            out["key"] = Value::from(key);
        }
        if let Some(name) = self.name {
            out["name"] = Value::from(name);
        }
        if let Some(why) = &self.ignored {
            out["ignored"] = Value::from(why.as_str());
        }
        out
    }
}

/// The value at `key` (a dotted path) in `doc`, if it has one.
fn lookup<'a>(doc: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.')
        .try_fold(doc, |at, part| at.get(part))
        .filter(|v| !v.is_null())
}

/// Put `value` at `key` in `doc`, or take the key out for `Null`.
fn put(doc: &mut Value, key: &str, value: Value) {
    let (tables, leaf) = match key.rsplit_once('.') {
        Some((tables, leaf)) => (Some(tables), leaf),
        None => (None, key),
    };
    let mut at = doc;
    for part in tables.into_iter().flat_map(|t| t.split('.')) {
        if !at[part].is_object() {
            at[part] = Value::Object(Map::new());
        }
        at = &mut at[part];
    }
    match (value, at.as_object_mut()) {
        (Value::Null, Some(table)) => {
            table.remove(leaf);
        }
        (value, _) => at[leaf] = value,
    }
}

/// A file's value as it may be shown: the places a key can sit in are
/// reduced to the fact that something is there. `atelier.toml` otherwise
/// names keys (`api_key_env`, `key_env`) rather than holding them.
fn redact(key: &str, value: &Value) -> Value {
    let mut value = value.clone();
    let tables = value.as_array_mut().into_iter().flatten();
    match key {
        // `"Name: Value"` strings: the name stays.
        "mcp_http" => {
            for table in tables {
                if let Some(headers) = table.get_mut("headers").and_then(Value::as_array_mut) {
                    for header in headers {
                        let name = header.as_str().and_then(|h| h.split(':').next());
                        *header = Value::from(name.unwrap_or_default().trim());
                    }
                }
            }
        }
        // A table of headers becomes their names; the key command (which
        // may spell the key out) becomes the fact that there is one.
        "provider" => {
            for table in tables {
                if let Some(headers) = table.get_mut("headers") {
                    let names = headers
                        .as_object()
                        .map(|h| h.keys().cloned().map(Value::from).collect());
                    *headers = Value::Array(names.unwrap_or_default());
                }
                if let Some(command) = table.get_mut("api_key_command") {
                    *command = Value::Bool(true);
                }
            }
        }
        _ => {}
    }
    value
}

/// What the environment variable `name`, set to `raw`, means for the
/// setting it overrides — read the way the code that honours it does.
/// `None`: a value it does not understand.
fn env_value(name: &str, raw: &str) -> Option<Value> {
    let truthy = match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    };
    match name {
        "ATELIER_APPROVE" => Some(json!(matches!(raw, "all" | "yes" | "1"))),
        "ATELIER_REVIEW" => Some(json!(matches!(raw, "on" | "yes" | "1" | "true"))),
        "ATELIER_PARALLEL_TOOLS" => Some(json!(!matches!(raw, "0" | "off" | "no" | "false"))),
        "ATELIER_SANDBOX" => truthy.map(Value::Bool),
        "ATELIER_CONTEXT_LIMIT" => raw.parse::<u32>().ok().filter(|&n| n > 0).map(Value::from),
        "ATELIER_MODE" => Mode::parse(raw).ok().map(|m| Value::from(m.name())),
        // A typo there is read as `never`, the careful reading.
        "ATELIER_COORDINATOR_WAKE" => {
            Some(Value::from(if WAKE.contains(&raw) { raw } else { "never" }))
        }
        _ => Some(Value::from(raw)),
    }
}

/// The settings of a session and of the two files, at one moment.
struct View<'a> {
    session: &'a Session,
    project: File,
    user: File,
    /// The project's file as settings, and as the session may act on it
    /// (see [`crate::trust::Trust::effective`]), both as JSON to compare a
    /// key in.
    written: Value,
    effective: Value,
    effective_settings: Settings,
}

impl<'a> View<'a> {
    fn new(session: &'a Session, root: &Path) -> View<'a> {
        let project = File::read(Some(Settings::path(root)));
        let user = File::read(Settings::user_dir().map(|dir| Settings::path(&dir)));
        let effective_settings = session.trust().effective(&project.typed);
        View {
            session,
            written: serde_json::to_value(&project.typed).unwrap_or_default(),
            effective: serde_json::to_value(&effective_settings).unwrap_or_default(),
            effective_settings,
            project,
            user,
        }
    }

    /// What the two files say about `spec`, the project's first.
    fn file_layers(&self, spec: &'static Spec, foreign: bool, out: &mut Vec<Layer>) {
        let trust = self.session.trust();
        let layer = |source, value: &Value, ignored: Option<String>| Layer {
            key: foreign.then_some(spec.key),
            ignored,
            ..Layer::new(source, redact(spec.key, value))
        };
        if let Some(value) = lookup(&self.project.raw, spec.key) {
            let withheld = !trust.active()
                && (spec.needs_trust
                    || lookup(&self.written, spec.key) != lookup(&self.effective, spec.key));
            let ignored = if spec.files == Files::User {
                Some("read from the user-global file only".to_string())
            } else if withheld {
                Some(format!(
                    "the project is not trusted ({}): accept its settings with /trust",
                    trust.state().name()
                ))
            } else {
                None
            };
            out.push(layer("project", value, ignored));
        }
        if let Some(value) = lookup(&self.user.raw, spec.key) {
            let ignored = (spec.files == Files::Project)
                .then(|| "read from the project's file only".to_string());
            out.push(layer("user", value, ignored));
        }
    }

    /// Everything that says something about `spec` short of the running
    /// session, the strongest first.
    fn configured(&self, spec: &'static Spec) -> Vec<Layer> {
        let mut layers = Vec::new();
        let set = |name: &str| match std::env::var(name) {
            // Set to nothing, it still keeps the files' model out: the
            // built-in one is used (see `Config::from_env`).
            Ok(v) if v.is_empty() && name == "ATELIER_MODEL" => {
                Some(crate::config::DEFAULT_MODEL.to_string())
            }
            v => v.ok().filter(|v| !v.is_empty()),
        };
        if let Some(name) = spec.env
            && let Some(raw) = set(name)
        {
            let value = env_value(name, &raw);
            layers.push(Layer {
                name: Some(name),
                ignored: (value.is_none()).then(|| format!("not a value {name} understands")),
                ..Layer::new("env", value.unwrap_or(Value::from(raw)))
            });
        }
        // The role is the newer spelling of the default model, and wins.
        if spec.key == "defaults.model"
            && let Some(role) = find("models.main")
        {
            self.file_layers(role, true, &mut layers);
        }
        self.file_layers(spec, false, &mut layers);
        match spec.default {
            Builtin::Unset => {}
            Builtin::Bool(b) => layers.push(Layer::new("default", Value::Bool(b))),
            Builtin::Int(n) => layers.push(Layer::new("default", Value::from(n))),
            Builtin::Text(s) => layers.push(Layer::new("default", Value::from(s))),
        }
        layers
    }

    /// The value the running session has for `key`, where it holds one of
    /// its own that a command can change.
    fn live(&self, key: &str) -> Option<Value> {
        let session = self.session;
        match key {
            // claude picks its own model: `config().model` is a label there.
            "defaults.model" if !session.is_claude() => Some(Value::from(&*session.config().model)),
            "defaults.mode" => Some(Value::from(session.mode().name())),
            "defaults.approve" => Some(Value::Bool(session.mode() == Mode::Yolo)),
            "defaults.context_limit" => Some(Value::from(session.context_limit())),
            "review.enabled" => Some(Value::Bool(session.review_enabled())),
            _ => None,
        }
    }

    /// Where the session's permission mode comes from, when that is not
    /// what `[defaults] mode` and `ATELIER_MODE` yield.
    fn mode_layer(&self) -> Layer {
        let mode = self.session.mode();
        let value = Value::from(mode.name());
        if FLAG_MODE.get() == Some(&mode) {
            return Layer {
                name: Some("--mode"),
                ..Layer::new("flag", value)
            };
        }
        // `ATELIER_APPROVE`, or the older `approve = true`.
        if mode == Mode::configured(&self.effective_settings.defaults) {
            let approve = std::env::var("ATELIER_APPROVE");
            if matches!(approve.as_deref(), Ok("all" | "yes" | "1")) {
                return Layer {
                    name: Some("ATELIER_APPROVE"),
                    ..Layer::new("env", value)
                };
            }
            return Layer {
                key: Some("defaults.approve"),
                ..Layer::new("project", value)
            };
        }
        Layer::new("session", value)
    }

    /// The layers of `spec`, the one in force first among those that count.
    fn layers(&self, spec: &'static Spec) -> Vec<Layer> {
        let mut layers = self.configured(spec);
        let Some(live) = self.live(spec.key) else {
            return layers;
        };
        let winner = layers.iter().find(|l| l.ignored.is_none());
        if winner.is_some_and(|l| l.value == live) {
            return layers;
        }
        let top = match spec.key {
            "defaults.mode" => self.mode_layer(),
            // Auto-approve is the `yolo` mode: it comes from where the mode
            // does.
            "defaults.approve" => {
                let mode = find("defaults.mode").map(|m| self.layers(m));
                let from = mode.and_then(|l| l.into_iter().find(|l| l.ignored.is_none()));
                let from = from.unwrap_or_else(|| self.mode_layer());
                Layer {
                    key: from.key.or(Some("defaults.mode")),
                    value: live,
                    ..from
                }
            }
            _ => Layer::new("session", live),
        };
        layers.insert(0, top);
        layers
    }

    /// One setting, as `settings/get` lists it.
    fn entry(&self, spec: &'static Spec) -> Value {
        let layers = self.layers(spec);
        let mut counting = layers.iter().filter(|l| l.ignored.is_none());
        let (value, source) = if spec.combined {
            let source = counting.clone().next().map_or("default", |l| l.source);
            let value = if spec.kind == Kind::Table {
                // By name, the stronger file's entry replacing the other's.
                let mut all = Map::new();
                for layer in counting.rev() {
                    all.extend(layer.value.as_object().cloned().unwrap_or_default());
                }
                Value::Object(all)
            } else {
                let lists = counting.filter_map(|l| l.value.as_array());
                Value::Array(lists.flatten().cloned().collect())
            };
            (value, source)
        } else {
            match counting.next() {
                Some(layer) => (layer.value.clone(), layer.source),
                None => (Value::Null, "default"),
            }
        };
        let scopes: Vec<&str> = spec.scopes.iter().map(|s| s.name()).collect();
        let mut entry = json!({
            "key": spec.key,
            "type": spec.kind.name(),
            "description": spec.about,
            "settable": !scopes.is_empty(),
            "scopes": scopes,
            "value": value,
            "source": source,
            "layers": layers.iter().map(Layer::json).collect::<Vec<_>>(),
        });
        if let Kind::Choice(values) = spec.kind {
            entry["values"] = json!(values);
        }
        if spec.combined {
            entry["combined"] = Value::Bool(true);
        }
        if let Some(label) = spec.label {
            entry["label"] = Value::from(label);
        }
        if let Some(env) = spec.env {
            entry["env"] = Value::from(env);
        }
        entry
    }
}

/// `settings/get`: every setting with its value and where it comes from,
/// the files involved, and whether the project is trusted.
pub fn get(session: &Session, root: &Path) -> Value {
    let view = View::new(session, root);
    let trust = session.trust();
    // Keys the environment holds: that they are there, never what they are.
    let keys: Vec<Value> = [
        ("ATELIER_API_KEY", "the bearer token of the `env` provider"),
        ("ATELIER_SEARCH_KEY", "the `web_search` provider's API key"),
    ]
    .iter()
    .map(|(name, about)| {
        let set = std::env::var_os(name).is_some_and(|v| !v.is_empty());
        json!({ "name": name, "description": about, "set": set, "source": set.then_some("env") })
    })
    .collect();
    json!({
        "files": { "user": view.user.describe(), "project": view.project.describe() },
        "trust": { "state": trust.state().name(), "active": trust.active() },
        "settings": SPECS.iter().map(|s| view.entry(s)).collect::<Vec<_>>(),
        "keys": keys,
    })
}

/// Check `value` against what `spec` holds.
fn validate(spec: &Spec, value: &Value) -> Result<(), Failure> {
    let key = spec.key;
    let fits = match spec.kind {
        Kind::Bool => value.is_boolean(),
        Kind::Integer => value
            .as_u64()
            .is_some_and(|n| n > 0 && u32::try_from(n).is_ok()),
        Kind::Text => value.as_str().is_some_and(|s| !s.trim().is_empty()),
        Kind::Choice(values) => value.as_str().is_some_and(|s| values.contains(&s)),
        Kind::List | Kind::Tables | Kind::Table => false,
    };
    if fits {
        return Ok(());
    }
    Err(Failure::params(match spec.kind {
        Kind::Bool => format!("'{key}' must be true or false"),
        Kind::Integer => format!("'{key}' must be a positive whole number"),
        Kind::Choice(values) => format!("'{key}' must be one of: {}", values.join(", ")),
        _ => format!("'{key}' must be a non-empty string"),
    }))
}

/// `settings` with `key` set to `value` (`Null`: unset).
fn changed(settings: &Settings, key: &str, value: &Value) -> Result<Settings, Failure> {
    let mut doc = serde_json::to_value(settings).map_err(internal)?;
    put(&mut doc, key, value.clone());
    serde_json::from_value(doc).map_err(|e| Failure::params(format!("'{key}': {e}")))
}

fn internal(e: impl std::fmt::Display) -> Failure {
    Failure::new(rpc::code::INTERNAL_ERROR, e.to_string())
}

/// `settings/set`: write one setting to the user-global `atelier.toml`, or
/// with `"scope": "project"` to the project's, and put it into effect where
/// the `/config` screen would.
pub fn set(session: &mut Session, root: &Path, params: &Value) -> Result<Value, Failure> {
    let key = params["key"]
        .as_str()
        .ok_or_else(|| Failure::params("\"key\" must be a setting's key path"))?;
    let Some(value) = params.get("value") else {
        return Err(Failure::params(
            "\"value\" is required (null unsets the setting)",
        ));
    };
    let scope = match &params["scope"] {
        Value::Null => Scope::User,
        Value::String(s) if s == "user" => Scope::User,
        Value::String(s) if s == "project" => Scope::Project,
        _ => return Err(Failure::params("\"scope\" must be \"user\" or \"project\"")),
    };
    let spec = find(key).ok_or_else(|| Failure::params(format!("unknown setting '{key}'")))?;
    if spec.scopes.is_empty() {
        return Err(Failure::params(format!(
            "'{key}' cannot be changed with settings/set"
        )));
    }
    if !spec.scopes.contains(&scope) {
        let other = spec.scopes[0].name();
        return Err(Failure::params(format!(
            "'{key}' is not read from the {} file: pass \"scope\": \"{other}\"",
            scope.name()
        )));
    }
    if !value.is_null() {
        validate(spec, value)?;
    }

    // Whether the session followed its configuration before the change: if
    // a command moved it since (`/model`), the new default waits.
    let before = View::new(session, root).layers(spec);
    let followed = before.first().is_none_or(|l| l.source != "session");
    let file = match scope {
        Scope::Project => {
            let before = session.settings().clone();
            *session.settings_mut() = changed(&before, key, value)?;
            if let Err(e) = session.save_settings() {
                *session.settings_mut() = before;
                return Err(internal(format!("{e:#}")));
            }
            Settings::path(root)
        }
        Scope::User => {
            let dir = Settings::user_dir()
                .ok_or_else(|| internal("no user configuration directory (HOME is not set)"))?;
            // A file that can't be read as settings is left alone: writing
            // would replace it with the one key.
            let before = Settings::load(&dir).map_err(|e| internal(format!("{e:#}")))?;
            let after = changed(&before, key, value)?;
            std::fs::create_dir_all(&dir)
                .map_err(|e| internal(format!("creating {}: {e}", dir.display())))?;
            after.save(&dir).map_err(|e| internal(format!("{e:#}")))?;
            Settings::path(&dir)
        }
    };

    // Into effect, as the screen does on leaving — when the value written
    // is the one that counts.
    let winner = {
        let view = View::new(session, root);
        let layers = view.configured(spec);
        layers.into_iter().find(|l| l.ignored.is_none())
    };
    let wins = winner
        .as_ref()
        .is_some_and(|l| l.key.is_none() && l.source == scope.name() && &l.value == value);
    let mut note = None;
    let applies = if value.is_null() {
        // The session keeps the value it has.
        "next_start"
    } else if spec.key == "defaults.model" && session.is_claude() {
        note = Some("the Claude backend picks its own model; this is for the others");
        "next_start"
    } else if scope == Scope::Project || (wins && followed) {
        apply(session, spec, value);
        "now"
    } else if wins {
        "next_start"
    } else {
        "overridden"
    };

    let mut result = json!({
        "key": key,
        "scope": scope.name(),
        "value": value,
        "file": file.display().to_string(),
        "applies": applies,
        "setting": View::new(session, root).entry(spec),
    });
    if applies == "overridden"
        && let Some(by) = winner
    {
        result["by"] = by.json();
    }
    if let Some(note) = note {
        result["note"] = Value::from(note);
    }
    Ok(result)
}

/// Put a setting's new value into the running session, with the setters
/// the `/config` screen uses. They also note the value in the project's
/// settings, which is put back: that file is written only when asked.
fn apply(session: &mut Session, spec: &Spec, value: &Value) {
    let project = session.settings().clone();
    match spec.key {
        "defaults.model" => session.set_default_model(value.as_str().unwrap_or_default()),
        "defaults.context_limit" => {
            let n = value.as_u64().and_then(|n| u32::try_from(n).ok());
            session.set_context_limit(n.unwrap_or(1));
        }
        "review.enabled" => session.set_review_enabled(value.as_bool().unwrap_or(false)),
        _ => {}
    }
    *session.settings_mut() = project;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_unique_and_name_real_settings() {
        // Every key is a path the settings can hold: set to a value of its
        // kind, the settings still parse, and say it back.
        for (i, spec) in SPECS.iter().enumerate() {
            assert!(
                SPECS[..i].iter().all(|s| s.key != spec.key),
                "{} twice",
                spec.key
            );
            let sample = match spec.kind {
                Kind::Bool => json!(true),
                Kind::Integer => json!(7),
                Kind::Text => json!("x"),
                Kind::Choice(values) => json!(values[0]),
                Kind::List => json!(["x"]),
                Kind::Tables => json!([]),
                Kind::Table => json!({}),
            };
            let settings = changed(&Settings::default(), spec.key, &sample)
                .unwrap_or_else(|e| panic!("{}: {}", spec.key, e.message));
            let doc = serde_json::to_value(&settings).unwrap();
            if !matches!(spec.kind, Kind::Tables | Kind::Table) {
                assert_eq!(lookup(&doc, spec.key), Some(&sample), "{}", spec.key);
            }
        }
    }

    #[test]
    fn choices_are_the_names_the_code_knows() {
        let modes: Vec<&str> = Mode::ALL.iter().map(|m| m.name()).collect();
        assert_eq!(modes, MODES);
        for wake in WAKE {
            assert!(crate::coord::Wake::parse(wake).is_some(), "{wake}");
        }
    }

    #[test]
    fn only_screen_rows_are_settable_and_nothing_gated_or_user_only_at_project_scope() {
        let labels: Vec<_> = screen().filter_map(|s| s.label).collect();
        assert_eq!(
            labels,
            ["model", "auto-approve", "context limit", "review mode"]
        );
        for spec in SPECS {
            if spec.label.is_none() {
                assert!(spec.scopes.is_empty(), "{}", spec.key);
            }
            if spec.scopes.contains(&Scope::Project) {
                assert!(
                    spec.files != Files::User && !spec.needs_trust,
                    "{}",
                    spec.key
                );
            }
            if spec.scopes.contains(&Scope::User) {
                assert!(spec.files != Files::Project, "{}", spec.key);
            }
        }
        assert!(find("defaults.approve").unwrap().scopes.is_empty());
        assert!(find("local.model").unwrap().scopes.is_empty());
    }

    #[test]
    fn values_are_checked_against_the_kind() {
        let limit = find("defaults.context_limit").unwrap();
        assert!(validate(limit, &json!(4000)).is_ok());
        for bad in [
            json!(0),
            json!(-1),
            json!("4000"),
            json!(1.5),
            json!(1u64 << 40),
        ] {
            assert!(validate(limit, &bad).is_err(), "{bad}");
        }
        let review = find("review.enabled").unwrap();
        assert!(validate(review, &json!(false)).is_ok());
        assert!(validate(review, &json!("on")).is_err());
        let model = find("defaults.model").unwrap();
        assert!(validate(model, &json!("local/m#high")).is_ok());
        assert!(validate(model, &json!("  ")).is_err());
        let mode = find("defaults.mode").unwrap();
        assert!(validate(mode, &json!("plan")).is_ok());
        assert!(validate(mode, &json!("planning")).is_err());
    }

    #[test]
    fn put_sets_and_unsets_a_path() {
        let mut doc = json!({"defaults": {"model": "a"}});
        put(&mut doc, "defaults.context_limit", json!(9));
        put(&mut doc, "review.enabled", json!(true));
        put(&mut doc, "defaults.model", Value::Null);
        assert_eq!(
            doc,
            json!({"defaults": {"context_limit": 9}, "review": {"enabled": true}})
        );
        assert_eq!(lookup(&doc, "defaults.context_limit"), Some(&json!(9)));
        assert_eq!(lookup(&doc, "defaults.model"), None);
    }

    #[test]
    fn header_values_and_key_commands_are_not_shown() {
        let http = json!([{"name": "s", "url": "u", "headers": ["Authorization: Bearer sk-1", "X-Org: 7"]}]);
        let shown = redact("mcp_http", &http);
        assert_eq!(shown[0]["headers"], json!(["Authorization", "X-Org"]));
        let provider = json!([{
            "name": "p", "base_url": "u", "api_key_env": "P_KEY",
            "api_key_command": "echo sk-2", "headers": {"X-Token": "sk-3"},
        }]);
        let shown = redact("provider", &provider);
        assert_eq!(shown[0]["api_key_command"], true);
        assert_eq!(shown[0]["headers"], json!(["X-Token"]));
        assert_eq!(shown[0]["api_key_env"], "P_KEY");
        assert!(!shown.to_string().contains("sk-"), "{shown}");
    }

    #[test]
    fn environment_values_are_read_as_the_code_reads_them() {
        assert_eq!(env_value("ATELIER_APPROVE", "all"), Some(json!(true)));
        assert_eq!(env_value("ATELIER_APPROVE", "no"), Some(json!(false)));
        assert_eq!(
            env_value("ATELIER_PARALLEL_TOOLS", "off"),
            Some(json!(false))
        );
        assert_eq!(env_value("ATELIER_SANDBOX", "maybe"), None);
        assert_eq!(env_value("ATELIER_CONTEXT_LIMIT", "0"), None);
        assert_eq!(env_value("ATELIER_CONTEXT_LIMIT", "900"), Some(json!(900)));
        assert_eq!(env_value("ATELIER_MODE", "plan"), Some(json!("plan")));
        assert_eq!(env_value("ATELIER_MODE", "plna"), None);
        assert_eq!(
            env_value("ATELIER_COORDINATOR_WAKE", "x"),
            Some(json!("never"))
        );
        assert_eq!(env_value("ATELIER_MODEL", "m"), Some(json!("m")));
    }
}
