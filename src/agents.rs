//! Agent definitions: what a subagent is, before it runs.
//!
//! The `task` tool (see [`crate::tools::task`]) runs a *named agent* on a
//! prompt in a fresh conversation and hands its final report back. This
//! module only says which agents exist and what each may do; running one is
//! the agent loop's business (`agent::subagent`).
//!
//! An agent is a system prompt plus limits: which tools it gets, how many
//! steps it may take, optionally which model serves it.
//!
//! # Where they come from
//!
//! By name, the first found wins:
//!
//! 1. `<project>/.atelier/agents/<name>.md`
//! 2. `<project>/.claude/agents/<name>.md` — Claude Code's agent files, read
//!    as they are (see below for what is and is not understood)
//! 3. the user-global directory ([`global_dir`]):
//!    `$XDG_CONFIG_HOME/atelier/agents`, else `~/.config/atelier/agents`
//! 4. the built-ins: `explore` (read-only search) and `general`
//!
//! So a project can replace `explore` with its own by defining an agent of
//! that name. Nothing is cached: the directories are re-read whenever agents
//! are listed or one is run, so an edited file applies at once.
//!
//! # The file
//!
//! Front matter (see [`crate::commands::front_matter`] — `key: value` lines,
//! not YAML) and a body, which is the agent's system prompt:
//!
//! - `name` — defaults to the file's stem
//! - `description` — required; it is what the calling model chooses by
//! - `model` — optional; the model that serves this agent's requests
//! - `tools` — optional; tool names, `*` wildcards allowed, as a comma list
//!   (`read, grep, mcp__docs__*`), a bracketed one, or `- item` lines.
//!   Default: every tool the session has
//! - `max_steps` — optional; model requests the agent may make before it is
//!   told to wrap up (default [`DEFAULT_MAX_STEPS`])
//!
//! No agent ever gets the `task` tool, whatever `tools` says: subagents do
//! not start subagents.
//!
//! Files under `.claude/agents` name Claude Code's tools (`Read`, `WebFetch`)
//! and Claude models (`sonnet`, `inherit`). The tool names are matched to
//! atelier's ignoring case and underscores; `model` is ignored there, since
//! it does not name a model of atelier's endpoint.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::commands::{config_home, front_matter};
use crate::permissions::wild;

/// The name of the tool that runs an agent — which no agent is given.
pub const TASK_TOOL: &str = "task";

/// Model requests an agent may make when its definition does not say.
pub const DEFAULT_MAX_STEPS: u32 = 40;

/// Longest agent name accepted.
const MAX_NAME_CHARS: usize = 64;

/// What the built-in `explore` agent may call: tools that only look.
const EXPLORE_TOOLS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "ls",
    "tree",
    "web_fetch",
    "web_search",
];

const EXPLORE_PROMPT: &str = "\
You are a read-only exploration agent working inside a project directory. \
Your job is to find things — where something is defined, how a piece of code \
works, which files are involved in a behaviour — and report back.

Search broadly first (grep, glob, tree), then read only the parts of files \
that matter. Do not modify anything: you have no tools for it, and you must \
not suggest that you changed something.

Finish with a report that stands on its own: answer the question asked, and \
back every claim with file:line references so the reader can go straight \
there. Say plainly what you looked for and did not find.";

const GENERAL_PROMPT: &str = "\
You are a general-purpose agent working inside a project directory. You have \
tools to read and modify files and run commands; use them to carry out the \
task you were given rather than guessing. Do what was asked — no more — and \
verify your work where you can.

Finish with a report that stands on its own: what you did (files changed, \
commands run and their outcome), what you found, and anything left undone or \
uncertain.";

/// Where an agent's definition was found.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Builtin,
    /// A file in the user-global directory.
    Global(PathBuf),
    /// A file in the project.
    Project(PathBuf),
}

/// One agent definition.
#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub name: String,
    /// When to use it — shown to the calling model and in `/agents`.
    pub description: String,
    /// The model for its requests; `None` means the session's.
    pub model: Option<String>,
    /// Tool-name patterns it may call; `None` means every tool (but `task`).
    pub tools: Option<Vec<String>>,
    /// Model requests it may make before tools are withdrawn.
    pub max_steps: u32,
    /// Its system prompt.
    pub prompt: String,
    pub source: Source,
}

impl Agent {
    /// Whether this agent may be given the tool called `tool`.
    pub fn allows(&self, tool: &str) -> bool {
        // Depth limit: an agent cannot start agents.
        if tool == TASK_TOOL {
            return false;
        }
        let Some(patterns) = &self.tools else {
            return true;
        };
        patterns.iter().any(|p| {
            wild(p, tool)
                // Claude Code's spelling of a tool (`WebFetch` for
                // `web_fetch`). Only for plain CamelCase names: folding a
                // pattern or an `mcp__server__tool` name could match more
                // than was written.
                || (!p.contains(['*', '_'])
                    && p.to_lowercase() == tool.replace('_', "").to_lowercase())
        })
    }
}

/// What a scan for agents found.
#[derive(Debug, Default)]
pub struct Found {
    /// Usable agents, sorted by name; one per name.
    pub agents: Vec<Agent>,
    /// One line per agent file that was skipped, saying why.
    pub invalid: Vec<String>,
}

impl Found {
    pub fn get(&self, name: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.name == name)
    }
}

/// The user-global agents directory from the environment, or `None` when no
/// home directory is known.
pub fn global_dir() -> Option<PathBuf> {
    Some(config_home()?.join("atelier").join("agents"))
}

/// Every agent available to the project at `root`. `global` is the
/// user-global directory — [`global_dir`] in production, a temp directory
/// (or `None`) in tests, which is why it is passed in.
pub fn discover(root: &Path, global: Option<&Path>) -> Found {
    let mut found = Found::default();
    let atelier = root.join(".atelier").join("agents");
    let claude = root.join(".claude").join("agents");
    scan(&atelier, Source::Project, false, &mut found);
    scan(&claude, Source::Project, true, &mut found);
    if let Some(dir) = global {
        scan(dir, Source::Global, false, &mut found);
    }
    for agent in builtins() {
        if found.get(&agent.name).is_none() {
            found.agents.push(agent);
        }
    }
    found.agents.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// The agents that exist without any file.
fn builtins() -> Vec<Agent> {
    vec![
        Agent {
            name: "explore".to_string(),
            description: "Read-only search of the project (and the web): finds where things \
                are and how they work, and reports with file:line references. Cannot modify \
                anything. Use it for questions that would take many file reads to answer."
                .to_string(),
            model: None,
            tools: Some(EXPLORE_TOOLS.iter().map(|t| t.to_string()).collect()),
            max_steps: DEFAULT_MAX_STEPS,
            prompt: EXPLORE_PROMPT.to_string(),
            source: Source::Builtin,
        },
        Agent {
            name: "general".to_string(),
            description: "General-purpose agent with every tool you have: carries out a \
                self-contained, multi-step task (research, an isolated change, running and \
                fixing something) and reports what it did."
                .to_string(),
            model: None,
            tools: None,
            max_steps: DEFAULT_MAX_STEPS,
            prompt: GENERAL_PROMPT.to_string(),
            source: Source::Builtin,
        },
    ]
}

/// Add the agents defined by the `*.md` files directly in `dir` to `found`,
/// skipping names an earlier directory already defined. `claude_compat`
/// marks Claude Code's own directory (see the module docs).
fn scan(dir: &Path, source: fn(PathBuf) -> Source, claude_compat: bool, found: &mut Found) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md") && p.is_file())
        .collect();
    // Directory order is arbitrary; make "first found" mean something.
    files.sort();
    for file in files {
        match load(&file, claude_compat) {
            Ok(agent) if found.get(&agent.name).is_some() => {}
            Ok(mut agent) => {
                agent.source = source(file);
                found.agents.push(agent);
            }
            Err(e) => found.invalid.push(format!("{}: {e:#}", file.display())),
        }
    }
}

/// Read one agent file. The source is filled in by the caller.
fn load(file: &Path, claude_compat: bool) -> Result<Agent> {
    let text = std::fs::read_to_string(file).context("reading the file")?;
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    parse(stem, &text, claude_compat)
}

/// Parse an agent file's `text`; `stem` names the agent unless the front
/// matter does.
fn parse(stem: &str, text: &str, claude_compat: bool) -> Result<Agent> {
    let (fields, body) = front_matter(text);
    let field = |key: &str| {
        fields
            .iter()
            .find(|(k, v)| *k == key && !v.is_empty())
            .map(|(_, v)| v.as_str())
    };
    let name = field("name").unwrap_or(stem);
    if name.is_empty()
        || name.chars().count() > MAX_NAME_CHARS
        || name.chars().any(char::is_whitespace)
    {
        bail!("`name` must be one word of at most {MAX_NAME_CHARS} characters");
    }
    let description = field("description").context("no `description` in the front matter")?;
    let prompt = body.trim();
    if prompt.is_empty() {
        bail!("no system prompt (the text after the front matter)");
    }
    let max_steps = match field("max_steps").or_else(|| field("max-steps")) {
        None => DEFAULT_MAX_STEPS,
        Some(v) => v
            .parse::<u32>()
            .ok()
            .filter(|&n| n > 0)
            .with_context(|| format!("`max_steps` must be a positive number, not {v:?}"))?,
    };
    // `inherit` is how other harnesses spell "the session's model".
    let model = field("model")
        .filter(|m| !claude_compat && *m != "inherit")
        .map(str::to_string);
    Ok(Agent {
        name: name.to_string(),
        description: description.to_string(),
        model,
        tools: field("tools").map(tool_list),
        max_steps,
        prompt: prompt.to_string(),
        // Replaced by `scan`, which knows the path.
        source: Source::Builtin,
    })
}

/// The patterns of a `tools` value: comma-separated, optionally bracketed,
/// each optionally quoted.
fn tool_list(value: &str) -> Vec<String> {
    let value = value.trim();
    let value = value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(value);
    value
        .split(',')
        .map(|t| t.trim().trim_matches(['"', '\'']))
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// `/agents`: the agents, where each comes from and what it may do.
pub fn command(root: &Path, global: Option<&Path>) -> Vec<String> {
    let found = discover(root, global);
    let mut out = vec![format!("{} agent(s):", found.agents.len())];
    for a in &found.agents {
        out.push(format!("  {} — {}", a.name, a.description));
        let source = match &a.source {
            Source::Builtin => "built-in".to_string(),
            // Project agents by their path in the project, global ones in full.
            Source::Project(p) => p.strip_prefix(root).unwrap_or(p).display().to_string(),
            Source::Global(p) => p.display().to_string(),
        };
        let tools = match &a.tools {
            None => "all".to_string(),
            Some(t) => t.join(", "),
        };
        let model = a
            .model
            .as_ref()
            .map(|m| format!(" · model: {m}"))
            .unwrap_or_default();
        out.push(format!(
            "      {source} · tools: {tools} · max {} steps{model}",
            a.max_steps
        ));
    }
    if !found.invalid.is_empty() {
        out.push("skipped:".to_string());
        out.extend(found.invalid.iter().map(|l| format!("  {l}")));
    }
    out.push(
        "  define one in .atelier/agents/<name>.md (front matter: description, tools, model, \
         max_steps; the body is its system prompt)"
            .to_string(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh project root and global agents directory under the system
    /// temp dir — never the real home.
    fn dirs(name: &str) -> (PathBuf, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("atelier-agents-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (root, global) = (base.join("project"), base.join("global"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        (root, global)
    }

    fn write(path: PathBuf, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn builtins_exist_and_never_get_task() {
        let (root, global) = dirs("builtins");
        let found = discover(&root, Some(&global));
        let names: Vec<&str> = found.agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["explore", "general"]);

        let explore = found.get("explore").unwrap();
        assert!(explore.allows("read") && explore.allows("grep") && explore.allows("web_fetch"));
        assert!(!explore.allows("write") && !explore.allows("bash") && !explore.allows("task"));

        let general = found.get("general").unwrap();
        assert!(general.allows("bash") && general.allows("mcp__x__y"));
        assert!(!general.allows("task"));
    }

    #[test]
    fn a_file_is_parsed_in_full() {
        let agent = parse(
            "reviewer",
            "---\ndescription: Reviews a diff\nmodel: big-model\ntools: read, grep, mcp__docs__*\n\
             max_steps: 12\n---\n\nYou review diffs.\n",
            false,
        )
        .unwrap();
        assert_eq!(agent.name, "reviewer");
        assert_eq!(agent.description, "Reviews a diff");
        assert_eq!(agent.model.as_deref(), Some("big-model"));
        assert_eq!(agent.max_steps, 12);
        assert_eq!(agent.prompt, "You review diffs.");
        assert!(agent.allows("read") && agent.allows("mcp__docs__search"));
        assert!(!agent.allows("write") && !agent.allows("mcp__other__search"));

        // Defaults: the stem names it, every tool but `task`, 40 steps.
        let plain = parse("helper", "---\ndescription: d\n---\nBody", false).unwrap();
        assert_eq!(plain.name, "helper");
        assert_eq!((&plain.model, plain.max_steps), (&None, DEFAULT_MAX_STEPS));
        assert!(plain.allows("bash") && !plain.allows("task"));
        // Even when the file asks for it.
        let greedy = parse("g", "---\ndescription: d\ntools: \"*\"\n---\nBody", false).unwrap();
        assert!(greedy.allows("bash") && !greedy.allows("task"));
    }

    #[test]
    fn tools_may_be_written_as_a_list() {
        for tools in ["tools: [read, \"grep\"]", "tools:\n  - read\n  - grep"] {
            let text = format!("---\ndescription: d\n{tools}\n---\nBody");
            let agent = parse("a", &text, false).unwrap();
            assert_eq!(agent.tools, Some(vec!["read".into(), "grep".into()]));
        }
    }

    #[test]
    fn claude_files_are_read_as_they_are() {
        let agent = parse(
            "a",
            "---\nname: code-finder\ndescription: d\ntools: Read, WebFetch, NotebookEdit\n\
             model: sonnet\n---\nBody",
            true,
        )
        .unwrap();
        assert_eq!(agent.name, "code-finder");
        assert!(agent.allows("read") && agent.allows("web_fetch") && agent.allows("notebook_edit"));
        assert!(!agent.allows("write"));
        // A Claude model name means nothing to atelier's endpoint.
        assert_eq!(agent.model, None);
        let inherit = parse("a", "---\ndescription: d\nmodel: inherit\n---\nBody", false);
        assert_eq!(inherit.unwrap().model, None);
    }

    #[test]
    fn broken_files_are_reported_not_used() {
        assert!(parse("a", "Just a prompt", false).is_err());
        assert!(parse("a", "---\ndescription: d\n---\n", false).is_err());
        assert!(parse("a", "---\ndescription: d\nmax_steps: 0\n---\nBody", false).is_err());
        assert!(
            parse(
                "a",
                "---\ndescription: d\nname: two words\n---\nBody",
                false
            )
            .is_err()
        );

        let (root, global) = dirs("broken");
        write(root.join(".atelier/agents/bad.md"), "no front matter");
        write(root.join(".atelier/agents/notes.txt"), "not an agent");
        let found = discover(&root, Some(&global));
        assert_eq!(found.agents.len(), 2);
        assert_eq!(found.invalid.len(), 1);
        assert!(found.invalid[0].contains("bad.md") && found.invalid[0].contains("description"));
    }

    #[test]
    fn project_beats_global_beats_builtin() {
        let (root, global) = dirs("precedence");
        let file = |d: &str| format!("---\ndescription: {d}\n---\nPrompt of {d}");
        write(global.join("explore.md"), &file("global explore"));
        write(global.join("helper.md"), &file("global helper"));
        write(
            root.join(".claude/agents/helper.md"),
            &file("claude helper"),
        );
        write(root.join(".claude/agents/other.md"), &file("claude other"));
        write(
            root.join(".atelier/agents/other.md"),
            &file("atelier other"),
        );

        let found = discover(&root, Some(&global));
        let got: Vec<(&str, &str)> = found
            .agents
            .iter()
            .map(|a| (a.name.as_str(), a.description.as_str()))
            .collect();
        assert_eq!(got[0], ("explore", "global explore"));
        assert_eq!(got[1].0, "general");
        assert_eq!(got[2], ("helper", "claude helper"));
        assert_eq!(got[3], ("other", "atelier other"));
        assert!(matches!(
            found.get("explore").unwrap().source,
            Source::Global(_)
        ));
        assert!(matches!(
            found.get("general").unwrap().source,
            Source::Builtin
        ));

        let listing = command(&root, Some(&global)).join("\n");
        assert!(listing.contains("4 agent(s):"), "{listing}");
        assert!(listing.contains(".atelier/agents/other.md · tools: all · max 40 steps"));
        assert!(listing.contains("built-in · tools: all"));
    }
}
