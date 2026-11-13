//! `atelier review`: a read-only review of a change, as a run of its own.
//!
//! Not the background reviewer of `crate::review` (the "subconscious" that
//! comments on the agent's own work as it goes, toggled by `/review`): this
//! is a command a person or a CI job runs to have a change looked at —
//! the uncommitted work, a branch against its base, or one commit — and get
//! back a list of defects.
//!
//! It is a headless run with three things decided for the caller:
//!
//! - **The prompt.** The diff is gathered here, with git, rather than left
//!   for the model to fetch: the reviewer has no shell (below), and what is
//!   under review should not depend on which commands a model thinks of. A
//!   diff past [`MAX_DIFF_BYTES`] is cut at a file boundary where it can be,
//!   and the prompt names the files that were cut or left out so the
//!   reviewer reads them itself instead of assuming they are fine.
//! - **The tools.** Only those that look ([`agents::read_only`], the built-in
//!   `explore` agent's set): a review that can edit or run things is not
//!   one a CI job can be handed. The session is otherwise the ordinary one
//!   (see `Session::standalone`), speaking from [`REVIEW_PROMPT`] plus
//!   whatever the project's instruction files say under a "Review" heading.
//! - **The answer's shape.** The findings are asked for as JSON, checked
//!   against [`schema`] and asked for once more if they don't fit — the
//!   `--output-schema` mechanism of [`super`] — because both outputs need
//!   them as data: `json` prints them, `text` orders and lays them out.
//!
//! The exit status says whether the review *ran*, not what it found: 0 with
//! or without findings, so a script can tell "nothing wrong" from "could not
//! look". A CI job that wants to fail on findings says from which severity
//! with `--fail-on` (status [`EXIT_FINDINGS`]).

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{EXIT_ERROR, HeadlessUi, run_prompt};
use crate::agent::Session;
use crate::events::truncate;
use crate::{agents, context};

/// Exit status: the review ran and found something at or above `--fail-on`.
pub const EXIT_FINDINGS: u8 = 5;

/// `atelier review --help`, and what follows a usage error.
pub const USAGE: &str = "\
usage: atelier review [--uncommitted | --base BRANCH | --commit SHA]
                      [options] [--] [extra instructions...]

Review a change, read-only, and report the defects found in it.

      --uncommitted          the working tree against HEAD, untracked files
                             included (the default when there are changes)
      --base BRANCH          everything since the branch left BRANCH (the
                             default otherwise: origin's default branch,
                             else main or master)
      --commit SHA           what one commit changed
      --output-format FMT    text (default) or json
      --fail-on SEVERITY     exit 5 when a finding is at least this severe
                             (P0, P1, P2 or P3; P0 is the worst)
      --max-turns N          stop after N model requests

exit status: 0 the review ran (with or without findings), 1 error, 2 usage,
             3 --max-turns reached, 4 the findings were not valid JSON,
             5 a finding at or above --fail-on";

/// The most of a diff put in the prompt, in bytes. Roughly 25k tokens: room
/// for a real change, while leaving the reviewer context to read around it.
const MAX_DIFF_BYTES: usize = 96 * 1024;

/// The reviewer's system prompt.
const REVIEW_PROMPT: &str = "\
You are a code reviewer working inside a project directory. You are given a \
change as a diff. Your job is to find the real defects it introduces.

Report only defects with consequences: correctness bugs, security holes, data \
loss, concurrency errors, misuse of an API. No style or naming remarks, no \
praise, no summary of what the change does, nothing that would merely be \
nicer.

Your tools only look: you can read and search the project, and cannot change \
it or run anything. Use them. A diff shows the changed lines and little \
else, so before you report a defect, open the code around it and the code \
that calls it or that it calls, and check that the failure can really \
happen. Drop every claim you could not confirm against the code.

For each defect, give:
- the file, and the line in the new version of that file, where it is;
- one sentence stating the defect;
- a concrete scenario in which it fails: the input or the sequence of \
events, and what goes wrong;
- a severity: P0 (data loss, a security hole, or broken for everyone — must \
not ship), P1 (wrong behaviour users will hit), P2 (wrong in a case that is \
uncommon or easy to recover from), P3 (minor, but still a defect).

An empty list is a valid answer: if the change holds up, report no findings.";

/// How bad a finding is. Ordered worst first, which is also the order the
/// findings are reported in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    P0,
    P1,
    P2,
    P3,
}

impl Severity {
    const ALL: [Severity; 4] = [Severity::P0, Severity::P1, Severity::P2, Severity::P3];

    fn parse(s: &str) -> Option<Severity> {
        Self::ALL
            .into_iter()
            .find(|v| v.as_str().eq_ignore_ascii_case(s))
    }

    fn as_str(self) -> &'static str {
        match self {
            Severity::P0 => "P0",
            Severity::P1 => "P1",
            Severity::P2 => "P2",
            Severity::P3 => "P3",
        }
    }
}

/// Which change to review.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// The working tree against `HEAD`.
    Uncommitted,
    /// Everything since the current branch left this one.
    Base(String),
    /// What this commit changed.
    Commit(String),
}

/// A review, as asked for on the command line.
#[derive(Debug, PartialEq)]
pub struct Options {
    /// `None`: decided from the state of the repository (see [`Git::target`]).
    target: Option<Target>,
    /// `--output-format json`.
    json: bool,
    /// `--fail-on`.
    fail_on: Option<Severity>,
    /// `--max-turns`.
    max_turns: Option<u32>,
    /// What the user added for the reviewer.
    instructions: Option<String>,
}

/// Parse the arguments after `review`. The error is the message for the user
/// (exit status 2). Same conventions as `cli::parse`: options anywhere,
/// `--flag value` or `--flag=value`, `--` ends them.
pub fn parse(args: &[String]) -> Result<Options, String> {
    let mut opts = Options {
        target: None,
        json: false,
        fail_on: None,
        max_turns: None,
        instructions: None,
    };
    let mut words: Vec<&str> = Vec::new();

    let mut it = args.iter().map(String::as_str);
    while let Some(arg) = it.next() {
        if arg == "--" {
            words.extend(it.by_ref());
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            words.push(arg);
            continue;
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v)),
            _ => (arg, None),
        };
        let mut value = || {
            inline
                .or_else(|| it.next())
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        let target = match flag {
            "--base" => Some(Target::Base(revision(flag, value()?)?)),
            "--commit" => Some(Target::Commit(revision(flag, value()?)?)),
            "--output-format" => {
                opts.json = match value()? {
                    "text" => false,
                    "json" => true,
                    v => {
                        return Err(format!(
                            "--output-format: unknown format '{v}' (text, json)"
                        ));
                    }
                };
                None
            }
            "--fail-on" => {
                let v = value()?;
                opts.fail_on = Some(Severity::parse(v).ok_or_else(|| {
                    format!("--fail-on: unknown severity '{v}' (P0, P1, P2, P3)")
                })?);
                None
            }
            "--max-turns" => {
                let v = value()?;
                opts.max_turns = match v.parse::<u32>() {
                    Ok(n) if n > 0 => Some(n),
                    _ => return Err(format!("{flag}: expected a positive number, got '{v}'")),
                };
                None
            }
            _ if inline.is_some() => return Err(format!("{flag} takes no value")),
            "--uncommitted" => Some(Target::Uncommitted),
            _ => return Err(format!("unknown option {arg}")),
        };
        if let Some(target) = target
            && opts.target.replace(target).is_some()
        {
            return Err("choose one of --uncommitted, --base and --commit".to_string());
        }
    }
    opts.instructions = (!words.is_empty()).then(|| words.join(" "));
    Ok(opts)
}

/// A branch or commit name from the command line. It is handed to git as an
/// argument, where a leading dash would make it an option.
fn revision(flag: &str, v: &str) -> Result<String, String> {
    if v.is_empty() || v.starts_with('-') {
        return Err(format!("{flag}: '{v}' is not a revision"));
    }
    Ok(v.to_string())
}

/// Run the review `opts` describes on `session` and return the exit status.
pub fn run(mut session: Session, opts: Options) -> u8 {
    match review(&mut session, &opts) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e:#}");
            EXIT_ERROR
        }
    }
}

fn review(session: &mut Session, opts: &Options) -> Result<u8> {
    // claude brings its own tools, a shell and an editor among them, which
    // atelier cannot take away: a review there would not be read-only.
    if session.is_claude() {
        bail!(
            "atelier review runs on atelier's own loop, which the Claude backend does not use \
             (unset ATELIER_BACKEND)"
        );
    }
    let root = std::env::current_dir().context("finding the current directory")?;
    let git = Git { root: &root };
    let change = git.change(opts.target.clone())?;
    let shown = cap(&change.files, MAX_DIFF_BYTES);

    let mut reviewed = change.reviewed.clone();
    reviewed["files"] = json!(shown.files);
    reviewed["truncated"] = json!(shown.truncated());
    reviewed["omitted"] = json!(shown.not_shown());

    if change.files.is_empty() {
        eprintln!("nothing to review: {} is empty", change.what);
        print_findings(opts, &[], reviewed);
        return Ok(0);
    }
    eprintln!("reviewing {} ({} file(s))", change.what, shown.files.len());

    let mut system = REVIEW_PROMPT.to_string();
    if let Some(section) = context::review_section(&root) {
        system
            .push_str("\n\nThe project's own review instructions, from its instruction files:\n\n");
        system.push_str(&section);
    }
    session.standalone(system, agents::read_only);
    session.set_max_steps(opts.max_turns);

    // Progress (the reviewer's reading, its answer as written) on stderr;
    // stdout is for the findings.
    let mut ui = HeadlessUi::on_stderr();
    let prompt = prompt(&change, &shown, opts.instructions.as_deref());
    let outcome = run_prompt(session, &mut ui, &prompt, Some(&schema()));
    let Some(answer) = &outcome.structured else {
        if let Some(m) = outcome.end.message() {
            eprintln!("Error: {m}");
        }
        return Ok(outcome.end.exit_code());
    };

    let mut findings: Vec<Finding> = answer["findings"]
        .as_array()
        .map(|list| list.iter().map(Finding::from_json).collect())
        .unwrap_or_default();
    // Worst first; the model's order within a severity is kept.
    findings.sort_by_key(|f| f.severity);
    print_findings(opts, &findings, reviewed);

    let failing = |min: Severity| findings.iter().any(|f| f.severity <= min);
    Ok(if opts.fail_on.is_some_and(failing) {
        EXIT_FINDINGS
    } else {
        0
    })
}

/// The JSON Schema the reviewer's final message must satisfy.
fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string" },
                        "line": { "type": "integer" },
                        "severity": { "enum": ["P0", "P1", "P2", "P3"] },
                        "summary": { "type": "string" },
                        "scenario": { "type": "string" },
                    },
                    "required": ["file", "line", "severity", "summary", "scenario"],
                },
            },
        },
        "required": ["findings"],
    })
}

/// One defect the reviewer reported.
#[derive(Debug, PartialEq)]
struct Finding {
    file: String,
    line: u64,
    severity: Severity,
    summary: String,
    scenario: String,
}

impl Finding {
    /// From an item that passed [`schema`], which is why nothing here fails:
    /// the fallbacks are for what the validator does not pin down (a
    /// negative line number).
    fn from_json(v: &Value) -> Finding {
        let text = |key: &str| v[key].as_str().unwrap_or_default().trim().to_string();
        Finding {
            file: text("file"),
            line: v["line"].as_u64().unwrap_or(0),
            severity: v["severity"]
                .as_str()
                .and_then(Severity::parse)
                .unwrap_or(Severity::P3),
            summary: text("summary"),
            scenario: text("scenario"),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "file": self.file,
            "line": self.line,
            "severity": self.severity.as_str(),
            "summary": self.summary,
            "scenario": self.scenario,
        })
    }
}

/// Write the review to stdout: one JSON object, or a block per finding.
fn print_findings(opts: &Options, findings: &[Finding], reviewed: Value) {
    if opts.json {
        let findings: Vec<Value> = findings.iter().map(Finding::to_json).collect();
        println!("{}", json!({ "findings": findings, "reviewed": reviewed }));
    } else {
        print!("{}", render(findings));
    }
}

/// The findings as text: a block each, in the order given.
fn render(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return "No findings.\n".to_string();
    }
    let blocks: Vec<String> = findings
        .iter()
        .map(|f| {
            format!(
                "{} {}:{}\n  {}\n  Fails when: {}\n",
                f.severity.as_str(),
                f.file,
                f.line,
                f.summary,
                f.scenario
            )
        })
        .collect();
    blocks.join("\n")
}

/// The reviewer's task: what the change is, what the user added, the diff,
/// and what of it is missing.
fn prompt(change: &Change, shown: &Shown, instructions: Option<&str>) -> String {
    let mut out = format!("Review this change: {}.\n", change.what);
    if let Some(message) = &change.message {
        out.push_str(&format!("\nIts commit message:\n{}\n", message.trim()));
    }
    if let Some(extra) = instructions {
        out.push_str(&format!(
            "\nFrom the person asking for the review:\n{extra}\n"
        ));
    }
    out.push_str(&format!("\n<diff>\n{}</diff>\n", shown.text));
    if shown.truncated() {
        out.push_str(&format!(
            "\nThe diff above is incomplete: it was cut at {} KiB.",
            MAX_DIFF_BYTES / 1024
        ));
        if let Some(file) = &shown.cut {
            out.push_str(&format!(" Cut short: {file}."));
        }
        if !shown.omitted.is_empty() {
            out.push_str(&format!(
                " Changed too, but not shown at all: {}.",
                shown.omitted.join(", ")
            ));
        }
        out.push_str(
            " These files are part of the change under review: read them with your tools \
             before reporting on them, and do not take what you were not shown for correct.\n",
        );
    }
    out
}

/// A change to review, as gathered from the repository.
struct Change {
    /// What it is, in words: "the uncommitted changes".
    what: String,
    /// What identifies it in the JSON output's `reviewed` object.
    reviewed: Value,
    /// The commit's message, when it is one commit.
    message: Option<String>,
    /// The diff, a piece per file, in git's order.
    files: Vec<FileDiff>,
}

/// One file's part of a diff.
#[derive(Debug, PartialEq)]
struct FileDiff {
    path: String,
    text: String,
}

/// Options every diff is taken with: plain text whatever the user's
/// configuration (no colour, no external diff program), and limited to the
/// current directory with paths relative to it — the directory the
/// reviewer's tools are confined to and resolve paths from.
const DIFF_OPTIONS: [&str; 3] = ["--no-color", "--no-ext-diff", "--relative"];

/// git, run in the project directory, for reading only.
struct Git<'a> {
    root: &'a Path,
}

impl Git<'_> {
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        // Without this, `git status` and `git diff` refresh the index file
        // as a side effect — harmless, but a write from a read-only run.
        cmd.env("GIT_OPTIONAL_LOCKS", "0")
            .arg("-C")
            .arg(self.root)
            .args(args);
        cmd
    }

    /// The output of a git command that must succeed.
    fn run(&self, args: &[&str]) -> Result<String> {
        let out = self
            .command(args)
            .output()
            .context("running git (is it installed?)")?;
        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr);
            bail!("git {}: {}", args.join(" "), why.trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// The trimmed output of a git command that may fail (a ref that does
    /// not exist); `None` when it does, or prints nothing.
    fn query(&self, args: &[&str]) -> Option<String> {
        let out = self.command(args).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !text.is_empty()).then_some(text)
    }

    /// The change `target` names, or the one the repository's state
    /// suggests: its uncommitted work if there is any, else the branch
    /// against the default one.
    fn change(&self, target: Option<Target>) -> Result<Change> {
        if self
            .query(&["rev-parse", "--is-inside-work-tree"])
            .as_deref()
            != Some("true")
        {
            bail!("{} is not inside a git work tree", self.root.display());
        }
        let target = match target {
            Some(target) => target,
            None if self.query(&["status", "--porcelain"]).is_some() => Target::Uncommitted,
            None => Target::Base(self.default_branch().context(
                "the working tree is clean and no default branch was found (origin/HEAD, main, \
                 master): say what to review with --base or --commit",
            )?),
        };
        match target {
            Target::Uncommitted => {
                // A repository without a commit has no HEAD to compare to;
                // what is staged is then all there is.
                let against = match self.query(&["rev-parse", "--verify", "--quiet", "HEAD"]) {
                    Some(_) => "HEAD",
                    None => "--cached",
                };
                Ok(Change {
                    what: "the uncommitted changes in the working tree".to_string(),
                    reviewed: json!({ "target": "uncommitted" }),
                    message: None,
                    files: self.tree_diff(against)?,
                })
            }
            Target::Base(base) => {
                let fork = self
                    .run(&["merge-base", &base, "HEAD"])
                    .with_context(|| format!("finding where this branch left {base}"))?;
                let fork = fork.trim();
                Ok(Change {
                    what: format!(
                        "everything on this branch since it left {base}, uncommitted work included"
                    ),
                    reviewed: json!({ "target": "base", "base": base, "merge_base": fork }),
                    message: None,
                    files: self.tree_diff(fork)?,
                })
            }
            Target::Commit(name) => {
                let sha = self
                    .run(&[
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        &format!("{name}^{{commit}}"),
                    ])
                    .map_err(|_| anyhow::anyhow!("{name} does not name a commit"))?;
                let sha = sha.trim();
                // A merge is reviewed as what it brought into its first
                // parent, the branch it was merged into.
                let mut args = vec!["show", "--format=", "--patch", "--diff-merges=first-parent"];
                args.extend(DIFF_OPTIONS);
                args.push(sha);
                Ok(Change {
                    what: format!("commit {}", &sha[..sha.len().min(12)]),
                    reviewed: json!({ "target": "commit", "commit": sha }),
                    message: Some(self.run(&["show", "--no-patch", "--format=%B", sha])?),
                    files: split_files(&self.run(&args)?),
                })
            }
        }
    }

    /// The remote's default branch as origin recorded it, else a local
    /// `main` or `master`.
    fn default_branch(&self) -> Option<String> {
        let remote = self.query(&[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ]);
        remote.or_else(|| {
            ["main", "master"]
                .into_iter()
                .find(|b| {
                    let name = format!("refs/heads/{b}");
                    self.query(&["rev-parse", "--verify", "--quiet", &name])
                        .is_some()
                })
                .map(str::to_string)
        })
    }

    /// The working tree against `against` (a commit, or `--cached`), then
    /// the files git does not track yet — which `git diff` leaves out, and
    /// which are as much a part of uncommitted work as an edit is.
    fn tree_diff(&self, against: &str) -> Result<Vec<FileDiff>> {
        let mut args = vec!["diff"];
        args.extend(DIFF_OPTIONS);
        args.push(against);
        let tracked = self.run(&args)?;
        let mut files = split_files(&tracked);

        let mut spent = tracked.len();
        let list = self.run(&["ls-files", "--others", "--exclude-standard", "-z"])?;
        for path in list.split('\0').filter(|p| !p.is_empty()) {
            let size = std::fs::metadata(self.root.join(path)).map_or(0, |m| m.len());
            // Past the cap nothing more is shown anyway: name the file (it
            // is then listed as left out) rather than read it — this may be
            // a tree of build output nobody ignored.
            let text = if spent > MAX_DIFF_BYTES || size > MAX_DIFF_BYTES as u64 {
                format!("diff --git a/{path} b/{path}\nnew file, {size} bytes (not read)\n")
            } else {
                self.new_file(path)?
            };
            spent += text.len();
            files.push(FileDiff {
                path: path.to_string(),
                text,
            });
        }
        Ok(files)
    }

    /// An untracked file as a diff that adds it.
    fn new_file(&self, path: &str) -> Result<String> {
        let args = [
            "diff",
            "--no-index",
            "--no-color",
            "--no-ext-diff",
            "--",
            "/dev/null",
            path,
        ];
        let out = self
            .command(&args)
            .output()
            .context("running git (is it installed?)")?;
        // `--no-index` exits 1 to say the two differ, as they must here.
        if !matches!(out.status.code(), Some(0 | 1)) {
            let why = String::from_utf8_lossy(&out.stderr);
            bail!("git diff --no-index {path}: {}", why.trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Split a diff into its files, at the `diff --git` lines.
fn split_files(diff: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    for line in diff.split_inclusive('\n') {
        match line.strip_prefix("diff --git ") {
            Some(names) => files.push(FileDiff {
                path: new_path(names.trim_end()),
                text: line.to_string(),
            }),
            // Whatever precedes the first file is not part of any.
            None => {
                if let Some(file) = files.last_mut() {
                    file.text.push_str(line);
                }
            }
        }
    }
    files
}

/// The file's path after the change, from the `a/old b/new` of a
/// `diff --git` line.
fn new_path(names: &str) -> String {
    // The two names are equal unless the file was renamed; a path holding
    // " b/" itself makes the split ambiguous, and the later one is the
    // better guess.
    let path = names.rsplit_once(" b/").map_or(names, |(_, new)| new);
    path.trim_matches('"').to_string()
}

/// A diff as it goes into the prompt.
#[derive(Debug, PartialEq)]
struct Shown {
    text: String,
    /// Every file of the change, shown or not.
    files: Vec<String>,
    /// The file whose diff was cut short.
    cut: Option<String>,
    /// The files after it, of which nothing is shown.
    omitted: Vec<String>,
}

impl Shown {
    fn truncated(&self) -> bool {
        self.cut.is_some() || !self.omitted.is_empty()
    }

    /// The files the reviewer was not shown in full.
    fn not_shown(&self) -> Vec<&String> {
        self.cut.iter().chain(&self.omitted).collect()
    }
}

/// Fit `files` in `max` bytes: whole files in order while they fit, then the
/// start of the first that does not (cut at a line), and nothing of the rest
/// — never a later, smaller file in place of an earlier one, so what was
/// left out is simply "from here on".
fn cap(files: &[FileDiff], max: usize) -> Shown {
    let mut shown = Shown {
        text: String::new(),
        files: files.iter().map(|f| f.path.clone()).collect(),
        cut: None,
        omitted: Vec::new(),
    };
    let mut full = false;
    for file in files {
        if full {
            shown.omitted.push(file.path.clone());
            continue;
        }
        let room = max - shown.text.len();
        if file.text.len() <= room {
            shown.text.push_str(&file.text);
            continue;
        }
        full = true;
        let (head, _) = truncate(&file.text, room);
        match head.rfind('\n') {
            Some(end) => {
                shown.text.push_str(&head[..=end]);
                shown.cut = Some(file.path.clone());
            }
            // Not even its first line fits.
            None => shown.omitted.push(file.path.clone()),
        }
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Options, String> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse(&args)
    }

    #[test]
    fn arguments_choose_target_format_and_instructions() {
        let none = parse_args(&[]).unwrap();
        assert_eq!(none.target, None);
        assert!(!none.json && none.fail_on.is_none() && none.instructions.is_none());

        let opts = parse_args(&[
            "focus on",
            "--base=origin/main",
            "--output-format",
            "json",
            "--fail-on",
            "p1",
            "the parser",
            "--max-turns",
            "30",
        ])
        .unwrap();
        assert_eq!(opts.target, Some(Target::Base("origin/main".into())));
        assert!(opts.json);
        assert_eq!(opts.fail_on, Some(Severity::P1));
        assert_eq!(opts.max_turns, Some(30));
        assert_eq!(opts.instructions.as_deref(), Some("focus on the parser"));

        let commit = parse_args(&["--commit", "abc123", "--", "--not-a-flag"]).unwrap();
        assert_eq!(commit.target, Some(Target::Commit("abc123".into())));
        assert_eq!(commit.instructions.as_deref(), Some("--not-a-flag"));
        assert_eq!(
            parse_args(&["--uncommitted"]).unwrap().target,
            Some(Target::Uncommitted)
        );
    }

    #[test]
    fn argument_errors() {
        let err = |args: &[&str]| parse_args(args).unwrap_err();
        assert!(err(&["--uncommitted", "--base", "main"]).contains("choose one"));
        assert_eq!(err(&["--base"]), "--base needs a value");
        // Not handed to git as an option.
        assert!(err(&["--commit", "--output=/tmp/x"]).contains("not a revision"));
        assert!(err(&["--fail-on", "high"]).contains("unknown severity"));
        assert!(err(&["--output-format", "stream-json"]).contains("unknown format"));
        assert!(err(&["--max-turns", "0"]).contains("positive number"));
        assert_eq!(err(&["--uncommitted=yes"]), "--uncommitted takes no value");
        assert_eq!(err(&["--frob"]), "unknown option --frob");
    }

    const DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 1..2 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1 +1 @@
-old
+new
diff --git a/old name.txt b/new name.txt
similarity index 90%
rename from old name.txt
rename to new name.txt
diff --git a/c.txt b/c.txt
new file mode 100644
--- /dev/null
+++ b/c.txt
@@ -0,0 +1 @@
+c
";

    #[test]
    fn a_diff_splits_into_its_files() {
        let files = split_files(DIFF);
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["src/a.rs", "new name.txt", "c.txt"]);
        assert!(files[0].text.starts_with("diff --git a/src/a.rs"));
        assert!(files[0].text.ends_with("+new\n"));
        let whole: String = files.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(whole, DIFF);
        assert!(split_files("").is_empty());
    }

    #[test]
    fn a_long_diff_is_cut_at_a_line_and_says_what_is_missing() {
        let files = split_files(DIFF);
        let all = cap(&files, DIFF.len());
        assert_eq!(all.text, DIFF);
        assert!(!all.truncated());

        // Room for the first file and two lines of the second.
        let max = files[0].text.len() + 70;
        let shown = cap(&files, max);
        assert!(shown.text.len() <= max);
        assert!(shown.text.starts_with(&files[0].text));
        assert!(shown.text.ends_with("similarity index 90%\n"));
        assert_eq!(shown.files, ["src/a.rs", "new name.txt", "c.txt"]);
        assert_eq!(shown.cut.as_deref(), Some("new name.txt"));
        assert_eq!(shown.omitted, ["c.txt"]);
        assert_eq!(shown.not_shown(), ["new name.txt", "c.txt"]);

        let change = Change {
            what: "the uncommitted changes".into(),
            reviewed: json!({}),
            message: None,
            files,
        };
        let prompt = prompt(&change, &shown, Some("mind the rename"));
        assert!(prompt.contains("From the person asking for the review:\nmind the rename"));
        assert!(prompt.contains("Cut short: new name.txt."));
        assert!(prompt.contains("not shown at all: c.txt."));
        assert!(!prompt.contains("+c\n"));

        // Too little room for even one line: the file is simply left out.
        let none = cap(&change.files, 10);
        assert_eq!(none.text, "");
        assert_eq!(none.cut, None);
        assert_eq!(none.omitted.len(), 3);
    }

    #[test]
    fn findings_are_laid_out_one_block_each() {
        let finding = Finding::from_json(&json!({
            "file": "src/a.rs", "line": 7, "severity": "P1",
            "summary": "The index is off by one.",
            "scenario": "An empty list panics.",
        }));
        assert_eq!(finding.severity, Severity::P1);
        assert_eq!(
            render(&[finding]),
            "P1 src/a.rs:7\n  The index is off by one.\n  Fails when: An empty list panics.\n"
        );
        assert_eq!(render(&[]), "No findings.\n");
        assert!(Severity::P0 < Severity::P3);
        assert!(crate::schema::validate(&schema(), &json!({"findings": []})).is_ok());
        assert!(
            crate::schema::validate(
                &schema(),
                &json!({"findings": [{"file": "a", "line": 1, "severity": "high",
                          "summary": "s", "scenario": "x"}]})
            )
            .is_err()
        );
    }
}
