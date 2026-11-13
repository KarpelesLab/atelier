//! Worktree isolation: a subagent working in a checkout of its own.
//!
//! Two conversations editing one directory trip over each other, and the
//! stale-edit check only catches it afterwards. An agent whose definition
//! says `isolation: worktree` — or whose `task` call does — is given a
//! fresh **git worktree** of the repository instead: `git worktree add
//! --detach` of the commit `HEAD` is at, under
//! `<project>/.atelier/worktrees/<id>/`. That directory is the child's
//! project root: its file tools are confined to it, its `bash` runs in it,
//! and its instruction files are read from it. What is not committed in the
//! parent's tree is not there.
//!
//! When the child ends ([`Worktree::settle`]):
//!
//! - it changed nothing (`git status --porcelain` is empty and `HEAD` has
//!   not moved): the worktree is removed;
//! - otherwise the worktree stays, and the child's report gains where it
//!   is, a `git diff --stat` against the base commit (the child's commits
//!   and its uncommitted changes together) and the command that brings the
//!   changes over. **Nothing is merged here**: the parent agent decides.
//!
//! Worktrees that stay are the user's to remove: `/agents clean`
//! ([`clean`]) removes those that hold nothing that would be lost, and one
//! named by id whatever it holds.
//!
//! # Details that matter
//!
//! - A worktree in use is **locked** (`git worktree lock`, the reason
//!   naming this process), so neither `git worktree prune` nor another
//!   atelier's `/agents clean` takes it from under a running agent. A lock
//!   whose process is gone counts for nothing.
//! - `.atelier/worktrees/` holds a `.gitignore` of `*`, so the copies never
//!   show in the parent's `git status`; the parent's own walks leave the
//!   directory out as they do spilled tool output (see
//!   [`crate::tools::spill::keep_in_walk`]).
//! - The project root may be a subdirectory of the repository. The
//!   worktree is of the whole repository; the child's root is the same
//!   subdirectory inside it.
//! - Untracked files count as changes. They are marked *intent to add* in
//!   the worktree's index before the summary is made, which is what makes
//!   `git diff <base>` — the summary and the hand-over command — include
//!   them.
//! - Except atelier's own scratch files ([`SCRATCH`]): output a tool
//!   spilled to a file and the `todo` list land under the child's
//!   `.atelier/` like anyone's, and are nobody's change. They are left out
//!   of every question asked here, and never marked.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::agents::WORKTREES_DIR;

/// The most lines of `git diff --stat` and of the commit list a report
/// carries.
const MAX_SUMMARY_LINES: usize = 40;

/// Pathspecs for "the whole worktree, but not what atelier itself writes
/// while an agent works": spilled tool output (`tools::spill::DIR`) and the
/// `todo` tool's list — wherever in the repository the project root is.
const SCRATCH: [&str; 4] = [
    "--",
    ":(top)",
    ":(exclude,glob)**/.atelier/tool-output/**",
    ":(exclude,glob)**/.atelier/todos.json",
];

/// The changes in the worktree at `dir`, one per line as `git status
/// --porcelain` gives them; empty when there are none.
fn changes(dir: &Path) -> Result<String, String> {
    // Every file by name: a directory shown as one entry would hide
    // whether all of it is scratch.
    let mut args = vec!["status", "--porcelain", "--untracked-files=all"];
    args.extend(SCRATCH);
    git(dir, &args)
}

/// A child's worktree, from its creation to the child's end.
pub(super) struct Worktree {
    /// The parent's project root, where git is run from and paths are
    /// shown relative to.
    project: PathBuf,
    /// The worktree's directory.
    dir: PathBuf,
    /// The child's project root: `dir`, or the project's subdirectory of
    /// the repository inside it.
    root: PathBuf,
    /// The repository's top directory, when the project is a subdirectory
    /// of it — where a patch of the whole repository has to be applied.
    top: Option<PathBuf>,
    /// The commit it was created at.
    base: String,
}

/// What became of a worktree when its child ended.
pub(super) struct Settled {
    /// Appended to the child's report.
    pub(super) report: String,
    /// Appended to the line the user is shown; empty when the worktree is
    /// gone.
    pub(super) note: String,
}

/// Run `git -C <dir> <args>`: its standard output, trimmed at the end, or
/// what went wrong as one line.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git could not be run: {e}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let first = stderr.lines().next().unwrap_or("").trim();
    Err(match first {
        "" => format!("git {} failed ({})", args.join(" "), output.status),
        said => said.to_string(),
    })
}

/// Where the worktree of the agent `id` goes in the project at `project`.
pub(super) fn dir_of(project: &Path, id: &str) -> PathBuf {
    project.join(WORKTREES_DIR).join(id)
}

/// A commit id as shown: its first twelve characters.
fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

/// `path` relative to `project` where it is inside it (as it always is
/// for a worktree), for display and for commands run from the project.
fn shown(project: &Path, path: &Path) -> String {
    path.strip_prefix(project)
        .unwrap_or(path)
        .display()
        .to_string()
}

impl Worktree {
    /// A fresh worktree for the agent `id` of the repository `project` is
    /// in, at the commit `HEAD` is at. `Err` is the tool result of the
    /// `task` call that asked: isolation was required, so there is no
    /// falling back to the shared directory.
    pub(super) fn create(project: &Path, id: &str) -> Result<Self, String> {
        let refused = |why: String| {
            format!(
                "error: the agent was not started: isolation \"worktree\" {why}. Run the task \
                 without isolation, or tell the user what is missing."
            )
        };
        let prefix = git(project, &["rev-parse", "--show-prefix"]).map_err(|e| {
            refused(format!(
                "needs a git repository, and {} is not in one ({e})",
                project.display()
            ))
        })?;
        let base = git(
            project,
            &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        )
        .map_err(|_| {
            refused("needs a commit to check out, and this repository has none yet".to_string())
        })?;
        let top = match prefix.is_empty() {
            true => None,
            false => {
                let top = git(project, &["rev-parse", "--show-toplevel"]).map_err(&refused)?;
                Some(PathBuf::from(top))
            }
        };

        let dir = dir_of(project, id);
        let home = project.join(WORKTREES_DIR);
        let prepared = std::fs::create_dir_all(&home).and_then(|()| {
            // Copies of the repository are nobody's changes.
            let ignore = home.join(".gitignore");
            match ignore.exists() {
                true => Ok(()),
                false => std::fs::write(ignore, "*\n"),
            }
        });
        prepared.map_err(|e| refused(format!("could not prepare {}: {e}", home.display())))?;
        let at = dir.display().to_string();
        git(project, &["worktree", "add", "--detach", &at, &base])
            .map_err(|e| refused(format!("could not create the worktree: {e}")))?;
        // Best effort: an unlocked worktree works just the same.
        let reason = format!("{LOCK_REASON}{})", std::process::id());
        let _ = git(project, &["worktree", "lock", "--reason", &reason, &at]);
        Ok(Self {
            project: project.to_path_buf(),
            root: match prefix.is_empty() {
                true => dir.clone(),
                false => dir.join(&prefix),
            },
            dir,
            top,
            base,
        })
    }

    /// The worktree's directory.
    pub(super) fn dir(&self) -> &Path {
        &self.dir
    }

    /// The project root of the child that works here.
    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    /// The child is done: remove the worktree if it changed nothing, else
    /// leave it and say what is in it and how to bring it over.
    pub(super) fn settle(self) -> Settled {
        let at = shown(&self.project, &self.dir);
        let _ = git(&self.project, &["worktree", "unlock", &at]);
        let status = changes(&self.dir);
        let head = git(&self.dir, &["rev-parse", "HEAD"]);
        let base = short(&self.base);
        if status.as_deref() == Ok("") && head.as_deref() == Ok(self.base.as_str()) {
            return match remove(&self.project, &self.dir) {
                Ok(()) => Settled {
                    report: "\n\n[worktree: the agent changed nothing in its worktree, which \
                             was removed]"
                        .to_string(),
                    note: String::new(),
                },
                Err(e) => Settled {
                    report: format!(
                        "\n\n[worktree: {at} — the agent changed nothing in it, but it could \
                         not be removed: {e}]"
                    ),
                    note: format!(" — worktree left at {at}"),
                },
            };
        }

        // From here on, untracked files are part of `git diff <base>`.
        let mut mark = vec!["add", "--all", "--intent-to-add"];
        mark.extend(SCRATCH);
        let _ = git(&self.dir, &mark);
        let range = format!("{}..HEAD", self.base);
        let commits = git(&self.dir, &["log", "--reverse", "--format=%h %s", &range]);
        let commits = commits.unwrap_or_default();
        let stat = git(&self.dir, &["diff", "--stat", &self.base])
            .unwrap_or_else(|e| format!("(git diff --stat failed: {e})"));
        let apply = match &self.top {
            None => "git apply".to_string(),
            Some(top) => format!("git -C {} apply", top.display()),
        };

        let mut report = format!(
            "\n\n[worktree: {at} — the agent worked in a git worktree of its own, created at \
             commit {base}. Its changes are there, NOT in your working tree, and nothing is \
             merged automatically.]\n"
        );
        if !commits.is_empty() {
            report.push_str(&format!(
                "commits it made:\n{}\n",
                clip_lines(&indent(&commits))
            ));
        }
        report.push_str(&format!(
            "changes against {base} (its commits and its uncommitted changes):\n{}\n",
            clip_lines(&stat)
        ));
        report.push_str(&format!(
            "To bring all of it into your working tree, run from the project directory:\n  \
             git -C {at} diff --binary {base} | {apply}\n"
        ));
        if !commits.is_empty() {
            let head = head.as_deref().map(short).unwrap_or("HEAD").to_string();
            report.push_str(&format!(
                "or take its commits as commits (without what it left uncommitted):\n  \
                 git cherry-pick {base}..{head}\n"
            ));
        }
        report.push_str(&format!(
            "Look before you take: `git -C {at} diff {base}` shows the changes in full. The \
             worktree stays until it is removed (`git worktree remove --force {at}`, or the \
             user's /agents clean)."
        ));
        Settled {
            report,
            note: format!(" — changes left in {at}"),
        }
    }
}

/// What opens the reason a worktree in use is locked with; the process id
/// and a `)` follow.
const LOCK_REASON: &str = "atelier agent at work (pid ";

/// Remove the worktree at `dir`, whatever is in it.
fn remove(project: &Path, dir: &Path) -> Result<(), String> {
    let at = dir.display().to_string();
    // Twice `--force`: once for what is not committed, once for a lock.
    git(project, &["worktree", "remove", "--force", "--force", &at]).map(|_| ())
}

/// Each line of `text` indented by two spaces.
fn indent(text: &str) -> String {
    let lines: Vec<String> = text.lines().map(|l| format!("  {l}")).collect();
    lines.join("\n")
}

/// `text` cut to [`MAX_SUMMARY_LINES`] lines — the first ones and the last,
/// which in a `--stat` is the total.
fn clip_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= MAX_SUMMARY_LINES {
        return text.to_string();
    }
    let kept = MAX_SUMMARY_LINES - 1;
    format!(
        "{}\n  … {} more lines …\n{}",
        lines[..kept].join("\n"),
        lines.len() - MAX_SUMMARY_LINES,
        lines[lines.len() - 1]
    )
}

/// The process a worktree is locked for, if it is locked by an atelier.
fn locked_by(dir: &Path) -> Option<u32> {
    // The lock is a file in the worktree's administrative directory,
    // holding the reason.
    let admin = git(dir, &["rev-parse", "--absolute-git-dir"]).ok()?;
    let reason = std::fs::read_to_string(Path::new(&admin).join("locked")).ok()?;
    let pid = reason.trim().strip_prefix(LOCK_REASON)?.strip_suffix(')')?;
    pid.parse().ok()
}

/// Whether the process `pid` exists. Where that cannot be asked, it is
/// taken to: a worktree is not removed on a guess.
fn alive(pid: u32) -> bool {
    #[cfg(unixlike)]
    {
        let probe = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status();
        probe.map(|s| s.success()).unwrap_or(true)
    }
    #[cfg(not(unixlike))]
    {
        let _ = pid;
        true
    }
}

/// `/agents clean [id]`: remove agents' worktrees under `project`. Without
/// an id, those that hold nothing that would be lost — no uncommitted or
/// untracked change, and a `HEAD` some branch or tag still reaches; with
/// one, that worktree whatever it holds: naming it is the confirmation.
/// Never one an agent is working in — `running` are this session's, and a
/// lock names another process's. Returns the lines to show.
pub(super) fn clean(project: &Path, running: &[PathBuf], only: Option<&str>) -> Vec<String> {
    let home = project.join(WORKTREES_DIR);
    let mut ids: Vec<String> = std::fs::read_dir(&home)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|id| only.is_none_or(|only| only == id))
        .collect();
    ids.sort();
    if ids.is_empty() {
        return vec![match only {
            Some(id) => format!("no worktree {id} under {WORKTREES_DIR}/"),
            None => format!("no agent worktrees under {WORKTREES_DIR}/"),
        }];
    }
    let mut lines = Vec::new();
    for id in ids {
        let dir = home.join(&id);
        if running.contains(&dir) {
            lines.push(format!(
                "{id}: an agent is working in it — /agents stop {id} first"
            ));
            continue;
        }
        // This process's lock without a running agent is one left behind.
        let holder = locked_by(&dir).filter(|&pid| pid != std::process::id() && alive(pid));
        if let Some(pid) = holder {
            lines.push(format!(
                "{id}: in use by another atelier (process {pid}) — left alone"
            ));
            continue;
        }
        let losing = match what_would_be_lost(project, &dir) {
            Ok(losing) => losing,
            Err(e) => Some(format!("it could not be examined: {e}")),
        };
        match (losing, only) {
            (Some(what), None) => lines.push(format!(
                "{id}: kept — {what}. /agents clean {id} removes it anyway"
            )),
            (losing, _) => lines.push(match remove(project, &dir) {
                Ok(()) if losing.is_some() => format!("{id}: removed, with its changes"),
                Ok(()) => format!("{id}: unchanged — removed"),
                Err(e) => format!("{id}: could not be removed: {e}"),
            }),
        }
    }
    lines
}

/// What removing the worktree at `dir` would lose, in a few words; `None`
/// when nothing.
fn what_would_be_lost(project: &Path, dir: &Path) -> Result<Option<String>, String> {
    let status = changes(dir)?;
    let head = git(dir, &["rev-parse", "HEAD"])?;
    let refs = ["refs/heads", "refs/tags", "refs/remotes"];
    let mut reached = vec!["for-each-ref", "--count=1", "--contains", &head];
    reached.extend(refs);
    let reached = git(project, &reached)?;
    let mut what = Vec::new();
    match status.lines().count() {
        0 => {}
        1 => what.push("1 changed file".to_string()),
        n => what.push(format!("{n} changed files")),
    }
    if reached.is_empty() {
        what.push("commits no branch holds".to_string());
    }
    Ok(Some(what.join(", ")).filter(|w| !w.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory, canonical (git reports real paths).
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("atl-worktree-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    /// Run git for the test itself, with an identity for commits.
    fn run(dir: &Path, args: &[&str]) -> String {
        // Whatever the machine's own git configuration says.
        let mut full = vec![
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
        ];
        full.extend(args);
        git(dir, &full).unwrap_or_else(|e| panic!("git {args:?}: {e}"))
    }

    /// A repository with one commit holding `a.txt` and `sub/b.txt`.
    fn repo(name: &str) -> PathBuf {
        let dir = scratch(name);
        run(&dir, &["init", "--quiet"]);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join("sub/b.txt"), "bee\n").unwrap();
        run(&dir, &["add", "--all"]);
        run(&dir, &["commit", "--quiet", "-m", "base"]);
        dir
    }

    #[test]
    fn an_unchanged_worktree_is_removed_when_its_agent_ends() {
        let project = repo("unchanged");
        // Not committed: not in the worktree.
        std::fs::write(project.join("draft.txt"), "uncommitted").unwrap();
        let worktree = Worktree::create(&project, "agent-1").unwrap();
        let dir = worktree.dir().to_path_buf();
        assert_eq!(dir, project.join(".atelier/worktrees/agent-1"));
        assert_eq!(worktree.root(), dir);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
        assert!(!dir.join("draft.txt").exists());
        // In use: locked for this process; and no trace in the parent's status.
        assert_eq!(locked_by(&dir), Some(std::process::id()));
        assert_eq!(run(&project, &["status", "--porcelain"]), "?? draft.txt");

        // What atelier's own tools leave under `.atelier/` is no change.
        std::fs::create_dir_all(dir.join(".atelier/tool-output")).unwrap();
        std::fs::write(dir.join(".atelier/tool-output/ab12cd.txt"), "spilled").unwrap();
        std::fs::write(dir.join(".atelier/todos.json"), "[]").unwrap();

        let settled = worktree.settle();
        assert!(
            settled.report.contains("changed nothing"),
            "{}",
            settled.report
        );
        assert_eq!(settled.note, "");
        assert!(!dir.exists());
        assert!(!run(&project, &["worktree", "list"]).contains("agent-1"));
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn a_changed_worktree_stays_and_the_report_says_how_to_take_its_changes() {
        let project = repo("changed");
        let worktree = Worktree::create(&project, "agent-2").unwrap();
        let dir = worktree.dir().to_path_buf();
        // A commit, a change left uncommitted, and a file never added.
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        run(&dir, &["commit", "--quiet", "-am", "the agent's commit"]);
        std::fs::write(dir.join("sub/b.txt"), "bee\nmore\n").unwrap();
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        // Scratch next to real changes is not part of them; a file the
        // agent made under `.atelier/` on purpose is.
        std::fs::create_dir_all(dir.join(".atelier/agents")).unwrap();
        std::fs::write(dir.join(".atelier/todos.json"), "[]").unwrap();
        std::fs::write(dir.join(".atelier/agents/helper.md"), "an agent\n").unwrap();

        let settled = worktree.settle();
        let report = &settled.report;
        assert_eq!(
            settled.note,
            " — changes left in .atelier/worktrees/agent-2"
        );
        assert!(
            report.starts_with("\n\n[worktree: .atelier/worktrees/agent-2 — "),
            "{report}"
        );
        assert!(report.contains("NOT in your working tree"), "{report}");
        assert!(report.contains("the agent's commit"), "{report}");
        for file in ["a.txt", "sub/b.txt", "new.txt", ".atelier/agents/helper.md"] {
            assert!(report.contains(&format!(" {file} ")), "{file}: {report}");
        }
        assert!(!report.contains("todos.json"), "{report}");
        assert!(report.contains("4 files changed"), "{report}");
        assert!(report.contains("git cherry-pick "), "{report}");
        assert!(dir.exists());
        // Finished: no longer locked.
        assert_eq!(locked_by(&dir), None);

        // The command the report gives does what it says.
        let command = report
            .lines()
            .find(|l| l.contains("diff --binary"))
            .expect("the command")
            .trim();
        assert!(
            command.starts_with("git -C .atelier/worktrees/agent-2 diff --binary ")
                && command.ends_with(" | git apply"),
            "{command}"
        );
        let applied = Command::new("sh")
            .args(["-c", command])
            .current_dir(&project)
            .output()
            .unwrap();
        assert!(applied.status.success(), "{applied:?}");
        let read = |file: &str| std::fs::read_to_string(project.join(file)).unwrap();
        assert_eq!(read("a.txt"), "two\n");
        assert_eq!(read("sub/b.txt"), "bee\nmore\n");
        assert_eq!(read("new.txt"), "new\n");
        assert_eq!(read(".atelier/agents/helper.md"), "an agent\n");
        assert!(!project.join(".atelier/todos.json").exists());
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn a_project_in_a_subdirectory_of_the_repository_keeps_its_place() {
        let top = repo("subdir");
        let project = top.join("sub");
        let worktree = Worktree::create(&project, "agent-1").unwrap();
        // The child's scratch is under its root, deeper in the worktree.
        let state = worktree.root().join(".atelier");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("todos.json"), "[]").unwrap();
        assert_eq!(changes(worktree.dir()).as_deref(), Ok(""));
        let dir = project.join(".atelier/worktrees/agent-1");
        assert_eq!(worktree.dir(), dir);
        // The whole repository is checked out; the child's root is the
        // project's directory in it.
        assert_eq!(worktree.root(), dir.join("sub"));
        assert!(worktree.root().join("b.txt").exists());
        std::fs::write(worktree.root().join("b.txt"), "changed\n").unwrap();
        let report = worktree.settle().report;
        // The patch is of the whole repository: applied at its top.
        let apply = format!(" | git -C {} apply", top.display());
        assert!(report.contains(&apply), "{report}");
        let _ = std::fs::remove_dir_all(&top);
    }

    #[test]
    fn without_a_repository_or_a_commit_there_is_no_worktree() {
        let plain = scratch("plain");
        let refused = Worktree::create(&plain, "agent-1").err().unwrap();
        assert!(
            refused.starts_with(
                "error: the agent was not started: isolation \"worktree\" needs a git repository"
            ),
            "{refused}"
        );
        assert!(!plain.join(".atelier").exists());

        run(&plain, &["init", "--quiet"]);
        let refused = Worktree::create(&plain, "agent-1").err().unwrap();
        assert!(
            refused.contains("this repository has none yet"),
            "{refused}"
        );
        assert!(!plain.join(".atelier").exists());
        let _ = std::fs::remove_dir_all(&plain);
    }

    #[test]
    fn clean_removes_what_holds_nothing_and_what_is_named() {
        let project = repo("clean");
        assert_eq!(
            clean(&project, &[], None),
            ["no agent worktrees under .atelier/worktrees/"]
        );
        // Left behind by agents: one untouched, one with an uncommitted
        // change, one with a commit of its own, one still at work.
        let create = |id: &str| Worktree::create(&project, id).unwrap().dir().to_path_buf();
        let (same, dirty, ahead, busy) = (
            create("agent-1"),
            create("agent-2"),
            create("agent-3"),
            create("agent-4"),
        );
        std::fs::write(dirty.join("new.txt"), "new").unwrap();
        std::fs::write(ahead.join("a.txt"), "two\n").unwrap();
        run(&ahead, &["commit", "--quiet", "-am", "ahead"]);

        let lines = clean(&project, std::slice::from_ref(&busy), None);
        assert_eq!(
            lines,
            [
                "agent-1: unchanged — removed",
                "agent-2: kept — 1 changed file. /agents clean agent-2 removes it anyway",
                "agent-3: kept — commits no branch holds. /agents clean agent-3 removes it anyway",
                "agent-4: an agent is working in it — /agents stop agent-4 first",
            ]
        );
        assert!(!same.exists() && dirty.exists() && ahead.exists() && busy.exists());

        // Named: removed whatever it holds — but not from under an agent.
        assert_eq!(
            clean(&project, std::slice::from_ref(&busy), Some("agent-2")),
            ["agent-2: removed, with its changes"]
        );
        assert!(!dirty.exists());
        assert_eq!(
            clean(&project, std::slice::from_ref(&busy), Some("agent-4")),
            ["agent-4: an agent is working in it — /agents stop agent-4 first"]
        );
        assert_eq!(
            clean(&project, &[], Some("agent-9")),
            ["no worktree agent-9 under .atelier/worktrees/"]
        );
        // A commit a branch has since taken is nothing to lose. (The agent
        // in agent-4 is gone: this process's lock alone does not hold it.)
        let commit = run(&ahead, &["rev-parse", "HEAD"]);
        run(&project, &["branch", "taken", &commit]);
        assert_eq!(
            clean(&project, &[], None),
            [
                "agent-3: unchanged — removed",
                "agent-4: unchanged — removed"
            ]
        );
        let _ = std::fs::remove_dir_all(&project);
    }

    /// A worktree locked for a process that is alive and is not this one is
    /// somebody's: left alone, even when named.
    #[cfg(unixlike)]
    #[test]
    fn clean_leaves_another_processes_worktree_alone() {
        let project = repo("clean-foreign");
        let dir = Worktree::create(&project, "agent-1")
            .unwrap()
            .dir()
            .to_path_buf();
        // Relock it for a process that exists and is not this one.
        let mut other = Command::new("sleep").arg("60").spawn().unwrap();
        let at = dir.display().to_string();
        run(&project, &["worktree", "unlock", &at]);
        let reason = format!("{LOCK_REASON}{})", other.id());
        run(&project, &["worktree", "lock", "--reason", &reason, &at]);
        let expected = format!(
            "agent-1: in use by another atelier (process {}) — left alone",
            other.id()
        );
        assert_eq!(clean(&project, &[], Some("agent-1")), [expected]);
        assert!(dir.exists());

        // Once that process is gone, so is its claim.
        other.kill().unwrap();
        other.wait().unwrap();
        assert_eq!(clean(&project, &[], None), ["agent-1: unchanged — removed"]);
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn long_summaries_keep_their_start_and_their_last_line() {
        let text: Vec<String> = (1..=100).map(|n| format!("line {n}")).collect();
        let clipped = clip_lines(&text.join("\n"));
        let lines: Vec<&str> = clipped.lines().collect();
        assert_eq!(lines.len(), MAX_SUMMARY_LINES + 1);
        assert_eq!(lines[0], "line 1");
        assert_eq!(lines[MAX_SUMMARY_LINES - 1], "  … 60 more lines …");
        assert_eq!(lines[MAX_SUMMARY_LINES], "line 100");
        assert_eq!(clip_lines("a\nb"), "a\nb");
    }
}
