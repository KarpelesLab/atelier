//! Permission rules (roadmap M17): which tool calls run, ask, or are refused.
//!
//! `[permissions]` in `atelier.toml` holds three lists of rules — `allow`,
//! `ask`, `deny`. A rule is `tool` or `tool(pattern)`:
//!
//! ```toml
//! [permissions]
//! allow = ["bash(git diff *)", "bash(cargo *)", "mcp__github__*"]
//! ask   = ["read(.env*)"]
//! deny  = ["read(~/.ssh/**)", "bash(rm -rf *)"]
//! ```
//!
//! # Decision order
//!
//! Any matching **deny** refuses the call (the model is told which rule, so
//! it can change course); else any matching **ask** prompts, even for a call
//! that would otherwise run unasked; else a matching **allow** runs it; else
//! the tool's own default applies ([`Tool::requires_approval_in`]). The
//! user-global file (`$XDG_CONFIG_HOME/atelier/atelier.toml`) is merged in:
//! the lists are unions, so a deny from either file wins over an allow from
//! either.
//!
//! [`Tool::requires_approval_in`]: crate::tools::Tool::requires_approval_in
//!
//! # What a pattern is matched against
//!
//! The tool part may hold `*` wildcards (`mcp__github__*`). The pattern is
//! matched against the call's *subject*, which depends on the tool:
//!
//! - **`bash`** — the command line. `*` matches any run of characters, except
//!   that a *trailing* `*` means "and any arguments": what precedes it must be
//!   whole words. `git diff *` and `git diff*` both match `git diff` and
//!   `git diff --stat`, and neither matches `git diffx`. (`**` at the end is
//!   the escape hatch: `./scripts/**` matches `./scripts/build.sh -v`.) A
//!   pattern without a trailing `*` must match the whole command. Matching is
//!   on the command's words after the shell's quoting is undone, joined by
//!   single spaces.
//!
//!   A compound command is taken apart first (see [`shell`]): every simple
//!   command in it — across `&&`, `||`, `;`, `|`, `&`, newlines, inside
//!   `$(…)`, backticks and `sh -c '…'` — is judged on its own. Deny and ask
//!   rules fire if *any* part matches (they also look through `sudo`, `env`,
//!   `VAR=x`, `/bin/` prefixes and the like, and at the raw line); allow
//!   needs *every* part matched by an allow rule. A part is never
//!   auto-allowed if it redirects output to a file, sets variables
//!   (`FOO=1 cmd`, unless the rule spells the assignment out), computes its
//!   command name, or sits in shell control flow. A command the splitter
//!   cannot follow is never auto-allowed by a pattern rule; deny and ask
//!   still see it through a cruder reading.
//!
//! - **file tools** (`read`, `write`, `edit`, `multiedit`, `apply_patch`,
//!   `notebook_edit`) — the path, made absolute and normalized lexically,
//!   exactly as [`tools::confine`](crate::tools::confine) does (so `a/../../x`
//!   is judged as what it resolves to, and symlinks are not followed). The
//!   pattern is a glob (`*` within a path component, `**` across them): `~/…`
//!   is the home directory, `/…` absolute, one containing a `/` is relative
//!   to the project root, and one without (`.env*`) names a file in any
//!   directory. A pattern naming a directory covers what is under it.
//!   `apply_patch` has several paths: any one matching is enough to deny or
//!   ask, all must match to allow.
//!
//! - **`web_fetch`** — the URL: `domain:example.com` matches that host and its
//!   subdomains; anything else is a `*` glob over the whole URL.
//!
//! - **anything else** (MCP tools, `node`, …) — the arguments as compact JSON
//!   (object keys sorted), with `*` matching any run of characters.
//!
//! A rule without a pattern covers the whole tool, which is what a bare name
//! in `allow` has always meant.
//!
//! # "Always"
//!
//! Answering "always" at a prompt saves the narrowest rule that would have
//! let the call through ([`Policy::propose`]), not the whole tool:
//! `bash(cargo test *)`, `web_fetch(domain:docs.rs)`. Where no narrow rule
//! makes sense — an open-ended launcher such as `sh`, `env`, `xargs`, `sudo`
//! or `python -c`, a command that cannot be followed — the call runs once and
//! nothing is saved.

mod shell;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use globset::GlobBuilder;
use serde_json::{Value, json};

use crate::agent::{Approval, Ui};
use crate::settings::{Permissions, Settings};
use crate::tools;

/// The tool result for a call the user refused at the prompt.
pub const DENIED: &str = "error: the user denied permission to run this tool.";

/// Tools whose subject is a path.
const FILE_TOOLS: &[&str] = &[
    "read",
    "write",
    "edit",
    "multiedit",
    "apply_patch",
    "notebook_edit",
];

/// Programs that run whatever they are handed: "always" never turns a call
/// to one of these into a rule.
const LAUNCHERS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "csh",
    "tcsh",
    "env",
    "xargs",
    "sudo",
    "doas",
    "su",
    "eval",
    "exec",
    "command",
    "builtin",
    "source",
    ".",
    "time",
    "nohup",
    "nice",
    "timeout",
    "watch",
    "parallel",
    "ssh",
    "nix-shell",
];

/// Interpreters: `python script.py …` can become a rule, `python -c …` (or
/// any other option first, or stdin) cannot.
const INTERPRETERS: &[&str] = &[
    "python", "python2", "python3", "node", "deno", "bun", "perl", "ruby", "php", "lua", "awk",
];

/// Programs whose first argument is a subcommand, so a useful rule keeps two
/// words (`git status`, not `git`).
const SUBCOMMAND_TOOLS: &[&str] = &[
    "git",
    "cargo",
    "npm",
    "pnpm",
    "yarn",
    "go",
    "docker",
    "podman",
    "kubectl",
    "helm",
    "gh",
    "pip",
    "pip3",
    "uv",
    "poetry",
    "rustup",
    "apt",
    "apt-get",
    "brew",
    "dotnet",
    "mvn",
    "gradle",
    "nix",
    "jj",
    "hg",
    "svn",
    "terraform",
    "systemctl",
    "composer",
    "bundle",
];

/// Programs too destructive to generalize from one approval: "always" saves
/// the exact command, never `rm *`.
const EXACT_ONLY: &[&str] = &[
    "rm", "rmdir", "dd", "shred", "chmod", "chown", "kill", "killall", "pkill", "truncate",
];

/// Which file a rule came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `<project>/atelier.toml`.
    Project,
    /// `$XDG_CONFIG_HOME/atelier/atelier.toml`.
    User,
}

impl Source {
    fn describe(self, root: &Path) -> String {
        match self {
            Source::Project => format!("project, {}", Settings::path(root).display()),
            Source::User => match Settings::user_dir() {
                Some(dir) => format!("user, {}", Settings::path(&dir).display()),
                None => "user".to_string(),
            },
        }
    }
}

/// One parsed rule: `tool` or `tool(pattern)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// As written.
    text: String,
    /// The tool name, possibly with `*` wildcards.
    tool: String,
    /// `None`: the whole tool.
    pattern: Option<String>,
}

impl Rule {
    /// Parse `tool` or `tool(pattern)`. The pattern runs from the first `(`
    /// to the closing `)` at the very end, so it may hold parentheses itself.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (tool, pattern) = match text.split_once('(') {
            None => (text, None),
            Some((tool, rest)) => {
                let pattern = rest
                    .strip_suffix(')')
                    .ok_or("a rule with a pattern must end with `)`")?
                    .trim();
                if pattern.is_empty() {
                    return Err("the pattern is empty".into());
                }
                // `tool(*)` is the whole tool, however the subject is read.
                (tool, Some(pattern).filter(|p| *p != "*"))
            }
        };
        if tool.is_empty() {
            return Err("the tool name is missing".into());
        }
        if tool.contains(|c: char| c.is_whitespace() || c == ')') {
            return Err("write `tool` or `tool(pattern)`".into());
        }
        if let Some(p) = pattern
            && FILE_TOOLS.contains(&tool)
            && let Err(e) = GlobBuilder::new(p).build()
        {
            return Err(format!("not a valid path glob: {e}"));
        }
        Ok(Self {
            text: text.to_string(),
            tool: tool.to_string(),
            pattern: pattern.map(str::to_string),
        })
    }

    /// Whether the rule is about `tool` at all.
    fn names(&self, tool: &str) -> bool {
        wild(&self.tool, tool)
    }
}

/// A rule and where it came from.
#[derive(Debug, Clone)]
struct Entry {
    rule: Rule,
    source: Source,
}

/// The rule behind a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matched {
    pub rule: String,
    pub source: Source,
}

impl Entry {
    fn matched(&self) -> Matched {
        Matched {
            rule: self.rule.text.clone(),
            source: self.source,
        }
    }
}

/// What the rules say about a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// A deny rule matched: refuse.
    Deny,
    /// An ask rule matched, or the command cannot be followed: prompt.
    Ask,
    /// Allow rules cover the call: run.
    Allow,
    /// No rule decides: the tool's own default applies.
    Default,
}

/// A [`Decision`] with its explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub decision: Decision,
    /// The rule that decided, if one did.
    pub by: Option<Matched>,
    /// How the call was read — per part of a compound command.
    pub notes: Vec<String>,
}

/// What a rule's pattern is matched against, by kind of tool.
enum Subject {
    Bash {
        /// The simple commands the line would run, or why it can't be
        /// followed.
        parts: Result<Vec<shell::Part>, String>,
        /// Every reading of the line a restrictive rule is tried on.
        forms: Vec<String>,
    },
    /// Absolute, lexically normalized.
    Paths(Vec<PathBuf>),
    Url {
        url: String,
        host: Option<String>,
    },
    Json(String),
}

impl Subject {
    fn of(tool: &str, args: &Value, root: &Path) -> Self {
        if tool == "bash" {
            let command = args.get("command").and_then(Value::as_str).unwrap_or("");
            let parts = shell::split(command);
            let mut forms = vec![command.split_whitespace().collect::<Vec<_>>().join(" ")];
            match &parts {
                Ok(parts) => {
                    forms.extend(parts.iter().flat_map(|p| shell::restrict_forms(&p.words)))
                }
                Err(_) => forms.extend(shell::rough_forms(command)),
            }
            Subject::Bash { parts, forms }
        } else if FILE_TOOLS.contains(&tool) {
            let named = if tool == "apply_patch" {
                tools::apply_patch::target_paths(args)
            } else {
                tools::path_arg(args)
                    .map(|p| vec![p.to_string()])
                    .unwrap_or_default()
            };
            Subject::Paths(named.iter().map(|p| absolute(root, p)).collect())
        } else if tool == "web_fetch" {
            let url = args.get("url").and_then(Value::as_str).unwrap_or("");
            Subject::Url {
                url: url.to_string(),
                host: url_host(url),
            }
        } else {
            Subject::Json(args.to_string())
        }
    }
}

/// `path` as the file tools resolve it: against the root, `.`/`..` folded,
/// symlinks left alone (the same reading as `tools::confine`).
fn absolute(root: &Path, path: &str) -> PathBuf {
    tools::normalize(&root.join(path))
}

/// The active rules, from both files.
#[derive(Clone)]
pub struct Policy {
    deny: Vec<Entry>,
    ask: Vec<Entry>,
    allow: Vec<Entry>,
    /// Rules that did not parse (and so are not applied), and other trouble
    /// the user should hear about.
    problems: Vec<String>,
    /// For `~/` in path patterns.
    home: Option<PathBuf>,
}

impl Policy {
    /// The policy for `project` rules over `user` ones.
    pub fn new(project: &Permissions, user: &Permissions, home: Option<PathBuf>) -> Self {
        let mut policy = Self {
            deny: Vec::new(),
            ask: Vec::new(),
            allow: Vec::new(),
            problems: Vec::new(),
            home,
        };
        for (perms, source) in [(project, Source::Project), (user, Source::User)] {
            for (name, texts) in [
                ("deny", &perms.deny),
                ("ask", &perms.ask),
                ("allow", &perms.allow),
            ] {
                for text in texts {
                    match Rule::parse(text) {
                        Ok(rule) => {
                            let list = match name {
                                "deny" => &mut policy.deny,
                                "ask" => &mut policy.ask,
                                _ => &mut policy.allow,
                            };
                            list.push(Entry { rule, source });
                        }
                        Err(e) => policy.problems.push(format!(
                            "ignored {name} rule `{text}` ({}): {e}",
                            match source {
                                Source::Project => "project",
                                Source::User => "user",
                            }
                        )),
                    }
                }
            }
        }
        policy
    }

    /// The policy for a session: the project's rules over the user-global
    /// file's, which is read once per process.
    pub fn load(project: &Permissions) -> Self {
        static USER: OnceLock<(Permissions, Option<String>)> = OnceLock::new();
        let (user, error) =
            USER.get_or_init(
                || match Settings::user_dir().map(|dir| Settings::load(&dir)) {
                    Some(Ok(settings)) => (settings.permissions, None),
                    Some(Err(e)) => (
                        Permissions::default(),
                        Some(format!("user permission rules not loaded: {e:#}")),
                    ),
                    None => (Permissions::default(), None),
                },
            );
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut policy = Self::new(project, user, home);
        policy.problems.extend(error.clone());
        policy
    }

    /// This policy with more project allow rules — to try rules out before
    /// saving them. `None` if one does not parse.
    fn with_allow(&self, rules: &[String]) -> Option<Self> {
        let mut policy = self.clone();
        for text in rules {
            policy.allow.push(Entry {
                rule: Rule::parse(text).ok()?,
                source: Source::Project,
            });
        }
        Some(policy)
    }

    /// What the rules say about calling `tool` with `args`.
    pub fn decide(&self, tool: &str, args: &Value, root: &Path) -> Verdict {
        let subject = Subject::of(tool, args, root);
        for (list, decision) in [(&self.deny, Decision::Deny), (&self.ask, Decision::Ask)] {
            let hit = list
                .iter()
                .find(|e| e.rule.names(tool) && self.restricts(&e.rule, &subject, root));
            if let Some(entry) = hit {
                return Verdict {
                    decision,
                    by: Some(entry.matched()),
                    notes: Vec::new(),
                };
            }
        }
        self.allowed(tool, &subject, root)
    }

    /// Whether a deny/ask `rule` catches the subject: any reading, any path.
    fn restricts(&self, rule: &Rule, subject: &Subject, root: &Path) -> bool {
        let Some(pattern) = &rule.pattern else {
            return true;
        };
        match subject {
            Subject::Bash { forms, .. } => forms.iter().any(|f| bash_match(pattern, f)),
            Subject::Paths(paths) => paths.iter().any(|p| self.path_match(pattern, p, root)),
            Subject::Url { url, host } => url_match(pattern, url, host.as_deref()),
            Subject::Json(json) => wild(pattern, json),
        }
    }

    /// The allow step: every part of the subject must be covered.
    fn allowed(&self, tool: &str, subject: &Subject, root: &Path) -> Verdict {
        let rules: Vec<&Entry> = self.allow.iter().filter(|e| e.rule.names(tool)).collect();
        let verdict = |decision, by: Option<&Entry>, notes| Verdict {
            decision,
            by: by.map(Entry::matched),
            notes,
        };
        // A whole-tool rule needs no reading of the subject.
        if let Some(entry) = rules.iter().find(|e| e.rule.pattern.is_none()) {
            return verdict(Decision::Allow, Some(entry), Vec::new());
        }
        let find = |matches: &dyn Fn(&str) -> bool| {
            rules
                .iter()
                .copied()
                .find(|e| e.rule.pattern.as_deref().is_some_and(matches))
        };
        match subject {
            Subject::Bash {
                parts: Err(why), ..
            } => verdict(
                Decision::Ask,
                None,
                vec![format!(
                    "the command has {why}, which cannot be followed with confidence: \
                     allow rules do not apply to it"
                )],
            ),
            Subject::Bash {
                parts: Ok(parts), ..
            } => {
                let mut by = None;
                let mut notes = Vec::new();
                let mut covered = true;
                for part in parts.iter().filter(|p| !p.wrapper) {
                    let line = shell::render(&part.words);
                    if let Some(blocker) = part.blocker {
                        covered = false;
                        notes.push(format!(
                            "`{line}` {blocker}: allow rules do not apply to it"
                        ));
                        continue;
                    }
                    let form = shell::render(shell::allow_form(&part.words));
                    match find(&|p| bash_match(p, &form)) {
                        Some(entry) => {
                            notes.push(format!("`{line}` is allowed by `{}`", entry.rule.text));
                            by = by.or(Some(entry));
                        }
                        None => {
                            covered = false;
                            notes.push(format!("no allow rule covers `{line}`"));
                        }
                    }
                }
                match by {
                    Some(entry) if covered => verdict(Decision::Allow, Some(entry), notes),
                    _ => verdict(Decision::Default, None, notes),
                }
            }
            Subject::Paths(paths) => {
                let found: Vec<Option<&Entry>> = paths
                    .iter()
                    .map(|path| find(&|p| self.path_match(p, path, root)))
                    .collect();
                match found.first() {
                    Some(Some(first)) if found.iter().all(Option::is_some) => {
                        verdict(Decision::Allow, Some(first), Vec::new())
                    }
                    _ => verdict(Decision::Default, None, Vec::new()),
                }
            }
            Subject::Url { url, host } => match find(&|p| url_match(p, url, host.as_deref())) {
                Some(entry) => verdict(Decision::Allow, Some(entry), Vec::new()),
                None => verdict(Decision::Default, None, Vec::new()),
            },
            Subject::Json(json) => match find(&|p| wild(p, json)) {
                Some(entry) => verdict(Decision::Allow, Some(entry), Vec::new()),
                None => verdict(Decision::Default, None, Vec::new()),
            },
        }
    }

    /// Whether the path glob `pattern` matches the absolute `path`.
    fn path_match(&self, pattern: &str, path: &Path, root: &Path) -> bool {
        let Some(glob) = path_glob(pattern, root, self.home.as_deref()) else {
            return false;
        };
        // The pattern itself, or — naming a directory — anything under it.
        [glob.clone(), format!("{glob}/**")].iter().any(|g| {
            GlobBuilder::new(g)
                .literal_separator(true)
                .build()
                .is_ok_and(|g| g.compile_matcher().is_match(path))
        })
    }

    /// The rules "always" would save for this call: the narrowest that let
    /// it (and its like) through, or none when no narrow rule makes sense —
    /// the call is then allowed once. For a compound command, one rule per
    /// part that is not already allowed.
    pub fn propose(&self, tool: &str, args: &Value, root: &Path) -> Vec<String> {
        match Subject::of(tool, args, root) {
            Subject::Bash { parts: Err(_), .. } => Vec::new(),
            Subject::Bash {
                parts: Ok(parts), ..
            } => {
                let mut rules = Vec::new();
                for part in parts.iter().filter(|p| !p.wrapper) {
                    if part.blocker.is_some() {
                        return Vec::new();
                    }
                    let words = shell::allow_form(&part.words);
                    let form = shell::render(words);
                    let has_rule = self.allow.iter().any(|e| {
                        e.rule.names(tool)
                            && e.rule
                                .pattern
                                .as_deref()
                                .is_none_or(|p| bash_match(p, &form))
                    });
                    if has_rule {
                        continue;
                    }
                    let Some(rule) = propose_bash(words) else {
                        return Vec::new();
                    };
                    if !rules.contains(&rule) {
                        rules.push(rule);
                    }
                }
                rules
            }
            Subject::Paths(paths) => paths
                .iter()
                .filter_map(|p| p.to_str())
                .map(|p| format!("{tool}({})", globset::escape(p)))
                .collect(),
            Subject::Url { host, .. } => host
                .map(|h| format!("{tool}(domain:{h})"))
                .into_iter()
                .collect(),
            // MCP tools and the rest: the tool, by name.
            Subject::Json(_) => vec![tool.to_string()],
        }
    }

    /// The active rules by source, for `/permissions`.
    pub fn listing(&self, root: &Path) -> Vec<String> {
        let mut out = vec!["permission rules (deny wins, then ask, then allow):".to_string()];
        for source in [Source::Project, Source::User] {
            out.push(format!("  {}:", source.describe(root)));
            let before = out.len();
            for (name, list) in [
                ("deny ", &self.deny),
                ("ask  ", &self.ask),
                ("allow", &self.allow),
            ] {
                out.extend(
                    list.iter()
                        .filter(|e| e.source == source)
                        .map(|e| format!("    {name} {}", e.rule.text)),
                );
            }
            if out.len() == before {
                out.push("    (none)".to_string());
            }
        }
        out.extend(self.problems.iter().map(|p| format!("  ! {p}")));
        out
    }
}

/// The rule "always" saves for one simple command: its first word, or first
/// two for programs with subcommands, plus ` *` when arguments followed.
fn propose_bash(words: &[String]) -> Option<String> {
    let assigned = words.iter().take_while(|w| shell::is_assignment(w)).count();
    let cmd = &words[assigned..];
    let program = shell::basename(cmd.first()?);
    if LAUNCHERS.contains(&program) {
        return None;
    }
    // `find … -exec` runs anything.
    if cmd
        .iter()
        .any(|w| matches!(w.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir"))
    {
        return None;
    }
    let keep = if INTERPRETERS.contains(&program) {
        // Only `python script.py …`: an option first means inline code, a
        // module, or stdin.
        if cmd.get(1).is_none_or(|arg| arg.starts_with('-')) {
            return None;
        }
        2
    } else if EXACT_ONLY.contains(&program) {
        cmd.len()
    } else if SUBCOMMAND_TOOLS.contains(&program) {
        match cmd.get(1) {
            None => 1,
            Some(sub)
                if sub.starts_with(|c: char| c.is_ascii_lowercase())
                    && sub
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_:".contains(c)) =>
            {
                2
            }
            // `git -C dir …`: the subcommand is somewhere behind options, and
            // `git *` would be far too wide.
            Some(_) => return None,
        }
    } else {
        1
    };
    let prefix = &words[..assigned + keep];
    // A word the rule syntax would read differently than the shell did.
    if prefix
        .iter()
        .any(|w| w.is_empty() || w.contains(|c: char| c.is_whitespace() || "*$`".contains(c)))
    {
        return None;
    }
    let mut rule = prefix.join(" ");
    if words.len() > prefix.len() {
        rule.push_str(" *");
    }
    Some(format!("bash({rule})"))
}

/// `*`-wildcard match of the whole of `text`: `*` is any run of characters
/// (none included), everything else is literal.
fn wild(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    // Where to resume after the last `*` when a literal run fails.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if p.get(pi) == Some(&'*') {
            star = Some((pi, ti));
            pi += 1;
        } else if p.get(pi) == Some(&t[ti]) {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            star = Some((sp, st + 1));
            pi = sp + 1;
            ti = st + 1;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Match a `bash` pattern against a command line (words joined by single
/// spaces). A trailing `*` is "and any arguments": the text before it must
/// match whole words, so `git diff *` — and `git diff*` — match `git diff`
/// and `git diff --stat` but not `git diffx`. Any other `*` is a plain
/// wildcard.
fn bash_match(pattern: &str, line: &str) -> bool {
    let pattern = pattern.split_whitespace().collect::<Vec<_>>().join(" ");
    match pattern.strip_suffix('*') {
        Some(head) => {
            let head = head.trim_end();
            head.is_empty() || wild(head, line) || wild(&format!("{head} *"), line)
        }
        None => wild(&pattern, line),
    }
}

/// The absolute glob a path pattern stands for: `~/…` under `home`, `/…` as
/// is, one with a `/` under the project root (leading `./` and `../`
/// resolved), and a bare name in any directory.
fn path_glob(pattern: &str, root: &Path, home: Option<&Path>) -> Option<String> {
    let escaped = |p: &Path| p.to_str().map(globset::escape);
    let pattern = match pattern.trim_end_matches('/') {
        "" => "/",
        trimmed => trimmed,
    };
    if pattern == "~" {
        return escaped(home?);
    }
    if let Some(rest) = pattern.strip_prefix("~/") {
        return Some(format!("{}/{rest}", escaped(home?)?));
    }
    if pattern.starts_with('/') {
        return Some(pattern.to_string());
    }
    if !pattern.contains('/') {
        return Some(format!("**/{pattern}"));
    }
    let mut base = root.to_path_buf();
    let mut rest = pattern;
    loop {
        if let Some(r) = rest.strip_prefix("./") {
            rest = r;
        } else if let Some(r) = rest.strip_prefix("../") {
            base.pop();
            rest = r;
        } else {
            break;
        }
    }
    Some(format!("{}/{rest}", escaped(&base)?))
}

/// The host of an `http(s)` URL, lowercased — `None` when it can't be read
/// with certainty (a rule on the domain then does not match).
fn url_host(url: &str) -> Option<String> {
    if url.contains(|c: char| c.is_whitespace() || c.is_control() || c == '\\') {
        return None;
    }
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    // `user:pass@host`: the host is what follows the last `@`.
    let host_port = authority.rsplit('@').next()?;
    let host = match host_port.strip_prefix('[') {
        Some(v6) => v6.split(']').next()?,
        None => host_port.split(':').next()?,
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// Match a `web_fetch` pattern: `domain:example.com` (that host or a
/// subdomain; with a `*`, a wildcard over the host), else a wildcard over the
/// URL.
fn url_match(pattern: &str, url: &str, host: Option<&str>) -> bool {
    let Some(domain) = pattern.strip_prefix("domain:") else {
        return wild(pattern, url);
    };
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    let Some(host) = host else { return false };
    if domain.contains('*') {
        wild(&domain, host)
    } else {
        host == domain || host.ends_with(&format!(".{domain}"))
    }
}

/// A tool call up for approval.
pub struct Request<'a> {
    pub tool: &'a str,
    /// Its arguments, as the JSON text the model sent.
    pub arguments: &'a str,
    /// The tool's own default for this call
    /// ([`Tool::requires_approval_in`](crate::tools::Tool::requires_approval_in)).
    pub needs_approval: bool,
}

/// Whether `req` may run: applies the rules, and prompts through `ui` when
/// they (or the tool's default) say to ask. `Err` carries the tool result for
/// the model — which rule refused, or that the user did. `auto_approve`
/// (`ATELIER_APPROVE=all`) answers every prompt with yes; deny rules still
/// hold. "Always" saves a rule to the project's `atelier.toml`.
pub fn approve(
    settings: &mut Settings,
    root: &Path,
    auto_approve: bool,
    req: Request,
    ui: &mut dyn Ui,
) -> Result<(), String> {
    let policy = Policy::load(&settings.permissions);
    // Trouble with the rule files is said once, not at every call.
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !policy.problems.is_empty() && !WARNED.swap(true, Ordering::Relaxed) {
        for problem in &policy.problems {
            ui.info(&format!("⚠ {problem}"));
        }
    }
    approve_with(&policy, settings, root, auto_approve, req, ui)
}

/// [`approve`] under a given policy.
fn approve_with(
    policy: &Policy,
    settings: &mut Settings,
    root: &Path,
    auto_approve: bool,
    req: Request,
    ui: &mut dyn Ui,
) -> Result<(), String> {
    let args: Value = serde_json::from_str(req.arguments).unwrap_or(Value::Null);
    let verdict = policy.decide(req.tool, &args, root);
    match verdict.decision {
        Decision::Deny => {
            let rule = verdict
                .by
                .map(|by| format!(" `{}` ({})", by.rule, by.source.describe(root)))
                .unwrap_or_default();
            return Err(format!(
                "error: refused by the permission rule{rule}. The user has ruled this out: do \
                 not retry it in another form — take an approach the rule does not cover, or \
                 ask the user."
            ));
        }
        Decision::Allow => return Ok(()),
        Decision::Default if !req.needs_approval => return Ok(()),
        Decision::Ask | Decision::Default if auto_approve => return Ok(()),
        Decision::Ask | Decision::Default => {}
    }
    if let Some(by) = &verdict.by {
        ui.info(&format!("the rule `{}` asks before this runs", by.rule));
    } else if verdict.decision == Decision::Ask {
        for note in &verdict.notes {
            ui.info(note);
        }
    }
    for signal in crate::risk::signals(req.tool, req.arguments) {
        ui.info(&format!("⚠ {signal}"));
    }
    match ui.ask_approval(req.tool, req.arguments) {
        Approval::Once => Ok(()),
        Approval::Deny => Err(DENIED.to_string()),
        Approval::Always => {
            remember(policy, settings, root, &verdict, req.tool, &args, ui);
            Ok(())
        }
    }
}

/// "Always": save the rules that let this call through, and say which — or
/// say that none fits and the call ran once.
fn remember(
    policy: &Policy,
    settings: &mut Settings,
    root: &Path,
    verdict: &Verdict,
    tool: &str,
    args: &Value,
    ui: &mut dyn Ui,
) {
    if let Some(by) = &verdict.by {
        // An allow rule would lose to the ask rule that prompted.
        ui.info(&format!(
            "allowed once: the rule `{}` asks every time (remove it to stop the prompt)",
            by.rule
        ));
        return;
    }
    let mut rules = policy.propose(tool, args, root);
    rules.retain(|r| !settings.permissions.allow.contains(r));
    // Only save what demonstrably lets this very call through next time.
    let works = policy
        .with_allow(&rules)
        .is_some_and(|trial| trial.decide(tool, args, root).decision == Decision::Allow);
    if rules.is_empty() || !works {
        ui.info(
            "allowed once: no narrow rule fits this call (to always allow it, add a rule to \
             [permissions] allow in atelier.toml)",
        );
        return;
    }
    settings.permissions.allow.extend(rules.iter().cloned());
    let saved = match settings.save(root) {
        Ok(()) => "saved to atelier.toml".to_string(),
        Err(e) => format!("kept for this session only — {e:#}"),
    };
    for rule in &rules {
        ui.info(&format!("always allow: `{rule}` ({saved})"));
    }
}

const USAGE: &str = "usage: atelier permissions check -- <command…>\n       \
                     atelier permissions check --tool <name> [--args '<json>']";

/// `atelier permissions check …`: print what the rules decide for a call, and
/// which rule decided.
pub fn run_command(args: &[String]) -> Result<()> {
    let Some(("check", rest)) = args.split_first().map(|(a, r)| (a.as_str(), r)) else {
        bail!(USAGE);
    };
    let (tool, call_args) = match rest {
        [flag, name, more @ ..] if flag == "--tool" => {
            let json = match more {
                [] => "{}",
                [flag, json] if flag == "--args" => json.as_str(),
                _ => bail!(USAGE),
            };
            let value = serde_json::from_str(json).context("--args is not valid JSON")?;
            (name.clone(), value)
        }
        _ => {
            // Several words are joined; quote the command as one argument to
            // check shell syntax (`-- 'git diff; rm x'`).
            let words = match rest {
                [dashes, words @ ..] if dashes == "--" => words,
                words => words,
            };
            if words.is_empty() {
                bail!(USAGE);
            }
            ("bash".to_string(), json!({ "command": words.join(" ") }))
        }
    };
    let root = std::env::current_dir()?;
    let settings = Settings::load(&root)?;
    let policy = Policy::load(&settings.permissions);
    // The built-in tools know their default; anything else (an MCP tool)
    // asks.
    let needs_approval = tools::builtin_registry()
        .get(&tool)
        .is_none_or(|t| t.requires_approval_in(&call_args, &root));
    for line in explain(&policy, &tool, &call_args, needs_approval, &root) {
        println!("{line}");
    }
    Ok(())
}

/// The lines `permissions check` prints.
fn explain(
    policy: &Policy,
    tool: &str,
    args: &Value,
    needs_approval: bool,
    root: &Path,
) -> Vec<String> {
    let verdict = policy.decide(tool, args, root);
    let decision = match verdict.decision {
        Decision::Deny => "deny",
        Decision::Ask => "ask",
        Decision::Allow => "allow",
        Decision::Default if needs_approval => "ask",
        Decision::Default => "allow",
    };
    let mut out = vec![format!("decision: {decision}")];
    out.push(match &verdict.by {
        Some(by) => format!("rule:     {} ({})", by.rule, by.source.describe(root)),
        None if verdict.decision == Decision::Ask => {
            "rule:     none — the command cannot be followed".to_string()
        }
        None if needs_approval => format!("rule:     none — `{tool}` asks by default"),
        None => format!("rule:     none — `{tool}` runs without asking by default"),
    });
    out.extend(verdict.notes.iter().map(|n| format!("  {n}")));
    out.extend(policy.problems.iter().map(|p| format!("! {p}")));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/work/proj";
    const HOME: &str = "/home/me";

    fn perms(allow: &[&str], ask: &[&str], deny: &[&str]) -> Permissions {
        let list = |l: &[&str]| l.iter().map(|s| s.to_string()).collect();
        Permissions {
            allow: list(allow),
            ask: list(ask),
            deny: list(deny),
        }
    }

    fn policy(allow: &[&str], ask: &[&str], deny: &[&str]) -> Policy {
        Policy::new(
            &perms(allow, ask, deny),
            &Permissions::default(),
            Some(PathBuf::from(HOME)),
        )
    }

    fn bash(policy: &Policy, command: &str) -> Decision {
        policy
            .decide("bash", &json!({ "command": command }), Path::new(ROOT))
            .decision
    }

    fn file(policy: &Policy, tool: &str, path: &str) -> Decision {
        policy
            .decide(tool, &json!({ "path": path }), Path::new(ROOT))
            .decision
    }

    fn fetch(policy: &Policy, url: &str) -> Decision {
        policy
            .decide("web_fetch", &json!({ "url": url }), Path::new(ROOT))
            .decision
    }

    use Decision::{Allow, Ask, Default as NoRule, Deny};

    // ── rule syntax ─────────────────────────────────────────────────────

    #[test]
    fn parses_rules() {
        let r = Rule::parse("bash").unwrap();
        assert_eq!((r.tool.as_str(), r.pattern), ("bash", None));
        let r = Rule::parse(" bash( git diff * ) ").unwrap();
        assert_eq!(r.tool, "bash");
        assert_eq!(r.pattern.as_deref(), Some("git diff *"));
        let r = Rule::parse("bash(echo (x) *)").unwrap();
        assert_eq!(r.pattern.as_deref(), Some("echo (x) *"));
        assert_eq!(Rule::parse("mcp__github__*").unwrap().pattern, None);
        // `tool(*)` is the whole tool.
        assert_eq!(Rule::parse("bash(*)").unwrap().pattern, None);
    }

    #[test]
    fn rejects_malformed_rules() {
        for bad in [
            "",
            "(x)",
            "bash(",
            "bash()",
            "bash(git diff",
            "bash(x) y",
            "bash git diff",
            "ba)sh",
            "read([)",
        ] {
            assert!(Rule::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn malformed_rules_are_reported_not_applied() {
        let p = policy(&["bash(git"], &[], &["bash()"]);
        assert_eq!(p.problems.len(), 2);
        assert_eq!(bash(&p, "git status"), NoRule);
    }

    #[test]
    fn wildcard_matching() {
        assert!(wild("", ""));
        assert!(wild("*", ""));
        assert!(wild("*", "anything at all"));
        assert!(wild("a*c", "abc"));
        assert!(wild("a*c", "ac"));
        assert!(wild("a*b*c", "a-b-b-c"));
        assert!(wild("*.rs", "main.rs"));
        assert!(wild("mcp__github__*", "mcp__github__create_issue"));
        assert!(!wild("a*c", "abd"));
        assert!(!wild("abc", "abcd"));
        assert!(!wild("abc", "ab"));
        assert!(!wild("", "a"));
        assert!(!wild("mcp__github__*", "mcp__gitlab__x"));
    }

    // ── bash patterns ───────────────────────────────────────────────────

    #[test]
    fn trailing_star_is_a_word_boundary() {
        for pattern in ["git diff *", "git diff*", "git  diff   *"] {
            assert!(bash_match(pattern, "git diff"), "{pattern}");
            assert!(bash_match(pattern, "git diff --stat HEAD~1"), "{pattern}");
            assert!(!bash_match(pattern, "git diffx"), "{pattern}");
            assert!(!bash_match(pattern, "git difftool"), "{pattern}");
            assert!(!bash_match(pattern, "git"), "{pattern}");
            assert!(!bash_match(pattern, "xgit diff"), "{pattern}");
        }
    }

    #[test]
    fn pattern_without_trailing_star_is_exact() {
        assert!(bash_match("git status", "git status"));
        assert!(!bash_match("git status", "git status -s"));
        assert!(!bash_match("git status", "git statusx"));
    }

    #[test]
    fn inner_star_is_a_plain_wildcard() {
        assert!(bash_match("git * --stat", "git diff --stat"));
        assert!(bash_match("git * --stat", "git diff HEAD --stat"));
        assert!(!bash_match("git * --stat", "git diff --stat x"));
        assert!(bash_match(
            "npm run test:* *",
            "npm run test:unit -- --watch"
        ));
        assert!(bash_match("npm run test:* *", "npm run test:unit"));
        assert!(!bash_match("npm run test:* *", "npm run build"));
        // `**` at the end: any tail, word boundary or not.
        assert!(bash_match("./scripts/**", "./scripts/build.sh -v"));
        assert!(!bash_match("./scripts/*", "./scripts/build.sh"));
        assert!(bash_match("*", "anything"));
    }

    #[test]
    fn bare_tool_in_allow_keeps_working() {
        let p = policy(&["bash", "mcp__srv__tool"], &[], &[]);
        for cmd in [
            "ls",
            "git diff; rm -rf ~",
            "cat <<EOF\nx\nEOF",
            "echo 'unbalanced",
            "",
        ] {
            assert_eq!(bash(&p, cmd), Allow, "{cmd:?}");
        }
        let root = Path::new(ROOT);
        assert_eq!(
            p.decide("mcp__srv__tool", &json!({"a": 1}), root).decision,
            Allow
        );
        assert_eq!(
            p.decide("mcp__srv__other", &json!({}), root).decision,
            NoRule
        );
        assert_eq!(
            bash(&policy(&["bash(*)"], &[], &[]), "anything | at all"),
            Allow
        );
    }

    #[test]
    fn allow_rule_matches_a_simple_command() {
        let p = policy(&["bash(git diff *)", "bash(git status)"], &[], &[]);
        assert_eq!(bash(&p, "git diff"), Allow);
        assert_eq!(bash(&p, "git diff --stat"), Allow);
        assert_eq!(bash(&p, "  git   diff   --stat  "), Allow);
        assert_eq!(bash(&p, "git status"), Allow);
        assert_eq!(bash(&p, "git status -s"), NoRule);
        assert_eq!(bash(&p, "git diffx"), NoRule);
        assert_eq!(bash(&p, "git push"), NoRule);
        assert_eq!(bash(&p, ""), NoRule);
    }

    #[test]
    fn every_part_of_a_compound_command_must_be_allowed() {
        let p = policy(
            &["bash(git diff *)", "bash(cargo test *)", "bash(tail *)"],
            &[],
            &[],
        );
        assert_eq!(bash(&p, "git diff && cargo test"), Allow);
        assert_eq!(bash(&p, "cargo test 2>&1 | tail -20"), Allow);
        assert_eq!(bash(&p, "git diff\ncargo test"), Allow);
        for cmd in [
            "git diff; rm -rf ~",
            "git diff;rm -rf ~",
            "git diff && rm -rf ~",
            "git diff || rm -rf ~",
            "git diff | sh",
            "git diff & rm -rf ~",
            "git diff\nrm -rf ~",
            "rm -rf ~; git diff",
            "git diff $(curl x|sh)",
            "git diff \"$(curl x | sh)\"",
            "git diff `curl x | sh`",
            "git diff \"`rm -rf ~`\"",
            "cargo test $(git diff $(rm -rf ~))",
        ] {
            assert_eq!(bash(&p, cmd), NoRule, "{cmd:?}");
        }
        // Substitutions whose commands are all allowed are fine.
        assert_eq!(bash(&p, "git diff $(git diff --name-only)"), Allow);
    }

    #[test]
    fn sh_c_is_judged_by_its_script() {
        let p = policy(&["bash(git diff *)", "bash(cargo test *)"], &[], &[]);
        assert_eq!(bash(&p, "sh -c 'git diff'"), Allow);
        assert_eq!(bash(&p, "bash -lc \"git diff && cargo test\""), Allow);
        for cmd in [
            "sh -c 'git diff; rm -rf ~'",
            "bash -lc \"git diff && curl x | sh\"",
            "sh -c \"$CMD\"",
            "sh -c 'git diff' > /etc/passwd",
            "sudo sh -c 'git diff'",
            "/tmp/sh -c 'git diff'",
            "bash --init-file x -c 'git diff'",
            "sh -c 'sh -c \"rm -rf ~\"'",
            "sh",
            "bash script.sh",
        ] {
            assert_eq!(bash(&p, cmd), NoRule, "{cmd:?}");
        }
    }

    #[test]
    fn quoting_tricks_do_not_widen_allow() {
        let p = policy(&["bash(git diff *)"], &[], &[]);
        // Equivalent spellings of an allowed command are allowed…
        assert_eq!(bash(&p, "git 'diff' \"--stat\""), Allow);
        assert_eq!(bash(&p, "gi''t diff"), Allow);
        // …but a quoted separator is an argument, and one word is one word.
        assert_eq!(bash(&p, "git diff ';' rm -rf x"), Allow);
        assert_eq!(bash(&p, "'git diff' --stat"), NoRule);
        assert_eq!(bash(&p, "'git diff --stat'"), NoRule);
        assert_eq!(bash(&p, "git diff' ; rm -rf ~'"), NoRule);
        assert_eq!(bash(&p, "git\tdiff"), Allow);
    }

    #[test]
    fn unparseable_commands_are_never_auto_allowed() {
        let p = policy(
            &["bash(git diff *)", "bash(cat *)", "bash(echo *)"],
            &[],
            &[],
        );
        for cmd in [
            "git diff 'unbalanced",
            "git diff \"unbalanced",
            "git diff \\\n; rm -rf ~",
            "cat <<EOF\n$(rm -rf ~)\nEOF",
            "cat <(rm -rf ~)",
            "echo $((1+1))",
            "(git diff)",
            "git diff # x",
            "echo $'\\x41'",
            "echo ${x:-`rm -rf ~`}",
            "echo hi > \"$f\"",
        ] {
            let v = p.decide("bash", &json!({ "command": cmd }), Path::new(ROOT));
            assert_eq!(v.decision, Ask, "{cmd:?}");
            assert!(v.by.is_none() && !v.notes.is_empty(), "{cmd:?}");
        }
    }

    #[test]
    fn writing_redirections_are_never_auto_allowed() {
        let p = policy(&["bash(echo *)", "bash(cargo test *)"], &[], &[]);
        assert_eq!(bash(&p, "echo hi"), Allow);
        assert_eq!(bash(&p, "cargo test 2>&1"), Allow);
        assert_eq!(bash(&p, "cargo test >/dev/null 2>&1"), Allow);
        for cmd in [
            "echo x > ~/.bashrc",
            "echo x >> ~/.bashrc",
            "echo x>.git/hooks/pre-commit",
            "echo x &> out",
            "echo x 2> out",
            "echo x >| out",
            "cargo test > /dev/null; echo x > out",
        ] {
            assert_eq!(bash(&p, cmd), NoRule, "{cmd:?}");
        }
    }

    #[test]
    fn environment_assignments_are_not_looked_through_for_allow() {
        let p = policy(
            &["bash(git diff *)", "bash(RUST_BACKTRACE=1 cargo test *)"],
            &[],
            &[],
        );
        assert_eq!(bash(&p, "GIT_EXTERNAL_DIFF='rm -rf ~' git diff"), NoRule);
        assert_eq!(bash(&p, "PATH=/tmp/evil git diff"), NoRule);
        assert_eq!(bash(&p, "PATH=/tmp/evil; git diff"), NoRule);
        // Unless the rule spells the assignment out.
        assert_eq!(bash(&p, "RUST_BACKTRACE=1 cargo test --lib"), Allow);
        assert_eq!(bash(&p, "RUST_BACKTRACE=full cargo test"), NoRule);
    }

    #[test]
    fn only_transparent_wrappers_are_looked_through_for_allow() {
        let p = policy(&["bash(git diff *)"], &[], &[]);
        for cmd in [
            "time git diff",
            "nohup git diff",
            "command git diff",
            "exec git diff",
            "builtin command git diff",
        ] {
            assert_eq!(bash(&p, cmd), Allow, "{cmd:?}");
        }
        for cmd in [
            "sudo git diff",
            "env git diff",
            "env -S 'rm -rf ~' git diff",
            "xargs git diff",
            "nice git diff",
            "timeout 5 git diff",
            "command -p git diff",
            "exec -a x git diff",
            "/tmp/evil/git diff",
            "./git diff",
            "$GIT diff",
            "time $X git diff",
        ] {
            assert_eq!(bash(&p, cmd), NoRule, "{cmd:?}");
        }
    }

    #[test]
    fn control_flow_is_never_auto_allowed() {
        let p = policy(&["bash(git diff *)", "bash(true)"], &[], &[]);
        for cmd in [
            "if true; then git diff; fi",
            "while true; do git diff; done",
            "for f in a b; do git diff; done",
            "! git diff",
            "{ git diff; }",
        ] {
            assert_eq!(bash(&p, cmd), NoRule, "{cmd:?}");
        }
    }

    #[test]
    fn deny_wins_and_looks_everywhere() {
        let p = policy(&["bash"], &[], &["bash(rm *)", "bash(git push --force *)"]);
        assert_eq!(bash(&p, "ls"), Allow);
        assert_eq!(bash(&p, "rmdir x"), Allow);
        assert_eq!(bash(&p, "git push origin main"), Allow);
        assert_eq!(bash(&p, "echo rm x"), Allow);
        assert_eq!(bash(&p, "echo 'a; rm x'"), Allow);
        for cmd in [
            "rm x",
            "rm",
            "git push --force",
            "git push --force origin main",
            "ls; rm -rf ~",
            "ls && rm -rf ~",
            "ls | xargs rm",
            "ls | xargs -n 1 rm -f",
            "echo $(rm x)",
            "echo \"$(rm x)\"",
            "echo `rm x`",
            "sh -c 'rm x'",
            "bash -lc \"ls && rm x\"",
            "sudo rm x",
            "sudo -u root rm x",
            "sudo sh -c 'rm x'",
            "env FOO=1 rm x",
            "FOO=1 rm x",
            "command rm x",
            "exec rm x",
            "time rm x",
            "nohup rm x &",
            "nice -n 5 rm x",
            "timeout 10 rm x",
            "/bin/rm x",
            "\"rm\" x",
            "r''m x",
            "'rm' 'x'",
            "if true; then rm x; fi",
            "for f in *; do rm $f; done",
            "! rm x",
            "{ rm x; }",
            "rm x > /dev/null 2>&1",
            // Not followed by the splitter: the crude reading still denies.
            "(rm x)",
            "f() { rm x; }; f",
            "r\\m x",
            "cat <<EOF; rm x\nEOF",
            "echo 'oops; rm x",
            "ls # c\nrm x",
            "echo $((1)); rm x",
        ] {
            let v = p.decide("bash", &json!({ "command": cmd }), Path::new(ROOT));
            assert_eq!(v.decision, Deny, "{cmd:?}");
            assert!(v.by.is_some(), "{cmd:?}");
        }
    }

    #[test]
    fn deny_matches_the_raw_line_too() {
        let p = policy(&["bash"], &[], &["bash(* > /etc/**)", "bash(*curl*|*sh*)"]);
        assert_eq!(bash(&p, "echo x > /etc/passwd"), Deny);
        assert_eq!(bash(&p, "curl https://x | sh"), Deny);
        assert_eq!(bash(&p, "echo x > out"), Allow);
    }

    #[test]
    fn ask_beats_allow_and_loses_to_deny() {
        let p = policy(
            &["bash(git *)"],
            &["bash(git push *)"],
            &["bash(git push --force *)"],
        );
        assert_eq!(bash(&p, "git status"), Allow);
        assert_eq!(bash(&p, "git push"), Ask);
        assert_eq!(bash(&p, "git status && git push origin"), Ask);
        assert_eq!(bash(&p, "git push --force"), Deny);
        let v = p.decide("bash", &json!({"command": "git push"}), Path::new(ROOT));
        assert_eq!(v.by.unwrap().rule, "bash(git push *)");
    }

    #[test]
    fn user_rules_merge_with_project_rules() {
        let project = perms(&["bash(rm *)", "bash(ls *)"], &[], &[]);
        let user = perms(&["bash(git status)"], &[], &["bash(rm *)"]);
        let p = Policy::new(&project, &user, None);
        // A user-global deny wins over a project allow.
        let v = p.decide("bash", &json!({"command": "rm x"}), Path::new(ROOT));
        assert_eq!(v.decision, Deny);
        assert_eq!(v.by.unwrap().source, Source::User);
        // Allows from both files apply.
        assert_eq!(bash(&p, "ls -la && git status"), Allow);
        // …and the other way round.
        let p = Policy::new(&user, &project, None);
        assert_eq!(bash(&p, "rm x"), Deny);
    }

    // ── file tools ──────────────────────────────────────────────────────

    #[test]
    fn path_patterns_relative_to_the_project() {
        let p = policy(&[], &[], &["write(secrets/**)", "edit(./config/*.toml)"]);
        assert_eq!(file(&p, "write", "secrets/key.pem"), Deny);
        assert_eq!(file(&p, "write", "secrets/a/b/c"), Deny);
        assert_eq!(file(&p, "write", "/work/proj/secrets/x"), Deny);
        assert_eq!(file(&p, "write", "src/secrets/x"), NoRule);
        assert_eq!(file(&p, "write", "/elsewhere/secrets/x"), NoRule);
        assert_eq!(file(&p, "edit", "config/app.toml"), Deny);
        // `*` stays within one path component.
        assert_eq!(file(&p, "edit", "config/sub/app.toml"), NoRule);
        // The rule names another tool.
        assert_eq!(file(&p, "read", "secrets/key.pem"), NoRule);
    }

    #[test]
    fn path_traversal_is_judged_by_where_it_lands() {
        let p = policy(
            &["read(docs/**)"],
            &[],
            &["read(/etc/**)", "read(src/secret.rs)"],
        );
        assert_eq!(file(&p, "read", "./a/../../../etc/passwd"), Deny);
        assert_eq!(file(&p, "read", "/work/proj/../../etc/shadow"), Deny);
        assert_eq!(file(&p, "read", "docs/../src/secret.rs"), Deny);
        assert_eq!(file(&p, "read", "src/./x/../secret.rs"), Deny);
        assert_eq!(file(&p, "read", "docs/../../other/docs/x"), NoRule);
        assert_eq!(file(&p, "read", "docs/a/../b.md"), Allow);
        assert_eq!(file(&p, "read", "docs/../README.md"), NoRule);
    }

    #[test]
    fn path_normalization_agrees_with_confine() {
        let root = Path::new(ROOT);
        for path in ["a/b", "./a/../b", "a/../../x", "/abs/../y", "a//b/./c"] {
            let ours = absolute(root, path);
            match tools::confine(root, path) {
                Ok(confined) => assert_eq!(ours, confined, "{path}"),
                Err(_) => assert!(!ours.starts_with(root), "{path}"),
            }
        }
    }

    #[test]
    fn bare_file_name_matches_in_any_directory() {
        let p = policy(&[], &["read(.env*)"], &["read(*.pem)"]);
        assert_eq!(file(&p, "read", ".env"), Ask);
        assert_eq!(file(&p, "read", ".env.local"), Ask);
        assert_eq!(file(&p, "read", "services/api/.env"), Ask);
        assert_eq!(file(&p, "read", "/somewhere/else/.env.prod"), Ask);
        assert_eq!(file(&p, "read", "src/env.rs"), NoRule);
        assert_eq!(file(&p, "read", "deep/dir/key.pem"), Deny);
        assert_eq!(file(&p, "read", "key.pem.txt"), NoRule);
    }

    #[test]
    fn home_and_absolute_patterns() {
        let p = policy(
            &["read(~/notes/**)", "read(/usr/share/doc)"],
            &[],
            &["read(~/.ssh/**)"],
        );
        assert_eq!(file(&p, "read", "/home/me/.ssh/id_rsa"), Deny);
        assert_eq!(file(&p, "read", "/home/me/notes/../.ssh/id_rsa"), Deny);
        assert_eq!(file(&p, "read", "/home/me/notes/todo.md"), Allow);
        assert_eq!(file(&p, "read", "/home/other/notes/todo.md"), NoRule);
        // A pattern naming a directory covers what is under it.
        assert_eq!(file(&p, "read", "/usr/share/doc/x/README"), Allow);
        assert_eq!(file(&p, "read", "/usr/share/doc"), Allow);
        assert_eq!(file(&p, "read", "/usr/share/docs"), NoRule);
        // `~` in an *argument* is not the home directory (the tools do not
        // expand it either).
        assert_eq!(file(&p, "read", "~/.ssh/id_rsa"), NoRule);
        // Without a home directory, `~/` patterns match nothing.
        let homeless = Policy::new(
            &perms(&["read(~/notes/**)"], &[], &[]),
            &Permissions::default(),
            None,
        );
        assert_eq!(file(&homeless, "read", "/home/me/notes/x"), NoRule);
    }

    #[test]
    fn parent_relative_patterns_resolve() {
        let p = policy(&["read(../sibling/**)"], &[], &[]);
        assert_eq!(file(&p, "read", "/work/sibling/a.rs"), Allow);
        assert_eq!(file(&p, "read", "../sibling/a.rs"), Allow);
        assert_eq!(file(&p, "read", "/work/proj/sibling/a.rs"), NoRule);
    }

    #[test]
    fn glob_characters_in_the_root_are_literal() {
        let root = Path::new("/work/pr[o]j*");
        let p = policy(&["read(src/**)"], &[], &[]);
        let at = |path: &str| p.decide("read", &json!({ "path": path }), root).decision;
        assert_eq!(at("src/a.rs"), Allow);
        assert_eq!(at("/work/proj/src/a.rs"), NoRule);
    }

    #[test]
    fn path_argument_aliases_and_missing_paths() {
        let p = policy(&["read(docs/**)"], &[], &["notebook_edit(*.ipynb)"]);
        let root = Path::new(ROOT);
        let d = |tool: &str, args: Value| p.decide(tool, &args, root).decision;
        assert_eq!(d("read", json!({"file_path": "docs/a.md"})), Allow);
        assert_eq!(
            d("notebook_edit", json!({"notebook_path": "n.ipynb"})),
            Deny
        );
        // No path: a pattern rule cannot vouch for the call.
        assert_eq!(d("read", json!({})), NoRule);
        assert_eq!(d("read", Value::Null), NoRule);
    }

    #[test]
    fn apply_patch_is_judged_on_every_path() {
        let patch = "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-x\n+y\n\
                     --- /dev/null\n+++ b/secrets/new.txt\n@@ -0,0 +1 @@\n+z\n";
        let args = json!({ "patch": patch });
        let root = Path::new(ROOT);
        let d = |p: &Policy| p.decide("apply_patch", &args, root).decision;
        assert_eq!(d(&policy(&[], &[], &["apply_patch(secrets/**)"])), Deny);
        assert_eq!(d(&policy(&[], &["apply_patch(*.txt)"], &[])), Ask);
        // Allow needs every path covered.
        assert_eq!(d(&policy(&["apply_patch(src/**)"], &[], &[])), NoRule);
        assert_eq!(
            d(&policy(
                &["apply_patch(src/**)", "apply_patch(secrets/**)"],
                &[],
                &[]
            )),
            Allow
        );
        assert_eq!(d(&policy(&["apply_patch"], &[], &[])), Allow);
    }

    // ── web_fetch and other tools ───────────────────────────────────────

    #[test]
    fn url_hosts() {
        let host = |u: &str| url_host(u);
        assert_eq!(
            host("https://Example.com/a?b#c").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            host("http://example.com:8080").as_deref(),
            Some("example.com")
        );
        assert_eq!(host("https://example.com.").as_deref(), Some("example.com"));
        assert_eq!(host("https://[::1]:80/x").as_deref(), Some("::1"));
        // Userinfo is not the host.
        assert_eq!(
            host("https://example.com@evil.test/").as_deref(),
            Some("evil.test")
        );
        assert_eq!(
            host("https://example.com:pw@evil.test:443/").as_deref(),
            Some("evil.test")
        );
        assert_eq!(
            host("https://evil.test/?u=https://example.com/").as_deref(),
            Some("evil.test")
        );
        assert_eq!(
            host("https://evil.test#@example.com").as_deref(),
            Some("evil.test")
        );
        // Not readable with certainty.
        assert_eq!(host("https://evil.test\\@example.com/"), None);
        assert_eq!(host("example.com/path"), None);
        assert_eq!(host("https:///path"), None);
        assert_eq!(host("https://exa mple.com/"), None);
    }

    #[test]
    fn web_fetch_domain_rules() {
        let p = policy(
            &[
                "web_fetch(domain:example.com)",
                "web_fetch(https://docs.rs/*)",
            ],
            &[],
            &[
                "web_fetch(domain:internal.example.com)",
                "web_fetch(domain:*.corp)",
            ],
        );
        assert_eq!(fetch(&p, "https://example.com/x"), Allow);
        assert_eq!(fetch(&p, "https://api.example.com/x"), Allow);
        assert_eq!(fetch(&p, "https://EXAMPLE.com/x"), Allow);
        assert_eq!(fetch(&p, "https://docs.rs/serde"), Allow);
        assert_eq!(fetch(&p, "https://internal.example.com/x"), Deny);
        assert_eq!(fetch(&p, "https://a.internal.example.com/x"), Deny);
        assert_eq!(fetch(&p, "https://wiki.corp/x"), Deny);
        for url in [
            "https://notexample.com/",
            "https://example.com.evil.test/",
            "https://example.com@evil.test/",
            "https://evil.test/example.com",
            "https://evil.test/?https://docs.rs/",
            "https://docs.rs.evil.test/",
            "http://docs.rs/serde",
            "not a url",
        ] {
            assert_eq!(fetch(&p, url), NoRule, "{url}");
        }
    }

    #[test]
    fn other_tools_match_on_tool_name_and_json() {
        let p = policy(
            &["mcp__github__*", "mcp__db__query(*\"sql\":\"SELECT *)"],
            &[],
            &["mcp__github__delete_*", "mcp__*__query(*DROP *)"],
        );
        let root = Path::new(ROOT);
        let d = |tool: &str, args: Value| p.decide(tool, &args, root).decision;
        assert_eq!(d("mcp__github__list_issues", json!({})), Allow);
        assert_eq!(d("mcp__github__delete_repo", json!({})), Deny);
        assert_eq!(d("mcp__gitlab__list", json!({})), NoRule);
        assert_eq!(d("mcp__db__query", json!({"sql": "SELECT 1"})), Allow);
        assert_eq!(d("mcp__db__query", json!({"sql": "UPDATE t"})), NoRule);
        assert_eq!(
            d("mcp__db__query", json!({"sql": "SELECT 1; DROP TABLE t"})),
            Deny
        );
        assert_eq!(d("mcp__other__query", json!({"q": "DROP x"})), Deny);
    }

    // ── "always" ────────────────────────────────────────────────────────

    fn proposed(command: &str) -> Vec<String> {
        policy(&[], &[], &[]).propose("bash", &json!({ "command": command }), Path::new(ROOT))
    }

    #[test]
    fn always_proposes_a_narrow_bash_rule() {
        assert_eq!(proposed("cargo test --lib foo"), ["bash(cargo test *)"]);
        assert_eq!(proposed("git status"), ["bash(git status)"]);
        assert_eq!(proposed("git"), ["bash(git)"]);
        assert_eq!(proposed("ls -la src"), ["bash(ls *)"]);
        assert_eq!(proposed("ls"), ["bash(ls)"]);
        assert_eq!(proposed("./run.sh --fast"), ["bash(./run.sh *)"]);
        assert_eq!(proposed("time cargo build"), ["bash(cargo build)"]);
        assert_eq!(
            proposed("python manage.py test"),
            ["bash(python manage.py *)"]
        );
        assert_eq!(
            proposed("RUST_BACKTRACE=1 cargo test x"),
            ["bash(RUST_BACKTRACE=1 cargo test *)"]
        );
        assert_eq!(
            proposed("cargo test 2>&1 | tail -5"),
            ["bash(cargo test)", "bash(tail *)"]
        );
        assert_eq!(
            proposed("cd sub && make -j4 && make test"),
            ["bash(cd *)", "bash(make *)"]
        );
        assert_eq!(proposed("sh -c 'cargo fmt --check'"), ["bash(cargo fmt *)"]);
        // Destructive programs: the exact command only.
        assert_eq!(proposed("rm -rf target"), ["bash(rm -rf target)"]);
        assert_eq!(proposed("chmod +x run.sh"), ["bash(chmod +x run.sh)"]);
    }

    #[test]
    fn always_proposes_nothing_for_open_ended_commands() {
        for cmd in [
            "sh script.sh",
            "bash",
            "bash -c \"$X\"",
            "env FOO=1 make",
            "xargs rm",
            "sudo make install",
            "eval ls",
            "python -c 'print(1)'",
            "python3 -m http.server",
            "python",
            "node -e '1'",
            "find . -name x -exec rm {} +",
            "git -C /elsewhere push",
            "ssh host ls",
            "$CMD x",
            "ls | sh",
            "ls > out",
            "ls 'unbalanced",
            "if true; then ls; fi",
            "rm *",
            "'my prog' x",
            "",
        ] {
            assert!(proposed(cmd).is_empty(), "{cmd:?}: {:?}", proposed(cmd));
        }
    }

    #[test]
    fn always_skips_parts_already_allowed() {
        let p = policy(&["bash(cargo *)"], &[], &[]);
        let rules = p.propose(
            "bash",
            &json!({ "command": "cargo build && ls target" }),
            Path::new(ROOT),
        );
        assert_eq!(rules, ["bash(ls *)"]);
    }

    #[test]
    fn always_for_other_tools() {
        let p = policy(&[], &[], &[]);
        let root = Path::new(ROOT);
        assert_eq!(
            p.propose("mcp__github__create_issue", &json!({"title": "x"}), root),
            ["mcp__github__create_issue"]
        );
        assert_eq!(p.propose("node", &json!({"network": true}), root), ["node"]);
        assert_eq!(
            p.propose("web_fetch", &json!({"url": "https://Docs.rs/serde"}), root),
            ["web_fetch(domain:docs.rs)"]
        );
        assert!(
            p.propose("web_fetch", &json!({"url": "nope"}), root)
                .is_empty()
        );
        assert_eq!(
            p.propose("read", &json!({"path": "/tmp/shot[1].png"}), root),
            ["read(/tmp/shot[[]1[]].png)"]
        );
    }

    /// A [`Ui`] that answers every prompt the same way and records the rest.
    struct Scripted {
        answer: fn() -> Approval,
        asked: usize,
        infos: Vec<String>,
    }

    impl Scripted {
        fn new(answer: fn() -> Approval) -> Self {
            Self {
                answer,
                asked: 0,
                infos: Vec::new(),
            }
        }
    }

    impl Ui for Scripted {
        fn reasoning(&mut self, _: &str) {}
        fn content(&mut self, _: &str) {}
        fn tool_start(&mut self, _: &str, _: &str) {}
        fn tool_end(&mut self, _: &str, _: &str, _: bool) {}
        fn info(&mut self, text: &str) {
            self.infos.push(text.to_string());
        }
        fn ask_approval(&mut self, _: &str, _: &str) -> Approval {
            self.asked += 1;
            (self.answer)()
        }
        fn notice(&mut self, _: &str) {}
    }

    /// Run one call through [`approve_with`] in a scratch project.
    struct Gate {
        dir: PathBuf,
        settings: Settings,
        auto_approve: bool,
    }

    impl Gate {
        fn new(name: &str, permissions: Permissions) -> Self {
            let dir =
                std::env::temp_dir().join(format!("atelier-perm-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self {
                dir,
                settings: Settings {
                    permissions,
                    ..Settings::default()
                },
                auto_approve: false,
            }
        }

        fn call(
            &mut self,
            tool: &str,
            args: Value,
            needs_approval: bool,
            ui: &mut Scripted,
        ) -> Result<(), String> {
            let policy = Policy::new(
                &self.settings.permissions,
                &Permissions::default(),
                Some(PathBuf::from(HOME)),
            );
            let arguments = args.to_string();
            let req = Request {
                tool,
                arguments: &arguments,
                needs_approval,
            };
            approve_with(
                &policy,
                &mut self.settings,
                &self.dir,
                self.auto_approve,
                req,
                ui,
            )
        }

        fn bash(&mut self, command: &str, ui: &mut Scripted) -> Result<(), String> {
            self.call("bash", json!({ "command": command }), true, ui)
        }
    }

    impl Drop for Gate {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn deny_refuses_without_asking_and_names_the_rule() {
        let mut gate = Gate::new("deny", perms(&["bash"], &[], &["bash(rm *)"]));
        let mut ui = Scripted::new(|| Approval::Once);
        let err = gate.bash("ls; rm -rf x", &mut ui).unwrap_err();
        assert!(err.contains("`bash(rm *)`"), "{err}");
        assert_eq!(ui.asked, 0);
        // …even with every prompt auto-approved.
        gate.auto_approve = true;
        assert!(gate.bash("rm x", &mut ui).is_err());
        assert!(gate.bash("ls", &mut ui).is_ok());
        assert_eq!(ui.asked, 0);
    }

    #[test]
    fn ask_rule_prompts_for_a_tool_that_would_not() {
        let mut gate = Gate::new("ask", perms(&[], &["read(.env*)"], &[]));
        let mut ui = Scripted::new(|| Approval::Deny);
        // `read` inside the project needs no approval by default…
        assert!(
            gate.call("read", json!({"path": "src/a.rs"}), false, &mut ui)
                .is_ok()
        );
        assert_eq!(ui.asked, 0);
        // …but the ask rule prompts, and the user's no is final.
        let err = gate
            .call("read", json!({"path": ".env"}), false, &mut ui)
            .unwrap_err();
        assert_eq!(err, DENIED);
        assert_eq!(ui.asked, 1);
        assert!(ui.infos.iter().any(|i| i.contains("read(.env*)")));
        // Auto-approve answers the prompt.
        gate.auto_approve = true;
        assert!(
            gate.call("read", json!({"path": ".env"}), false, &mut ui)
                .is_ok()
        );
        assert_eq!(ui.asked, 1);
    }

    #[test]
    fn always_on_an_ask_rule_saves_nothing() {
        let mut gate = Gate::new("ask-always", perms(&[], &["bash(git push *)"], &[]));
        let mut ui = Scripted::new(|| Approval::Always);
        assert!(gate.bash("git push", &mut ui).is_ok());
        assert!(gate.settings.permissions.allow.is_empty());
        assert!(gate.bash("git push", &mut ui).is_ok());
        assert_eq!(ui.asked, 2);
    }

    #[test]
    fn always_saves_a_narrow_rule_and_stops_asking() {
        let mut gate = Gate::new("always", Permissions::default());
        let mut ui = Scripted::new(|| Approval::Always);
        assert!(gate.bash("cargo test --lib", &mut ui).is_ok());
        assert_eq!(gate.settings.permissions.allow, ["bash(cargo test *)"]);
        assert!(
            ui.infos.iter().any(|i| i.contains("`bash(cargo test *)`")),
            "{:?}",
            ui.infos
        );
        // Persisted, and reloads as written.
        let saved = Settings::load(&gate.dir).unwrap();
        assert_eq!(saved.permissions.allow, ["bash(cargo test *)"]);
        assert!(saved.permissions.deny.is_empty());
        // The same kind of call no longer asks; a different one does.
        assert!(gate.bash("cargo test other", &mut ui).is_ok());
        assert_eq!(ui.asked, 1);
        assert!(gate.bash("cargo test; cargo publish", &mut ui).is_ok());
        assert_eq!(ui.asked, 2);
        assert_eq!(
            gate.settings.permissions.allow,
            ["bash(cargo test *)", "bash(cargo publish)"]
        );
    }

    #[test]
    fn always_on_a_launcher_is_once() {
        let mut gate = Gate::new("launcher", Permissions::default());
        let mut ui = Scripted::new(|| Approval::Always);
        for cmd in [
            "sh -c \"$X\"",
            "sudo make install",
            "python -c 'x'",
            "ls > out",
            "(ls)",
        ] {
            assert!(gate.bash(cmd, &mut ui).is_ok(), "{cmd}");
            assert!(gate.bash(cmd, &mut ui).is_ok(), "{cmd}");
        }
        assert_eq!(ui.asked, 10);
        assert!(gate.settings.permissions.allow.is_empty());
        assert!(ui.infos.iter().any(|i| i.contains("allowed once")));
        assert!(!Settings::path(&gate.dir).exists());
    }

    #[test]
    fn always_on_an_mcp_tool_saves_its_name() {
        let mut gate = Gate::new("mcp", Permissions::default());
        let mut ui = Scripted::new(|| Approval::Always);
        assert!(
            gate.call("mcp__gh__list", json!({"a": 1}), true, &mut ui)
                .is_ok()
        );
        assert_eq!(gate.settings.permissions.allow, ["mcp__gh__list"]);
        assert!(
            gate.call("mcp__gh__list", json!({"a": 2}), true, &mut ui)
                .is_ok()
        );
        assert_eq!(ui.asked, 1);
        assert!(
            gate.call("mcp__gh__delete", json!({}), true, &mut ui)
                .is_ok()
        );
        assert_eq!(ui.asked, 2);
    }

    #[test]
    fn defaults_apply_when_no_rule_matches() {
        let mut gate = Gate::new("default", perms(&["bash(ls *)"], &[], &[]));
        let mut ui = Scripted::new(|| Approval::Deny);
        assert!(gate.bash("ls -la", &mut ui).is_ok());
        assert_eq!(gate.bash("make", &mut ui).unwrap_err(), DENIED);
        assert_eq!(ui.asked, 1);
        // Malformed arguments are judged as no arguments.
        let req = Request {
            tool: "bash",
            arguments: "{not json",
            needs_approval: true,
        };
        let policy = policy(&["bash(ls *)"], &[], &[]);
        assert!(approve_with(&policy, &mut gate.settings, &gate.dir, false, req, &mut ui).is_err());
        assert_eq!(ui.asked, 2);
    }

    // ── explanations ────────────────────────────────────────────────────

    #[test]
    fn check_explains_the_decision() {
        let p = policy(&["bash(git diff *)"], &["read(.env*)"], &["bash(rm *)"]);
        let root = Path::new(ROOT);
        let run = |tool: &str, args: Value, needs: bool| explain(&p, tool, &args, needs, root);

        let out = run("bash", json!({"command": "git diff --stat"}), true);
        assert_eq!(out[0], "decision: allow");
        assert!(out[1].contains("bash(git diff *)") && out[1].contains("project"));

        let out = run("bash", json!({"command": "git diff; rm -rf ~"}), true);
        assert_eq!(out[0], "decision: deny");
        assert!(out[1].contains("bash(rm *)"));

        let out = run("bash", json!({"command": "git diff && make"}), true);
        assert_eq!(out[0], "decision: ask");
        assert!(
            out.iter()
                .any(|l| l.contains("no allow rule covers `make`"))
        );
        assert!(out.iter().any(|l| l.contains("`git diff` is allowed by")));

        let out = run("bash", json!({"command": "git diff <<EOF"}), true);
        assert_eq!(out[0], "decision: ask");
        assert!(out.iter().any(|l| l.contains("heredoc")), "{out:?}");

        let out = run("read", json!({"path": ".env"}), false);
        assert_eq!(out[0], "decision: ask");
        let out = run("read", json!({"path": "src/a.rs"}), false);
        assert_eq!(out[0], "decision: allow");
        assert!(out[1].contains("by default"));
    }

    #[test]
    fn listing_groups_rules_by_source() {
        let p = Policy::new(
            &perms(&["bash(ls *)"], &[], &["bash(rm *)", "oops("]),
            &perms(&[], &["read(.env*)"], &[]),
            None,
        );
        let lines = p.listing(Path::new(ROOT));
        let text = lines.join("\n");
        let project = text.find("project,").unwrap();
        let user = text.find("user").unwrap();
        let at = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle}: {text}"))
        };
        assert!(project < at("deny  bash(rm *)") && at("deny  bash(rm *)") < user);
        assert!(at("allow bash(ls *)") < user);
        assert!(at("ask   read(.env*)") > user);
        assert!(text.contains("! ignored deny rule `oops(`"));
        assert!(
            policy(&[], &[], &[])
                .listing(Path::new(ROOT))
                .join("\n")
                .contains("(none)")
        );
    }
}
