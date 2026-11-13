//! Instruction files: what the user and the repository tell the agent before
//! the first prompt.
//!
//! # What is loaded
//!
//! Every file that applies, least specific first, so a closer file can refine
//! or override a broader one simply by coming later:
//!
//! 1. the user-global file, `$XDG_CONFIG_HOME/atelier/AGENTS.md` (else
//!    `~/.config/atelier/AGENTS.md`);
//! 2. one file per directory from the repository root (the nearest ancestor
//!    holding `.git`; the project root itself when there is none) down to the
//!    project root atelier was launched in.
//!
//! In each directory the first existing name in [`CANDIDATES`] wins, so a
//! repository written for another tool still works without both files being
//! read. The files are concatenated under a header naming each one's path —
//! the model can then tell whose rule it is following, and so can the user
//! reading a transcript.
//!
//! # Imports
//!
//! A line that is nothing but `@path` is replaced by that file's content
//! (see [`Loader::expand`]). An import that can't be honoured leaves a one-line
//! note where it stood rather than failing the load or vanishing silently: a
//! missing include is something the model — and the user — should see.
//!
//! # Budget
//!
//! The whole block is capped at [`BUDGET_BYTES`]. It is cut exactly there and
//! a closing note names the file that was cut and those dropped after it;
//! nothing is reordered or summarized, so what the model sees stays
//! predictable from the files on disk.
//!
//! # Two ways in
//!
//! - [`InstructionsProvider`] yields the root→leaf block as a *standing*
//!   context item: outside the per-turn token budget (it has its own, above)
//!   and kept in front of the model on every request. Because providers run
//!   each turn and the cross-turn dedup compares bodies, an edited instruction
//!   file reaches the model on the next turn without a restart.
//! - [`nested_instructions`] covers directories *below* the project root.
//!   Their rules only matter when working there, so they are not loaded up
//!   front: the agent loop appends them to the result of the first tool call
//!   that reads or writes under such a directory (the tool layer's
//!   `FileState` reports each directory once per session).

use super::{ContextItem, ContextProvider};
use std::path::{Path, PathBuf};

/// Candidate instruction files, in precedence order. The first one that
/// exists in a directory wins for that directory.
const CANDIDATES: &[&str] = &[
    "AGENTS.md",
    ".atelier/instructions.md",
    "CLAUDE.md",
    ".cursorrules",
];

/// Upper bound on the rendered instructions, headers included. Also the
/// largest file an `@import` may pull in: anything bigger could not fit
/// anyway, and is more likely a mistake (a log, a binary) than instructions.
const BUDGET_BYTES: usize = 32 * 1024;

/// How deep `@imports` may nest below the file that was loaded for its name.
const MAX_IMPORT_DEPTH: usize = 5;

/// Reads instruction files. Carries the two locations that lie outside the
/// project, so tests can point them at a temp dir instead of the real home.
#[derive(Clone, Default)]
struct Loader {
    /// The user-global instructions file, if a config directory is known.
    global: Option<PathBuf>,
    /// What a leading `~/` in an import expands to.
    home: Option<PathBuf>,
}

impl Loader {
    /// The locations for this process, from `XDG_CONFIG_HOME` and `HOME`.
    fn from_env() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".config")));
        Self {
            global: config.map(|c| c.join("atelier").join("AGENTS.md")),
            home,
        }
    }

    /// The files that apply at `root`, least specific first.
    fn applicable(&self, root: &Path) -> Vec<PathBuf> {
        let global = self.global.iter().filter(|p| p.is_file()).cloned();
        let project = chain(root)
            .iter()
            .filter_map(|dir| pick(dir))
            .collect::<Vec<_>>();
        global.chain(project).collect()
    }

    /// Renders `files` in order, each under a header naming it, within
    /// [`BUDGET_BYTES`]. `None` when there is nothing to show.
    fn render(&self, files: &[PathBuf]) -> Option<String> {
        let mut out = String::new();
        let mut cut: Option<&Path> = None;
        let mut dropped: Vec<String> = Vec::new();
        for file in files {
            if cut.is_some() {
                dropped.push(file.display().to_string());
                continue;
            }
            let content = match read_text(file, None) {
                Ok(text) => {
                    // The stack holds the files being expanded, to spot a
                    // cycle; it starts with the one loaded for its name.
                    let mut stack = vec![canonical(file)];
                    self.expand(file, &text, &mut stack)
                }
                Err(why) => format!("[skipped: {why}]"),
            };
            let section = format!("### {}\n{}\n\n", file.display(), content.trim());
            let room = BUDGET_BYTES - out.len();
            if section.len() <= room {
                out.push_str(&section);
            } else {
                out.push_str(&section[..floor_char_boundary(&section, room)]);
                cut = Some(file);
            }
        }
        let mut out = out.trim_end().to_string();
        if let Some(file) = cut {
            out.push_str(&format!(
                "\n[instructions truncated at the {} KiB budget: {} was cut short",
                BUDGET_BYTES / 1024,
                file.display()
            ));
            if !dropped.is_empty() {
                out.push_str(&format!("; left out entirely: {}", dropped.join(", ")));
            }
            out.push(']');
        }
        (!out.is_empty()).then_some(out)
    }

    /// Replaces each import line in `text` (the content of `file`) with the
    /// file it names, recursively.
    ///
    /// An import is a line consisting only of `@path`: relative to the
    /// importing file, absolute, or under `~/`. Lines inside a fenced code
    /// block are left alone — an `@decorator` in a code sample is not an
    /// import. `stack` is the chain of files currently being expanded.
    fn expand(&self, file: &Path, text: &str, stack: &mut Vec<PathBuf>) -> String {
        let mut out = String::with_capacity(text.len());
        // The fence character (` or ~) of the code block we're inside, if any.
        let mut fence: Option<char> = None;
        for line in text.lines() {
            let trimmed = line.trim();
            let marker = ['`', '~']
                .into_iter()
                .find(|c| trimmed.starts_with(&c.to_string().repeat(3)));
            match (fence, marker) {
                (None, Some(c)) => fence = Some(c),
                (Some(open), Some(c)) if open == c => fence = None,
                _ => {}
            }
            match import_target(trimmed).filter(|_| fence.is_none() && marker.is_none()) {
                Some(target) => match self.import(file, target, stack) {
                    Ok(content) => out.push_str(content.trim_end()),
                    Err(why) => out.push_str(&format!("[import @{target} skipped: {why}]")),
                },
                None => out.push_str(line),
            }
            out.push('\n');
        }
        out
    }

    /// The expanded content of the file `target` names when imported from
    /// `from`, or why it can't be imported.
    fn import(
        &self,
        from: &Path,
        target: &str,
        stack: &mut Vec<PathBuf>,
    ) -> Result<String, String> {
        if stack.len() > MAX_IMPORT_DEPTH {
            return Err(format!("imports nest deeper than {MAX_IMPORT_DEPTH}"));
        }
        let path = match target.strip_prefix("~/") {
            Some(rest) => self.home.as_ref().ok_or("no home directory")?.join(rest),
            // `join` keeps an absolute target as it is.
            None => from.parent().unwrap_or(Path::new("")).join(target),
        };
        let key = canonical(&path);
        if stack.contains(&key) {
            return Err("import cycle".to_string());
        }
        // A repository's instruction files are loaded without asking, so
        // they may only pull in files of that repository: otherwise a cloned
        // project could put any text file of the user's (credentials, keys)
        // into the prompt. The user's own global file may import anything.
        let top = &stack[0];
        let own = self.global.as_deref().map(canonical).as_ref() == Some(top);
        if !own && !key.starts_with(import_area(top)) {
            return Err("outside the project".to_string());
        }
        let text = read_text(&path, Some(BUDGET_BYTES as u64))?;
        stack.push(key);
        let content = self.expand(&path, &text, stack);
        stack.pop();
        Ok(content)
    }
}

/// The directories whose instruction file applies at `root`: from the
/// repository root down to `root` itself.
fn chain(root: &Path) -> Vec<PathBuf> {
    let top = root
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(root);
    let mut dirs: Vec<PathBuf> = root
        .ancestors()
        .take_while(|dir| dir.starts_with(top))
        .map(Path::to_path_buf)
        .collect();
    dirs.reverse();
    dirs
}

/// The instruction file of `dir`: the first of [`CANDIDATES`] present.
fn pick(dir: &Path) -> Option<PathBuf> {
    CANDIDATES
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
}

/// The directory tree a project instruction file may import from: its
/// repository (nearest ancestor with `.git`), else its own directory.
fn import_area(file: &Path) -> &Path {
    let dir = file.parent().unwrap_or(Path::new(""));
    dir.ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(dir)
}

/// The path an import line names, if `line` (already trimmed) is one.
fn import_target(line: &str) -> Option<&str> {
    line.strip_prefix('@')
        .filter(|target| !target.is_empty() && !target.contains(char::is_whitespace))
}

/// `path` with symlinks resolved, so one file reached by two spellings is
/// recognized as the same; the path as given when it can't be resolved.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Reads `path` as text, or says briefly why it won't do. `max_bytes` refuses
/// larger files before reading them.
fn read_text(path: &Path, max_bytes: Option<u64>) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|_| "not found")?;
    if !meta.is_file() {
        return Err("not a regular file".to_string());
    }
    if let Some(max) = max_bytes.filter(|max| meta.len() > *max) {
        return Err(format!("larger than {} KiB", max / 1024));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("unreadable ({})", e.kind()))?;
    match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => Ok(text),
        _ => Err("not a text file".to_string()),
    }
}

/// The largest index `<= at` that doesn't split a character of `text`.
fn floor_char_boundary(text: &str, at: usize) -> usize {
    (0..=at.min(text.len()))
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0)
}

/// Reports the instruction files that apply at the project root (see the
/// module docs for which, and in what order), or `None` if there are none.
pub struct InstructionsProvider {
    loader: Loader,
}

impl InstructionsProvider {
    pub fn new() -> Self {
        Self {
            loader: Loader::from_env(),
        }
    }
}

impl ContextProvider for InstructionsProvider {
    fn name(&self) -> &str {
        "instructions"
    }

    fn gather(&self, root: &Path) -> Option<ContextItem> {
        let body = self.loader.render(&self.loader.applicable(root))?;
        Some(ContextItem::new("project instructions", body, 250).standing())
    }
}

/// The instruction files of `dirs` that lie below `root`, rendered as a note
/// to append to a tool result — or `None` when none of them has one.
///
/// `dirs` are the directories the agent has just touched for the first time.
/// `root` itself and anything above or outside it are ignored: those files are
/// [`InstructionsProvider`]'s, or not this project's at all.
pub fn nested_instructions(root: &Path, dirs: &[PathBuf]) -> Option<String> {
    nested(&Loader::from_env(), root, dirs)
}

fn nested(loader: &Loader, root: &Path, dirs: &[PathBuf]) -> Option<String> {
    let mut below: Vec<&PathBuf> = dirs
        .iter()
        .filter(|dir| dir.starts_with(root) && dir.as_path() != root)
        .collect();
    // Path order puts a directory before those inside it: root→leaf again.
    below.sort();
    below.dedup();
    let files: Vec<PathBuf> = below.into_iter().filter_map(|dir| pick(dir)).collect();
    let body = loader.render(&files)?;
    Some(format!(
        "\n\n<nested-instructions>\n\
         Project instructions for the directories just entered. Each applies to work under \
         the directory its file is in.\n\n{body}\n</nested-instructions>"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::dedup_items;
    use std::collections::HashMap;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atelier-instructions-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// A provider that sees no user-global file and no home directory.
    fn provider() -> InstructionsProvider {
        InstructionsProvider {
            loader: Loader::default(),
        }
    }

    fn body(provider: &InstructionsProvider, root: &Path) -> String {
        provider.gather(root).expect("instructions expected").body
    }

    #[test]
    fn a_single_file_is_shown_under_its_path() {
        let dir = temp_dir("single");
        write(&dir.join("AGENTS.md"), "Use tabs, not spaces.\n");

        let item = provider().gather(&dir).unwrap();
        assert_eq!(item.title, "project instructions");
        assert!(item.standing);
        assert_eq!(
            item.body,
            format!(
                "### {}\nUse tabs, not spaces.",
                dir.join("AGENTS.md").display()
            )
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_candidate_wins_per_directory() {
        let dir = temp_dir("precedence");
        write(&dir.join("CLAUDE.md"), "claude content");
        assert!(body(&provider(), &dir).contains("claude content"));

        write(&dir.join(".atelier/instructions.md"), "atelier content");
        let b = body(&provider(), &dir);
        assert!(b.contains("atelier content") && !b.contains("claude content"));

        write(&dir.join("AGENTS.md"), "agents content");
        let b = body(&provider(), &dir);
        assert!(b.contains("agents content") && !b.contains("atelier content"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn none_when_no_instructions_file_present() {
        let dir = temp_dir("none");
        assert!(provider().gather(&dir).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn global_then_repo_root_down_to_project_root() {
        let dir = temp_dir("chain");
        let repo = dir.join("repo");
        let project = repo.join("crates/app");
        fs::create_dir_all(repo.join(".git")).unwrap();
        write(&dir.join("AGENTS.md"), "ABOVE the repository");
        write(&repo.join("AGENTS.md"), "REPO rules");
        // `crates/` has no file of its own; a fallback name counts below it.
        write(&project.join("CLAUDE.md"), "APP rules");
        write(&dir.join("config/atelier/AGENTS.md"), "GLOBAL rules");

        let provider = InstructionsProvider {
            loader: Loader {
                global: Some(dir.join("config/atelier/AGENTS.md")),
                home: None,
            },
        };
        let b = body(&provider, &project);
        let at = |needle: &str| b.find(needle).unwrap_or_else(|| panic!("missing {needle}"));
        assert!(at("GLOBAL rules") < at("REPO rules"));
        assert!(at("REPO rules") < at("APP rules"));
        assert!(b.contains(&format!("### {}", repo.join("AGENTS.md").display())));
        assert!(b.contains(&format!("### {}", project.join("CLAUDE.md").display())));
        assert!(!b.contains("ABOVE"), "nothing above the repository root");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_a_repository_only_the_project_root_counts() {
        let dir = temp_dir("norepo");
        let project = dir.join("project");
        write(&dir.join("AGENTS.md"), "parent rules");
        write(&project.join("AGENTS.md"), "project rules");

        // The temp dir may itself sit inside a checkout; only assert when the
        // project really has no repository above it.
        if !project.ancestors().any(|d| d.join(".git").exists()) {
            let b = body(&provider(), &project);
            assert!(b.contains("project rules") && !b.contains("parent rules"));
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn imports_are_inlined_relative_to_the_importing_file() {
        let dir = temp_dir("imports");
        write(
            &dir.join("AGENTS.md"),
            "before\n@docs/style.md\n  @~/prefs.md  \nafter\n",
        );
        // An import inside an imported file resolves next to *that* file.
        write(&dir.join("docs/style.md"), "style line\n@more.md\n");
        write(&dir.join("docs/more.md"), "more line\n");
        write(&dir.join("home/prefs.md"), "home prefs\n");

        let provider = InstructionsProvider {
            loader: Loader {
                global: None,
                home: Some(dir.join("home")),
            },
        };
        let b = body(&provider, &dir);
        assert!(
            b.ends_with("before\nstyle line\nmore line\nhome prefs\nafter"),
            "{b}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_files_import_only_from_their_repository() {
        let dir = temp_dir("importarea");
        let repo = dir.join("repo");
        write(&repo.join(".git/HEAD"), "");
        write(&dir.join("secret.txt"), "TOP-SECRET\n");
        write(&repo.join("shared.md"), "shared\n");
        write(
            &repo.join("sub/AGENTS.md"),
            &format!(
                "@../shared.md\n@../../secret.txt\n@~/secret.txt\n@{}\n",
                dir.join("secret.txt").display()
            ),
        );
        // The user's own global file is not restricted.
        write(&dir.join("cfg/AGENTS.md"), "@../secret.txt\n");

        let provider = InstructionsProvider {
            loader: Loader {
                global: Some(dir.join("cfg/AGENTS.md")),
                home: Some(dir.clone()),
            },
        };
        let b = body(&provider, &repo.join("sub"));
        assert_eq!(b.matches("TOP-SECRET").count(), 1, "{b}");
        assert_eq!(b.matches("skipped: outside the project").count(), 3, "{b}");
        assert!(b.contains("shared"), "{b}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_whole_line_outside_code_fences_is_an_import() {
        let dir = temp_dir("notimport");
        write(&dir.join("x.md"), "IMPORTED");
        write(
            &dir.join("AGENTS.md"),
            "mail @x.md for help\n```python\n@x.md\n```\n@x.md\n",
        );

        let b = body(&provider(), &dir);
        assert!(b.contains("mail @x.md for help"));
        assert!(b.contains("```python\n@x.md\n```"));
        assert_eq!(b.matches("IMPORTED").count(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn imports_that_cannot_be_honoured_leave_a_note() {
        let dir = temp_dir("badimports");
        write(
            &dir.join("AGENTS.md"),
            "@missing.md\n@loop.md\n@blob.bin\n@huge.md\n@sub\n@~/x.md\nkept\n",
        );
        write(&dir.join("loop.md"), "loop start\n@AGENTS.md\n");
        fs::write(dir.join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
        write(&dir.join("huge.md"), &"x".repeat(BUDGET_BYTES + 1));
        fs::create_dir_all(dir.join("sub")).unwrap();

        let b = body(&provider(), &dir);
        assert!(b.contains("[import @missing.md skipped: not found]"), "{b}");
        assert!(b.contains("loop start\n[import @AGENTS.md skipped: import cycle]"));
        assert!(b.contains("[import @blob.bin skipped: not a text file]"));
        assert!(b.contains("[import @huge.md skipped: larger than 32 KiB]"));
        assert!(b.contains("[import @sub skipped: not a regular file]"));
        assert!(b.contains("[import @~/x.md skipped: no home directory]"));
        assert!(b.ends_with("kept"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn imports_stop_at_the_depth_limit() {
        let dir = temp_dir("depth");
        write(&dir.join("AGENTS.md"), "level 0\n@l1.md\n");
        for n in 1..=MAX_IMPORT_DEPTH + 1 {
            write(
                &dir.join(format!("l{n}.md")),
                &format!("level {n}\n@l{}.md\n", n + 1),
            );
        }

        let b = body(&provider(), &dir);
        assert!(b.contains(&format!("level {MAX_IMPORT_DEPTH}\n")));
        assert!(!b.contains(&format!("level {}\n", MAX_IMPORT_DEPTH + 1)));
        assert!(b.ends_with(&format!(
            "[import @l{}.md skipped: imports nest deeper than {MAX_IMPORT_DEPTH}]",
            MAX_IMPORT_DEPTH + 1
        )));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn over_budget_is_cut_and_says_which_files_suffered() {
        let dir = temp_dir("budget");
        let repo = dir.join("repo");
        let mid = repo.join("a");
        let project = mid.join("b");
        fs::create_dir_all(repo.join(".git")).unwrap();
        write(&repo.join("AGENTS.md"), "small root file");
        // Multi-byte characters, so the cut has to find a char boundary.
        write(&mid.join("AGENTS.md"), &"é".repeat(BUDGET_BYTES));
        write(&project.join("AGENTS.md"), "never shown");

        let b = body(&provider(), &project);
        let (kept, note) = b.split_once("\n[instructions truncated").unwrap();
        assert!(kept.len() <= BUDGET_BYTES);
        assert!(
            kept.len() > BUDGET_BYTES - 8,
            "cut at the budget, not before"
        );
        assert!(kept.contains("small root file"));
        assert!(!b.contains("never shown"));
        assert_eq!(
            note,
            format!(
                " at the 32 KiB budget: {} was cut short; left out entirely: {}]",
                mid.join("AGENTS.md").display(),
                project.join("AGENTS.md").display()
            )
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_within_budget_is_not_annotated() {
        let dir = temp_dir("fits");
        write(&dir.join("AGENTS.md"), &"a".repeat(BUDGET_BYTES / 2));
        assert!(!body(&provider(), &dir).contains("truncated"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_edited_file_is_sent_again_and_an_unchanged_one_is_not() {
        let dir = temp_dir("live");
        write(&dir.join("AGENTS.md"), "@rule.md\n");
        write(&dir.join("rule.md"), "old rule");
        let provider = provider();
        let gather = || vec![provider.gather(&dir).unwrap()];

        let first = dedup_items(&HashMap::new(), gather());
        assert_eq!(first.changed.len(), 1);
        let second = dedup_items(&first.snapshot, gather());
        assert!(
            second.changed.is_empty(),
            "unchanged instructions are not repeated"
        );

        // Even a change in an imported file counts.
        write(&dir.join("rule.md"), "new rule");
        let third = dedup_items(&second.snapshot, gather());
        assert_eq!(third.changed.len(), 1);
        assert!(third.changed[0].body.contains("new rule"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn nested_files_are_found_for_touched_directories_below_the_root() {
        let dir = temp_dir("nested");
        let root = dir.join("project");
        write(&root.join("AGENTS.md"), "ROOT rules");
        write(&root.join("web/AGENTS.md"), "WEB rules\n@extra.md\n");
        write(&root.join("web/extra.md"), "WEB extra");
        write(&root.join("web/ui/.cursorrules"), "UI rules");
        write(&dir.join("AGENTS.md"), "OUTSIDE rules");
        fs::create_dir_all(root.join("src")).unwrap();

        // As the tool layer reports them: every ancestor of the touched file,
        // in no particular order.
        let touched = vec![
            root.join("web/ui"),
            root.clone(),
            dir.clone(),
            root.join("web"),
        ];
        let note = nested(&Loader::default(), &root, &touched).unwrap();
        assert!(note.contains("<nested-instructions>"));
        let at = |needle: &str| {
            note.find(needle)
                .unwrap_or_else(|| panic!("missing {needle}"))
        };
        assert!(
            at("WEB rules\nWEB extra") < at("UI rules"),
            "outer before inner"
        );
        assert!(note.contains(&format!("### {}", root.join("web/AGENTS.md").display())));
        assert!(
            !note.contains("ROOT rules"),
            "the root's file is the provider's"
        );
        assert!(!note.contains("OUTSIDE rules"));

        // A directory without an instruction file adds nothing.
        assert!(nested(&Loader::default(), &root, &[root.join("src")]).is_none());
        assert!(nested(&Loader::default(), &root, &[]).is_none());

        let _ = fs::remove_dir_all(&dir);
    }
}
