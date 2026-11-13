//! `atelier doctor`: what atelier would run with here, and what is wrong.
//!
//! Most "it doesn't work" reports come down to something atelier already
//! knows and says nowhere, or says once on stderr at startup: the endpoint
//! is the built-in default because a variable was misspelt, `atelier.toml`
//! does not parse and was replaced by empty settings, the project's hooks
//! are waiting for approval. `doctor` walks the same sources a session
//! reads — through the same functions, so it cannot describe a different
//! program — and prints one line per finding:
//!
//! - `ok`: as it should be, with the fact itself (which file, which model
//!   and where its name came from), since the facts are half the use;
//! - `warn`: works, but probably not as intended (an agent file that was
//!   skipped, a model the endpoint does not list);
//! - `FAIL`: this will not work (a settings file that does not parse, an
//!   endpoint that does not answer). Any of these makes the exit status 1.
//!
//! # What it does not do
//!
//! It starts no session: no hook runs, no coordinator is launched, nothing
//! is written. It makes one network request, `GET /models`, which
//! `--offline` skips. It does not start the configured MCP servers — they
//! are programs, some slow, some with side effects — unless `--mcp` asks.
//!
//! Every check catches its own errors: a doctor that dies of the disease it
//! was asked about helps nobody, so nothing here returns `Err` or panics on
//! bad input, and a failure is a line like any other.

use std::io::IsTerminal;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::agent::{to_http, to_stdio};
use crate::config::{self, Backend, Config};
use crate::hooks::Hooks;
use crate::settings::{self, Settings};
use crate::trust::{State, Trust};
use crate::{agents, auth, commands, context, coord, headless, mcp, memory, provider, skills};

const USAGE: &str = "\
usage: atelier doctor [--json] [--offline] [--mcp] [--claude]

Check the setup atelier would run with in this directory.

      --json       print the checks as one JSON object
      --offline    make no network request (skip the endpoint checks)
      --mcp        start the configured MCP servers and report on each
      --claude     check the Claude Code backend (as ATELIER_BACKEND=claude)

exit status: 0 nothing failed (warnings included), 1 a check failed, 2 usage";

/// How long the endpoint gets to accept a connection. Short: this is a
/// check someone is waiting on, and a server that takes longer to connect
/// is one worth hearing about.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How many names of a list are spelt out on a line.
const NAMES_SHOWN: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    /// As printed: failures in capitals so they stand out in a column of
    /// `ok`.
    fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "FAIL",
        }
    }
}

/// One line of the checklist.
#[derive(Debug)]
struct Check {
    name: String,
    status: Status,
    detail: String,
}

/// The checklist so far.
#[derive(Default)]
struct Report {
    checks: Vec<Check>,
}

impl Report {
    fn add(&mut self, name: &str, status: Status, detail: impl Into<String>) {
        self.checks.push(Check {
            name: name.to_string(),
            status,
            // One line per check, whatever an error message contained.
            detail: detail
                .into()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        });
    }

    fn ok(&mut self, name: &str, detail: impl Into<String>) {
        self.add(name, Status::Ok, detail);
    }

    fn failed(&self) -> bool {
        self.checks.iter().any(|c| c.status == Status::Fail)
    }

    fn text(&self) -> String {
        let lines = self
            .checks
            .iter()
            .map(|c| format!("{:<5} {}: {}\n", c.status.label(), c.name, c.detail));
        lines.collect()
    }

    fn json(&self) -> Value {
        let checks: Vec<Value> = self
            .checks
            .iter()
            .map(|c| {
                json!({
                    "name": c.name,
                    "status": c.status.label().to_lowercase(),
                    "detail": c.detail,
                })
            })
            .collect();
        json!({ "ok": !self.failed(), "checks": checks })
    }
}

/// What the command line asked for.
#[derive(Debug, Default, PartialEq)]
struct Options {
    json: bool,
    offline: bool,
    mcp: bool,
    claude: bool,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut opts = Options::default();
    for arg in args {
        match arg.as_str() {
            "--json" => opts.json = true,
            "--offline" => opts.offline = true,
            "--mcp" => opts.mcp = true,
            "--claude" => opts.claude = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(opts)
}

/// `atelier doctor [--json] [--offline] [--mcp] [--claude]`: print the
/// checklist and return the exit status.
pub fn run_command(args: &[String]) -> u8 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return 0;
    }
    let opts = match parse(args) {
        Ok(opts) => opts,
        Err(msg) => {
            eprintln!("atelier: doctor: {msg}\n\n{USAGE}");
            return headless::EXIT_USAGE;
        }
    };
    let report = match std::env::current_dir() {
        Ok(root) => examine(&root, &opts),
        Err(e) => {
            let mut report = Report::default();
            report.add(
                "project",
                Status::Fail,
                format!("no current directory: {e}"),
            );
            report
        }
    };
    if opts.json {
        println!("{}", report.json());
    } else {
        print!("{}", report.text());
    }
    u8::from(report.failed())
}

/// Every check, for the project at `root`.
fn examine(root: &Path, opts: &Options) -> Report {
    let mut report = Report::default();
    report.ok(
        "version",
        format!(
            "{}, {}/{}",
            crate::update::version_line(),
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    );

    let settings = check_settings(&mut report, root);
    let cfg = check_backend(&mut report, &settings, opts);
    if !matches!(cfg.backend, Backend::Claude { .. }) {
        check_endpoint(&mut report, &cfg, opts.offline);
    }
    check_instructions(&mut report, root);
    check_extensions(&mut report, root, &settings);
    check_mcp(&mut report, &settings, &cfg, opts.mcp);
    report.ok("coordinator", coord::describe());
    check_git(&mut report, root);
    check_terminal(&mut report);
    report
}

/// The two settings files: whether each exists and parses. Returns the
/// project's settings as a session would get them — empty when the file is
/// absent or broken.
fn check_settings(report: &mut Report, root: &Path) -> Settings {
    let project = Settings::path(root);
    let mut loaded = Settings::default();
    match read_settings(&project) {
        Ok(None) => report.ok("project config", format!("none ({})", project.display())),
        Ok(Some(settings)) => {
            report.ok("project config", project.display().to_string());
            loaded = settings;
        }
        Err(why) => report.add(
            "project config",
            Status::Fail,
            format!(
                "{} does not parse, so it is ignored as a whole — {why}",
                project.display()
            ),
        ),
    }
    // Only its `[permissions]` and `[[hooks]]` are read, but it is one
    // file: a mistake anywhere in it loses both.
    match Settings::user_dir().map(|dir| dir.join(settings::FILE_NAME)) {
        None => report.ok(
            "user config",
            "none (neither XDG_CONFIG_HOME nor HOME is set)",
        ),
        Some(user) => match read_settings(&user) {
            Ok(None) => report.ok("user config", format!("none ({})", user.display())),
            Ok(Some(_)) => report.ok("user config", user.display().to_string()),
            Err(why) => report.add(
                "user config",
                Status::Fail,
                format!("{} does not parse — {why}", user.display()),
            ),
        },
    }
    loaded
}

/// The settings in `path`: `None` when there is no such file, `Err` with
/// what is wrong (and where) when it can't be read as settings.
fn read_settings(path: &Path) -> Result<Option<Settings>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("unreadable: {e}"))?;
    parse_settings(&text).map(Some)
}

/// Parse a settings file's text; the error names the offending key when it
/// can be found.
fn parse_settings(text: &str) -> Result<Settings, String> {
    let error = match tomlproc::serde::from_str::<Settings>(text) {
        Ok(settings) => return Ok(settings),
        Err(e) => e.to_string(),
    };
    // A syntax error has no key to name, and the parser's message (with its
    // line) is all there is. Text that is valid TOML but not valid settings
    // can be narrowed down.
    let Ok(doc) = tomlproc::serde::from_str::<Value>(text) else {
        return Err(format!("syntax error: {error}"));
    };
    Err(match culprit(&doc) {
        Some((key, why)) => format!("at `{key}`: {why}"),
        None => error,
    })
}

/// The key of `doc` (a settings file as plain data) that keeps it from
/// being settings, as a path like `defaults.context_limit` or `mcp[1]`,
/// with what is wrong there.
///
/// Found by leaving things out: the culprit is the entry without which the
/// rest is accepted, looked for again inside itself for as long as that
/// narrows it down. `None` when no single entry is to blame.
fn culprit(doc: &Value) -> Option<(String, String)> {
    let accepts = |doc: &Value| serde_json::from_value::<Settings>(doc.clone());
    let why = accepts(doc).err()?.to_string();

    let mut path: Vec<Step> = Vec::new();
    loop {
        let children: Vec<Step> = match at(doc, &path)? {
            Value::Object(map) => map.keys().cloned().map(Step::Key).collect(),
            Value::Array(list) => (0..list.len()).map(Step::Index).collect(),
            _ => break,
        };
        let guilty = children.into_iter().find(|child| {
            let mut without = doc.clone();
            path.push(child.clone());
            let removed = remove(&mut without, &path);
            path.pop();
            removed && accepts(&without).is_ok()
        });
        match guilty {
            Some(step) => path.push(step),
            None => break,
        }
    }
    if path.is_empty() {
        return None;
    }
    let mut key = String::new();
    for step in &path {
        match step {
            Step::Key(k) if key.is_empty() => key.push_str(k),
            Step::Key(k) => key.push_str(&format!(".{k}")),
            Step::Index(i) => key.push_str(&format!("[{i}]")),
        }
    }
    Some((key, why))
}

/// One step into a document: a table's key or an array's index.
#[derive(Clone)]
enum Step {
    Key(String),
    Index(usize),
}

/// The value `path` leads to in `doc`.
fn at<'a>(doc: &'a Value, path: &[Step]) -> Option<&'a Value> {
    path.iter().try_fold(doc, |value, step| match step {
        Step::Key(k) => value.get(k),
        Step::Index(i) => value.get(i),
    })
}

/// Remove the entry `path` leads to; whether there was one.
fn remove(doc: &mut Value, path: &[Step]) -> bool {
    let Some((last, parents)) = path.split_last() else {
        return false;
    };
    let mut value = doc;
    for step in parents {
        let next = match step {
            Step::Key(k) => value.get_mut(k),
            Step::Index(i) => value.get_mut(i),
        };
        match next {
            Some(next) => value = next,
            None => return false,
        }
    }
    match (value, last) {
        (Value::Object(map), Step::Key(k)) => map.remove(k).is_some(),
        (Value::Array(list), Step::Index(i)) if *i < list.len() => {
            list.remove(*i);
            true
        }
        _ => false,
    }
}

/// Which backend and model a session would use, and what decided each.
/// Returns the configuration, resolved the way `main` resolves it.
fn check_backend(report: &mut Report, settings: &Settings, opts: &Options) -> Config {
    let set = |key: &str| std::env::var(key).is_ok_and(|v| !v.is_empty());
    let mut cfg = Config::from_env(opts.claude);

    if let Backend::Claude { bin, model, .. } = &cfg.backend {
        let why = if opts.claude {
            "--claude"
        } else {
            "ATELIER_BACKEND=claude"
        };
        match on_path(bin) {
            Some(path) => report.ok(
                "backend",
                format!("Claude Code CLI at {} ({why})", path.display()),
            ),
            None => report.add(
                "backend",
                Status::Fail,
                format!("Claude Code CLI ({why}), but `{bin}` is not on PATH (ATELIER_CLAUDE_BIN names it)"),
            ),
        }
        report.ok(
            "model",
            match model {
                Some(m) => format!("{m} (ATELIER_MODEL, passed to claude as --model)"),
                None => "chosen by claude (ATELIER_MODEL is not set)".to_string(),
            },
        );
        return cfg;
    }

    let from = if set("ATELIER_BASE_URL") {
        "ATELIER_BASE_URL"
    } else {
        "the built-in default"
    };
    // The providers and model roles of atelier.toml, as a session applies
    // them; what follows describes the main model's.
    let user = Settings::load_user().unwrap_or_default();
    for warning in cfg.apply_settings(settings, &user) {
        report.add("providers", Status::Warn, warning);
    }
    let main = cfg.target(config::Role::Main);
    let key = main.provider.key_description();
    let signed_in = auth::load().is_some();
    if signed_in {
        report.ok(
            "backend",
            "signed in with ChatGPT: requests go to OpenAI's Responses API with that sign-in, \
             and its own model (/model). The endpoint below is the one /logout returns to",
        );
    }
    report.ok(
        if signed_in {
            "configured endpoint"
        } else {
            "backend"
        },
        format!(
            "OpenAI-compatible endpoint {} (provider `{}`{}); key: {key}",
            main.provider.base_url,
            main.provider.name,
            if main.provider.name == "env" {
                format!(", {from}")
            } else {
                String::new()
            },
        ),
    );

    let why = if set("ATELIER_MODEL") {
        "ATELIER_MODEL"
    } else if settings.models.main.is_some() {
        "[models] main in atelier.toml"
    } else if settings.defaults.model.is_some() {
        "[defaults] model in atelier.toml"
    } else {
        "the built-in default"
    };
    report.ok("model", format!("{} ({why})", cfg.model));
    if !set("ATELIER_BASE_URL") && main.provider.base_url == config::DEFAULT_BASE_URL {
        report.add(
            "endpoint address",
            Status::Warn,
            "ATELIER_BASE_URL is not set: the built-in default is a development server that \
             is unlikely to be yours",
        );
    }
    cfg
}

/// The full path of `bin` if it can be run: as given when it names a path,
/// else the first match on `PATH`.
fn on_path(bin: &str) -> Option<std::path::PathBuf> {
    let candidate = Path::new(bin);
    if candidate.components().count() > 1 {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
}

/// Whether the endpoint answers `GET /models`, how fast, and whether the
/// configured model is among those it lists.
fn check_endpoint(report: &mut Report, cfg: &Config, offline: bool) {
    if offline {
        report.ok("endpoint", "not contacted (--offline)");
        return;
    }
    let provider = cfg.target(config::Role::Main).provider;
    let url = provider.endpoint("models");
    let started = Instant::now();
    let models = match provider::list_models_within(&provider, CONNECT_TIMEOUT) {
        Ok(models) => models,
        Err(e) => {
            report.add("endpoint", Status::Fail, format!("GET {url}: {e:#}"));
            return;
        }
    };
    report.ok(
        "endpoint",
        format!(
            "GET {url} answered in {} ms with {} model(s)",
            started.elapsed().as_millis(),
            models.len()
        ),
    );
    if models.contains(&cfg.model) {
        report.ok(
            "model offered",
            format!("{} is in the endpoint's list", cfg.model),
        );
    } else {
        // A warning, not a failure: some servers serve one model under any
        // name, and some list only part of what they serve.
        report.add(
            "model offered",
            Status::Warn,
            format!(
                "{} is not in the endpoint's list ({})",
                cfg.model,
                some_of(&models)
            ),
        );
    }
}

/// The first few of `names`, and how many more there are.
fn some_of(names: &[String]) -> String {
    if names.is_empty() {
        return "it lists none".to_string();
    }
    let mut shown = names[..names.len().min(NAMES_SHOWN)].join(", ");
    if names.len() > NAMES_SHOWN {
        shown.push_str(&format!(", and {} more", names.len() - NAMES_SHOWN));
    }
    shown
}

/// The instruction files a session here would load, with their sizes.
fn check_instructions(report: &mut Report, root: &Path) {
    let files = context::instruction_files(root);
    if files.is_empty() {
        report.ok(
            "instructions",
            "no instruction file applies (AGENTS.md, CLAUDE.md, …)",
        );
        return;
    }
    let listed: Vec<String> = files
        .iter()
        .map(|f| {
            let size = std::fs::metadata(f).map_or(0, |m| m.len());
            format!("{} ({size} bytes)", f.display())
        })
        .collect();
    report.ok(
        "instructions",
        format!(
            "{} file(s), in load order: {}",
            files.len(),
            listed.join(", ")
        ),
    );
}

/// What extends the agent here: memory, skills, commands, agents, hooks.
/// Counts when all is well; a warning naming each file that was skipped.
fn check_extensions(report: &mut Report, root: &Path, settings: &Settings) {
    let store = memory::Store::new(root, memory::global_dir());
    let notes = store.list();
    let global = notes
        .iter()
        .filter(|(s, _)| *s == memory::Scope::Global)
        .count();
    report.ok(
        "memory",
        format!("{} project note(s), {global} global", notes.len() - global),
    );

    let found = skills::discover(root, &skills::global_dirs());
    counted(report, "skills", found.skills.len(), &found.invalid);

    let listed = commands::list(root, commands::global_dir().as_deref());
    report.ok("commands", format!("{} custom command(s)", listed.len()));

    let found = agents::discover(root, agents::global_dir().as_deref());
    counted(report, "agents", found.agents.len(), &found.invalid);

    // Whether the project's hooks run is part of whether the project is
    // trusted (see `trust`).
    let trust = Trust::load(root, settings, crate::trust::Store::from_env());
    let user_config = Settings::user_dir().map(|d| d.join(crate::settings::FILE_NAME));
    let hooks = Hooks::load(
        root,
        &settings.hooks,
        user_config.as_deref(),
        trust.hooks_state(),
    );
    let (user, project) = hooks.counts();
    let (status, trust) = match hooks.trust_state() {
        _ if project == 0 => (Status::Ok, String::new()),
        State::NotNeeded | State::Trusted => (Status::Ok, " (trusted)".to_string()),
        State::Env => (
            Status::Ok,
            " (run because the environment says to trust them)".to_string(),
        ),
        State::Untrusted => (
            Status::Warn,
            " — the project is NOT trusted, so they do not run: review them and /trust".to_string(),
        ),
        State::Changed => (
            Status::Warn,
            " — changed since the project was trusted, so they do not run: review them and \
             /trust"
                .to_string(),
        ),
    };
    let problems = hooks.problems();
    let detail = format!("{user} of the user's, {project} of the project's{trust}");
    if problems.is_empty() {
        report.add("hooks", status, detail);
    } else {
        report.add(
            "hooks",
            Status::Warn,
            format!("{detail}; left out: {}", problems.join("; ")),
        );
    }
}

/// A check for things loaded from files: how many were found, and a warning
/// if some files were skipped.
fn counted(report: &mut Report, name: &str, found: usize, invalid: &[String]) {
    if invalid.is_empty() {
        report.ok(name, format!("{found} found"));
    } else {
        report.add(
            name,
            Status::Warn,
            format!("{found} found; skipped: {}", invalid.join("; ")),
        );
    }
}

/// The configured MCP servers; with `connect`, each is started (or
/// contacted) and reports how long that took and how many tools it has.
fn check_mcp(report: &mut Report, settings: &Settings, cfg: &Config, connect: bool) {
    if settings.mcp.is_empty() && settings.mcp_http.is_empty() {
        report.ok("mcp", "no MCP servers configured");
        return;
    }
    let claude = matches!(cfg.backend, Backend::Claude { .. });
    let mut server = |name: &str, target: String, tools: &dyn Fn() -> anyhow::Result<usize>| {
        let check = format!("mcp '{name}'");
        if claude {
            report.ok(
                &check,
                format!("{target} — handed to claude, which starts it"),
            );
        } else if !connect {
            report.ok(&check, format!("{target} — not started (--mcp starts it)"));
        } else {
            let started = Instant::now();
            match tools() {
                Ok(n) => report.ok(
                    &check,
                    format!(
                        "{target} — connected in {} ms, {n} tool(s)",
                        started.elapsed().as_millis()
                    ),
                ),
                Err(e) => report.add(&check, Status::Fail, format!("{target} — {e:#}")),
            }
        }
    };
    for s in &settings.mcp {
        let target = format!("{} {}", s.command, s.args.join(" "));
        server(&s.name, target.trim().to_string(), &|| {
            mcp::connect_stdio(&to_stdio(s)).map(|c| c.tools.len())
        });
    }
    for s in &settings.mcp_http {
        server(&s.name, format!("{} (http)", s.url), &|| {
            mcp::connect_http(&to_http(s, &mcp::oauth::TokenStore::user())).map(|c| c.tools.len())
        });
    }
}

/// The state of the git repository, which the per-turn context and
/// `atelier review` read.
fn check_git(report: &mut Report, root: &Path) {
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    if git(&["--version"]).is_none() {
        report.add("git", Status::Warn, "git is not installed (or not on PATH)");
        return;
    }
    if git(&["rev-parse", "--is-inside-work-tree"]).as_deref() != Some("true") {
        report.add(
            "git",
            Status::Warn,
            "not a git repository: no git context for the model, and `atelier review` needs one",
        );
        return;
    }
    let branch = match git(&["branch", "--show-current"]) {
        Some(b) if !b.is_empty() => format!("on {b}"),
        _ => "detached HEAD".to_string(),
    };
    let changed = git(&["status", "--porcelain"]).map_or(0, |s| s.lines().count());
    report.ok(
        "git",
        match changed {
            0 => format!("{branch}, working tree clean"),
            n => format!("{branch}, {n} changed file(s)"),
        },
    );
}

/// What the terminal offers the inline TUI: it needs one on both ends.
fn check_terminal(report: &mut Report) {
    let tty = |is: bool| if is { "a terminal" } else { "not a terminal" };
    let (stdin, stdout) = (
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    );
    let term = std::env::var("TERM").unwrap_or_else(|_| "unset".to_string());
    let mut detail = format!(
        "stdin is {}, stdout is {}, TERM={term}",
        tty(stdin),
        tty(stdout)
    );
    #[cfg(tui)]
    match crate::tui::window_size() {
        Some((cols, rows)) => detail.push_str(&format!(", {cols}x{rows}")),
        None => detail.push_str(", no window size"),
    }
    #[cfg(not(tui))]
    detail.push_str("; this build has no TUI (the line REPL is used)");
    report.ok("terminal", detail);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn located(text: &str) -> String {
        parse_settings(text).expect_err("does not parse")
    }

    #[test]
    fn a_bad_settings_file_names_its_key() {
        assert!(parse_settings("").is_ok());
        assert!(parse_settings("[defaults]\nmodel = \"m\"\n").is_ok());

        let scalar = located("[defaults]\nmodel = \"m\"\ncontext_limit = \"lots\"\n");
        assert!(
            scalar.starts_with("at `defaults.context_limit`: "),
            "{scalar}"
        );

        let element = located("[[mcp]]\nname = \"a\"\ncommand = \"x\"\n\n[[mcp]]\nname = \"b\"\n");
        assert!(element.starts_with("at `mcp[1]`: "), "{element}");
        assert!(element.contains("command"), "{element}");

        let nested = located(
            "[[hooks]]\nevent = \"turn_end\"\ncommand = \"true\"\ntimeout_secs = \"soon\"\n",
        );
        assert!(
            nested.starts_with("at `hooks[0].timeout_secs`: "),
            "{nested}"
        );

        let table = located("review = 3\n");
        assert!(table.starts_with("at `review`: "), "{table}");

        assert!(located("[defaults\nmodel = 1\n").starts_with("syntax error: "));
    }

    #[test]
    fn the_report_fails_when_a_check_does() {
        let mut report = Report::default();
        report.ok("version", "atelier 1.2.3");
        report.add(
            "skills",
            Status::Warn,
            "1 found; skipped:\n  x/SKILL.md: no name",
        );
        assert!(!report.failed());
        assert_eq!(report.json()["ok"], true);
        report.add("endpoint", Status::Fail, "GET http://x/v1/models: refused");
        assert!(report.failed());
        assert_eq!(
            report.text(),
            "ok    version: atelier 1.2.3\n\
             warn  skills: 1 found; skipped: x/SKILL.md: no name\n\
             FAIL  endpoint: GET http://x/v1/models: refused\n"
        );
        let json = report.json();
        assert_eq!(json["ok"], false);
        assert_eq!(json["checks"][2]["status"], "fail");
        assert_eq!(json["checks"][1]["name"], "skills");
    }

    #[test]
    fn arguments() {
        let parse = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            parse(&args)
        };
        assert_eq!(parse(&[]), Ok(Options::default()));
        let all = parse(&["--mcp", "--json", "--offline", "--claude"]).unwrap();
        assert!(all.json && all.offline && all.mcp && all.claude);
        assert_eq!(parse(&["--fix"]), Err("unknown argument --fix".to_string()));
    }

    #[test]
    fn long_lists_are_abbreviated() {
        let names: Vec<String> = (0..11).map(|n| format!("m{n}")).collect();
        assert_eq!(some_of(&names[..2]), "m0, m1");
        assert_eq!(
            some_of(&names),
            "m0, m1, m2, m3, m4, m5, m6, m7, and 3 more"
        );
        assert_eq!(some_of(&[]), "it lists none");
    }
}
