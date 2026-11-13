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
//! # Files named in the settings
//!
//! `[instructions] files` in `atelier.toml` names more files — paths or
//! globs ([`InstructionFiles`]). They are instruction files like the others
//! (same headers, imports and budget) and keep the least-specific-first
//! order: the user-global settings' files follow the user-global
//! `AGENTS.md`, the project's follow the root→leaf chain. Within one list
//! the order is the one written, and a glob's matches are sorted.
//!
//! A project's `atelier.toml` is a file of the repository, read without
//! asking, so what it names is confined like a project file's imports: a
//! path that resolves outside the repository (through `..`, an absolute
//! path, `~/`, or a symlink) is not read, and a note says so. The user's
//! own settings may name files anywhere.
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
use crate::settings::Settings;
use globset::GlobBuilder;
use std::path::{Component, Path, PathBuf};

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

/// How far below its fixed leading directories a glob in `[instructions]
/// files` is looked for when it contains `**`. The patterns are resolved on
/// every turn, so the walk has to stay small.
const MAX_GLOB_DEPTH: usize = 8;

/// Directory entries one glob may look at before it is given up on as too
/// broad (`**/*.md` at the root of a large tree).
const MAX_GLOB_VISITS: usize = 10_000;

/// Most files one glob may contribute.
const MAX_GLOB_FILES: usize = 64;

/// The `[instructions] files` lists of the two settings files (see the
/// module docs): paths or globs, kept as written.
#[derive(Debug, Clone, Default)]
pub struct InstructionFiles {
    /// From the user-global `atelier.toml`. Relative ones are resolved
    /// against that file's directory.
    user: Vec<String>,
    /// From the project's `atelier.toml`, relative to the project root.
    project: Vec<String>,
}

impl InstructionFiles {
    /// The lists of the project's settings and the user-global ones.
    pub fn new(project: &Settings, user: &Settings) -> Self {
        Self {
            user: user.instructions.files.clone(),
            project: project.instructions.files.clone(),
        }
    }

    /// [`new`](Self::new), reading the user-global settings from disk — for
    /// a caller that holds only the project's.
    pub fn with_user(project: &Settings) -> Self {
        Self::new(project, &Settings::load_user().unwrap_or_default())
    }

    /// Both lists read from disk, for the project at `root`.
    fn load(root: &Path) -> Self {
        Self::with_user(&Settings::load(root).unwrap_or_default())
    }
}

/// One entry of the list to show the model.
#[derive(Debug, Clone, PartialEq)]
enum Source {
    File {
        path: PathBuf,
        /// It is the user's own — their global file, or one their settings
        /// name — and so may import from anywhere (see [`Loader::import`]).
        own: bool,
    },
    /// Something `[instructions] files` named that is not read, and why:
    /// shown in its place, like an import that can't be honoured.
    Skipped { named: String, why: String },
}

impl Source {
    /// A file of the project.
    fn project(path: PathBuf) -> Self {
        Source::File { path, own: false }
    }

    /// What it is shown as: its header, and its name in the closing note.
    fn name(&self) -> String {
        match self {
            Source::File { path, .. } => path.display().to_string(),
            Source::Skipped { named, .. } => named.clone(),
        }
    }
}

/// The files being expanded, outermost first — to spot an import cycle and
/// bound the nesting — and what the outermost one may import.
struct Trail {
    stack: Vec<PathBuf>,
    /// The file loaded for its name is the user's own: no confinement.
    own: bool,
}

impl Trail {
    fn new(file: &Path, own: bool) -> Self {
        Self {
            stack: vec![canonical(file)],
            own,
        }
    }
}

/// Reads instruction files. Carries the two locations that lie outside the
/// project, so tests can point them at a temp dir instead of the real home.
#[derive(Clone, Default)]
struct Loader {
    /// The user-global instructions file, if a config directory is known.
    global: Option<PathBuf>,
    /// What a leading `~/` in an import expands to.
    home: Option<PathBuf>,
    /// The files the settings name.
    extra: InstructionFiles,
}

impl Loader {
    /// The locations for this process, from `XDG_CONFIG_HOME` and `HOME`.
    fn from_env(extra: InstructionFiles) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".config")));
        Self {
            global: config.map(|c| c.join("atelier").join("AGENTS.md")),
            home,
            extra,
        }
    }

    /// What applies at `root`, least specific first: the user's files, then
    /// the project's (see the module docs). A file reached twice — named by
    /// two patterns, or already loaded for its name — is shown once, where
    /// it first comes.
    fn applicable(&self, root: &Path) -> Vec<Source> {
        let mut found = Vec::new();
        if let Some(path) = self.global.clone().filter(|p| p.is_file()) {
            found.push(Source::File { path, own: true });
        }
        // The user-global settings sit next to the user-global file.
        let config_dir = self.global.as_deref().and_then(Path::parent);
        self.named(&self.extra.user, config_dir, None, &mut found);

        let dirs = chain(root);
        found.extend(dirs.iter().filter_map(|dir| pick(dir)).map(Source::project));
        let repository = canonical(&dirs[0]);
        self.named(
            &self.extra.project,
            Some(root),
            Some(&repository),
            &mut found,
        );

        let mut seen = Vec::new();
        found.retain(|source| match source {
            Source::File { path, .. } => {
                let key = canonical(path);
                let fresh = !seen.contains(&key);
                seen.push(key);
                fresh
            }
            Source::Skipped { .. } => true,
        });
        found
    }

    /// Resolve the `patterns` of one `[instructions] files` list into `out`.
    /// A relative pattern is taken from `base`; with `area`, only files
    /// inside it are read (the project's list — the user's has none).
    fn named(
        &self,
        patterns: &[String],
        base: Option<&Path>,
        area: Option<&Path>,
        out: &mut Vec<Source>,
    ) {
        for pattern in patterns.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
            let skipped = |named: String, why: &str| Source::Skipped {
                named,
                why: why.to_string(),
            };
            let path = match (pattern.strip_prefix("~/"), &self.home, base) {
                (Some(rest), Some(home), _) => home.join(rest),
                (Some(_), None, _) => {
                    out.push(skipped(pattern.to_string(), "no home directory"));
                    continue;
                }
                // `join` keeps an absolute pattern as it is.
                (None, _, Some(base)) => base.join(pattern),
                (None, _, None) => PathBuf::from(pattern),
            };
            let inside = |real: &Path| area.is_none_or(|area| real.starts_with(area));

            if !is_glob(pattern) {
                // Resolved first, so that `..` and symlinks are judged by
                // where they lead; what can't be resolved isn't there.
                out.push(match std::fs::canonicalize(&path) {
                    Ok(real) if !inside(&real) => {
                        skipped(pattern.to_string(), "outside the project")
                    }
                    Ok(_) => Source::File {
                        path,
                        own: area.is_none(),
                    },
                    Err(_) => skipped(pattern.to_string(), "not found"),
                });
                continue;
            }

            let (dir, glob) = split_glob(&path);
            let Ok(matcher) = GlobBuilder::new(&glob)
                // `*` stays within one directory; `**` is how to cross them.
                .literal_separator(true)
                .build()
                .map(|g| g.compile_matcher())
            else {
                out.push(skipped(pattern.to_string(), "not a valid pattern"));
                continue;
            };
            // A glob that matches nothing says nothing — unlike a plain
            // path, it does not promise a file.
            let Ok(real_dir) = std::fs::canonicalize(&dir) else {
                continue;
            };
            if !inside(&real_dir) {
                out.push(skipped(pattern.to_string(), "outside the project"));
                continue;
            }
            let depth = if glob.contains("**") {
                MAX_GLOB_DEPTH
            } else {
                glob.split('/').count()
            };
            let mut walk = Walk::default();
            walk.files(&dir, depth);
            let mut matches: Vec<PathBuf> = walk
                .found
                .into_iter()
                .filter(|file| matcher.is_match(file.strip_prefix(&dir).unwrap_or(file)))
                .collect();
            matches.sort();
            let surplus = matches.len().saturating_sub(MAX_GLOB_FILES);
            matches.truncate(MAX_GLOB_FILES);
            for path in matches {
                // A match may still be a symlink leading out.
                out.push(if inside(&canonical(&path)) {
                    Source::File {
                        path,
                        own: area.is_none(),
                    }
                } else {
                    skipped(path.display().to_string(), "outside the project")
                });
            }
            if walk.visits > MAX_GLOB_VISITS {
                let why = format!(
                    "too broad — gave up after {MAX_GLOB_VISITS} directory entries, \
                     so some matches may be missing"
                );
                out.push(skipped(pattern.to_string(), &why));
            } else if surplus > 0 {
                let why = format!(
                    "{surplus} more file(s) match than the {MAX_GLOB_FILES} one pattern may add"
                );
                out.push(skipped(pattern.to_string(), &why));
            }
        }
    }

    /// Renders `sources` in order, each under a header naming it, within
    /// [`BUDGET_BYTES`]. `None` when there is nothing to show.
    fn render(&self, sources: &[Source]) -> Option<String> {
        let mut out = String::new();
        let mut cut: Option<String> = None;
        let mut dropped: Vec<String> = Vec::new();
        for source in sources {
            if cut.is_some() {
                dropped.push(source.name());
                continue;
            }
            let content = match source {
                Source::File { path, own } => match read_text(path, None) {
                    Ok(text) => self.expand(path, &text, &mut Trail::new(path, *own)),
                    Err(why) => format!("[skipped: {why}]"),
                },
                Source::Skipped { why, .. } => format!("[skipped: {why}]"),
            };
            let section = format!("### {}\n{}\n\n", source.name(), content.trim());
            let room = BUDGET_BYTES - out.len();
            if section.len() <= room {
                out.push_str(&section);
            } else {
                out.push_str(&section[..floor_char_boundary(&section, room)]);
                cut = Some(source.name());
            }
        }
        let mut out = out.trim_end().to_string();
        if let Some(file) = cut {
            out.push_str(&format!(
                "\n[instructions truncated at the {} KiB budget: {file} was cut short",
                BUDGET_BYTES / 1024,
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
    /// import. `trail` is the chain of files currently being expanded.
    fn expand(&self, file: &Path, text: &str, trail: &mut Trail) -> String {
        let mut out = String::with_capacity(text.len());
        // The fence character (` or ~) of the code block we're inside, if any.
        let mut fence: Option<char> = None;
        for line in text.lines() {
            let trimmed = line.trim();
            let marker = fence_marker(trimmed);
            match (fence, marker) {
                (None, Some(c)) => fence = Some(c),
                (Some(open), Some(c)) if open == c => fence = None,
                _ => {}
            }
            match import_target(trimmed).filter(|_| fence.is_none() && marker.is_none()) {
                Some(target) => match self.import(file, target, trail) {
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
    fn import(&self, from: &Path, target: &str, trail: &mut Trail) -> Result<String, String> {
        if trail.stack.len() > MAX_IMPORT_DEPTH {
            return Err(format!("imports nest deeper than {MAX_IMPORT_DEPTH}"));
        }
        let path = match target.strip_prefix("~/") {
            Some(rest) => self.home.as_ref().ok_or("no home directory")?.join(rest),
            // `join` keeps an absolute target as it is.
            None => from.parent().unwrap_or(Path::new("")).join(target),
        };
        let key = canonical(&path);
        if trail.stack.contains(&key) {
            return Err("import cycle".to_string());
        }
        // A repository's instruction files are loaded without asking, so
        // they may only pull in files of that repository: otherwise a cloned
        // project could put any text file of the user's (credentials, keys)
        // into the prompt. The user's own files may import anything.
        if !trail.own && !key.starts_with(import_area(&trail.stack[0])) {
            return Err("outside the project".to_string());
        }
        let text = read_text(&path, Some(BUDGET_BYTES as u64))?;
        trail.stack.push(key);
        let content = self.expand(&path, &text, trail);
        trail.stack.pop();
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
pub fn pick(dir: &Path) -> Option<PathBuf> {
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

/// Whether an `[instructions] files` entry is a pattern rather than a path.
fn is_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '[', '{'])
}

/// Cut the pattern `path` into the directory its leading plain components
/// name — where to look — and the glob for what is below it, `/`-separated.
fn split_glob(path: &Path) -> (PathBuf, String) {
    let mut dir = PathBuf::new();
    let mut glob: Vec<String> = Vec::new();
    for component in path.components() {
        let text = component.as_os_str().to_string_lossy();
        let plain = !matches!(component, Component::Normal(_)) || !is_glob(&text);
        if glob.is_empty() && plain {
            dir.push(component);
        } else {
            glob.push(text.into_owned());
        }
    }
    (dir, glob.join("/"))
}

/// A bounded walk for the files a glob may match.
#[derive(Default)]
struct Walk {
    found: Vec<PathBuf>,
    /// Directory entries looked at; past [`MAX_GLOB_VISITS`] the walk stops.
    visits: usize,
}

impl Walk {
    /// Collect the files up to `depth` levels below `dir`. Symlinked
    /// directories are not entered (no loops, no walking out of the tree),
    /// and neither is `.git`.
    fn files(&mut self, dir: &Path, depth: usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            self.visits += 1;
            if self.visits > MAX_GLOB_VISITS {
                return;
            }
            // Does not follow symlinks: a link is neither a directory here…
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if depth > 1 && entry.file_name() != ".git" {
                    self.files(&entry.path(), depth - 1);
                }
            // …nor a file, so it is asked what it leads to.
            } else if entry.path().is_file() {
                self.found.push(entry.path());
            }
        }
    }
}

/// The fence character (`` ` `` or `~`) of a line (already trimmed) that
/// opens or closes a fenced code block.
fn fence_marker(line: &str) -> Option<char> {
    ['`', '~']
        .into_iter()
        .find(|c| line.starts_with(&c.to_string().repeat(3)))
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
    /// `files` are the `[instructions] files` of the settings.
    pub fn new(files: InstructionFiles) -> Self {
        Self {
            loader: Loader::from_env(files),
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
    // Only the directories' own files are rendered: no settings needed.
    nested(&Loader::from_env(InstructionFiles::default()), root, dirs)
}

/// The instruction files a session at `root` loads up front, least specific
/// first — what [`InstructionsProvider`] renders, the files the settings
/// name included. For `atelier doctor`.
pub fn instruction_files(root: &Path) -> Vec<PathBuf> {
    let sources = Loader::from_env(InstructionFiles::load(root)).applicable(root);
    let file = |source| match source {
        Source::File { path, .. } => Some(path),
        Source::Skipped { .. } => None,
    };
    sources.into_iter().filter_map(file).collect()
}

/// The titles (compared ignoring case) of the section an instruction file
/// gives a reviewer.
const REVIEW_TITLES: &[&str] = &["review", "code review"];

/// The most of the review sections handed to a reviewer.
const REVIEW_SECTION_BYTES: usize = 8 * 1024;

/// What the instruction files that apply at `root` say about reviewing: the
/// text under their headings titled "Review" or "Code review" (any level,
/// any case), in file order — or `None` when no file has such a section.
///
/// For `atelier review`, whose reviewer should follow the project's own
/// idea of what matters in a review. The whole files are in front of it too
/// (standing context); this lifts the part addressed to it into its
/// instructions, where it is not one rule among fifty.
pub fn review_section(root: &Path) -> Option<String> {
    review_sections(&Loader::from_env(InstructionFiles::load(root)), root)
}

fn review_sections(loader: &Loader, root: &Path) -> Option<String> {
    let mut found = Vec::new();
    for source in loader.applicable(root) {
        let Source::File { path, own } = source else {
            continue;
        };
        let Ok(text) = read_text(&path, None) else {
            continue;
        };
        let text = loader.expand(&path, &text, &mut Trail::new(&path, own));
        found.extend(sections_titled(&text, REVIEW_TITLES));
    }
    let mut all = found.join("\n\n");
    if all.len() > REVIEW_SECTION_BYTES {
        all.truncate(floor_char_boundary(&all, REVIEW_SECTION_BYTES));
        all.push_str("\n[cut: the rest is in the instruction files]");
    }
    (!all.is_empty()).then_some(all)
}

/// The bodies of the sections of the Markdown `text` whose heading is one
/// of `titles`: everything up to the next heading of the same or a higher
/// level, sub-headings included. A `#` line inside a fenced code block is a
/// comment in a code sample, not a heading.
fn sections_titled(text: &str, titles: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    // The level of the wanted heading we are under, and its body so far.
    let mut open: Option<(usize, String)> = None;
    let mut fence: Option<char> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        let marker = fence_marker(trimmed);
        match (fence, marker) {
            (None, Some(c)) => fence = Some(c),
            (Some(o), Some(c)) if o == c => fence = None,
            _ => {}
        }
        let heading = heading(trimmed).filter(|_| fence.is_none() && marker.is_none());
        if let Some((level, title)) = heading {
            if open.as_ref().is_some_and(|(at, _)| level <= *at) {
                out.extend(open.take().map(|(_, body)| body));
            }
            if open.is_none() && titles.iter().any(|t| title.eq_ignore_ascii_case(t)) {
                open = Some((level, String::new()));
                continue;
            }
        }
        if let Some((_, body)) = &mut open {
            body.push_str(line);
            body.push('\n');
        }
    }
    out.extend(open.map(|(_, body)| body));
    out.iter()
        .map(|body| body.trim().to_string())
        .filter(|body| !body.is_empty())
        .collect()
}

/// The level and title of an ATX heading line (`## Title`), already trimmed.
fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.bytes().take_while(|b| *b == b'#').count();
    let rest = &line[level..];
    if !(1..=6).contains(&level) || !rest.starts_with(char::is_whitespace) {
        return None;
    }
    // A closing run of `#` and a trailing colon are decoration.
    let title = rest.trim().trim_end_matches('#').trim_end();
    Some((level, title.trim_end_matches(':')))
}

fn nested(loader: &Loader, root: &Path, dirs: &[PathBuf]) -> Option<String> {
    let mut below: Vec<&PathBuf> = dirs
        .iter()
        .filter(|dir| dir.starts_with(root) && dir.as_path() != root)
        .collect();
    // Path order puts a directory before those inside it: root→leaf again.
    below.sort();
    below.dedup();
    let picked = below.into_iter().filter_map(|dir| pick(dir));
    let files: Vec<Source> = picked.map(Source::project).collect();
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
                ..Loader::default()
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
                home: Some(dir.join("home")),
                ..Loader::default()
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
                ..Loader::default()
            },
        };
        let b = body(&provider, &repo.join("sub"));
        assert_eq!(b.matches("TOP-SECRET").count(), 1, "{b}");
        assert_eq!(b.matches("skipped: outside the project").count(), 3, "{b}");
        assert!(b.contains("shared"), "{b}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// A loader with `project` as the project settings' list and `user` as
    /// the user-global one, whose config directory is `dir/cfg`.
    fn with_files(dir: &Path, user: &[&str], project: &[&str]) -> InstructionsProvider {
        let list = |patterns: &[&str]| patterns.iter().map(|p| p.to_string()).collect();
        InstructionsProvider {
            loader: Loader {
                global: Some(dir.join("cfg/AGENTS.md")),
                home: Some(dir.join("home")),
                extra: InstructionFiles {
                    user: list(user),
                    project: list(project),
                },
            },
        }
    }

    #[test]
    fn files_named_in_the_settings_follow_the_ones_found_by_name() {
        let dir = temp_dir("named");
        let repo = dir.join("repo");
        write(&repo.join(".git/HEAD"), "");
        write(&repo.join("AGENTS.md"), "CHAIN rules");
        write(&repo.join("docs/conventions.md"), "CONVENTIONS\n@more.md\n");
        write(&repo.join("docs/more.md"), "imported by a named file");
        write(&repo.join("team/b.md"), "TEAM-B");
        write(&repo.join("team/a.md"), "TEAM-A");
        write(&repo.join("team/notes.txt"), "NOT-MARKDOWN");
        write(&repo.join("team/deep/c.md"), "TOO-DEEP");
        write(&dir.join("cfg/AGENTS.md"), "GLOBAL rules");
        write(&dir.join("cfg/mine.md"), "USER-RELATIVE");
        write(&dir.join("home/notes/style.md"), "USER-HOME");

        let provider = with_files(
            &dir,
            &["mine.md", "~/notes/*.md"],
            // A glob before a path: the order written, not sorted across.
            &["team/*.md", "docs/conventions.md", "AGENTS.md", "none/*.md"],
        );
        let b = body(&provider, &repo);
        let order = [
            "GLOBAL rules",
            "USER-RELATIVE",
            "USER-HOME",
            "CHAIN rules",
            "TEAM-A",
            "TEAM-B",
            "CONVENTIONS\nimported by a named file",
        ];
        let at: Vec<usize> = order
            .iter()
            .map(|needle| b.find(needle).unwrap_or_else(|| panic!("{needle} in {b}")))
            .collect();
        assert!(at.is_sorted(), "{b}");
        assert!(b.contains(&format!("### {}\nTEAM-A", repo.join("team/a.md").display())));
        assert!(
            !b.contains("NOT-MARKDOWN") && !b.contains("TOO-DEEP"),
            "{b}"
        );
        // Named again, the root file is not shown twice; a glob without a
        // match says nothing.
        assert_eq!(b.matches("CHAIN rules").count(), 1, "{b}");
        assert!(!b.contains("none/"), "{b}");

        // `**` crosses directories.
        let b = body(&with_files(&dir, &[], &["team/**/*.md"]), &repo);
        assert!(b.contains("TOO-DEEP") && b.contains("TEAM-A"), "{b}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_projects_settings_cannot_name_files_outside_the_repository() {
        let dir = temp_dir("named-area");
        let repo = dir.join("repo");
        let project = repo.join("app");
        write(&repo.join(".git/HEAD"), "");
        write(&repo.join("shared.md"), "SHARED in the repository");
        write(&project.join("ok.md"), "OK");
        write(&dir.join("secret.md"), "TOP-SECRET");
        write(&dir.join("home/key.md"), "TOP-SECRET");
        write(&dir.join("outside/x.md"), "TOP-SECRET");
        std::os::unix::fs::symlink(dir.join("secret.md"), project.join("link.md")).unwrap();
        fs::create_dir_all(project.join("linked")).unwrap();
        std::os::unix::fs::symlink(dir.join("secret.md"), project.join("linked/l.md")).unwrap();
        let absolute = dir.join("secret.md").display().to_string();
        let patterns = [
            "ok.md",
            // Above the project root, still in the repository: fine.
            "../shared.md",
            "../../secret.md",
            &absolute,
            "~/key.md",
            "link.md",
            "../../outside/*.md",
            "linked/*.md",
            "missing.md",
            "bad/[.md",
        ];

        let b = body(&with_files(&dir, &[], &patterns), &project);
        assert!(
            b.contains("OK") && b.contains("SHARED in the repository"),
            "{b}"
        );
        assert!(!b.contains("TOP-SECRET"), "{b}");
        for named in [
            "../../secret.md",
            &absolute,
            "~/key.md",
            "link.md",
            "../../outside/*.md",
            &project.join("linked/l.md").display().to_string(),
        ] {
            let note = format!("### {named}\n[skipped: outside the project]");
            assert!(b.contains(&note), "{note} in {b}");
        }
        assert!(b.contains("### missing.md\n[skipped: not found]"), "{b}");
        assert!(
            b.contains("### bad/[.md\n[skipped: not a valid pattern]"),
            "{b}"
        );

        // The same paths in the user's own settings are theirs to name, and
        // such a file imports from anywhere.
        write(&dir.join("home/key.md"), "@../outside/x.md\n");
        let b = body(&with_files(&dir, &[&absolute, "~/key.md"], &[]), &project);
        assert_eq!(b.matches("TOP-SECRET").count(), 2, "{b}");
        assert!(!b.contains("skipped"), "{b}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_glob_adds_a_bounded_number_of_files_and_says_so() {
        let dir = temp_dir("named-many");
        for n in 0..MAX_GLOB_FILES + 3 {
            write(
                &dir.join(format!("rules/{n:03}.md")),
                &format!("RULE-{n:03}"),
            );
        }
        let b = body(&with_files(&dir, &[], &["rules/*.md"]), &dir);
        assert!(
            b.contains("RULE-000") && b.contains("RULE-063"),
            "sorted, first kept"
        );
        assert!(!b.contains("RULE-064"));
        assert!(
            b.ends_with(
                "### rules/*.md\n[skipped: 3 more file(s) match than the 64 one pattern may add]"
            ),
            "{b}"
        );
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

    #[test]
    fn review_sections_are_found_at_any_level_and_end_at_the_next_peer() {
        let text = "# Project\nintro\n\n## Code Review:\nCheck locking.\n\n### Details\nmore\n\n\
                    ```sh\n# not a heading\n```\n\n## Style\ntabs\n\n# review\nTop-level too.\n";
        assert_eq!(
            sections_titled(text, REVIEW_TITLES),
            [
                "Check locking.\n\n### Details\nmore\n\n```sh\n# not a heading\n```",
                "Top-level too."
            ]
        );
        assert!(sections_titled("## Reviewers\nalice\n", REVIEW_TITLES).is_empty());
        assert!(sections_titled("#Review\nno space\n", REVIEW_TITLES).is_empty());
        // An empty section says nothing.
        assert!(sections_titled("## Review\n\n## Next\nx\n", REVIEW_TITLES).is_empty());
    }

    #[test]
    fn review_section_is_read_from_the_applicable_files() {
        let dir = temp_dir("review-section");
        write(&dir.join("AGENTS.md"), "Rules.\n\n## Review\n@review.md\n");
        write(
            &dir.join("review.md"),
            "Every unsafe block needs a comment.",
        );
        let loader = Loader::default();
        assert_eq!(
            review_sections(&loader, &dir).as_deref(),
            Some("Every unsafe block needs a comment.")
        );
        write(&dir.join("AGENTS.md"), "Rules only.\n");
        assert_eq!(review_sections(&loader, &dir), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
