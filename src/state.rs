//! Where atelier keeps what it writes for a project: never in the project.
//!
//! A project directory holds the user's files. What atelier itself writes
//! while working there — the saved session, the `todo` list, the project's
//! memory notes, tool output too long for the conversation, the worktrees
//! of isolated subagents — goes to a directory of the user's, one per
//! project, named after the project's path:
//!
//! ```text
//! $XDG_STATE_HOME/atelier/projects/<slug>/      (else ~/.local/state/atelier/…)
//!     session.json      the conversation, for --continue
//!     claude-session    the Claude backend's session id
//!     todos.json        the todo tool's list
//!     memory/           the project's memory notes
//!     tool-output/      output a tool cut short, kept whole
//!     worktrees/<id>/   isolated subagents' git worktrees
//! ```
//!
//! `<slug>` is the project's canonical absolute path with every `/`
//! replaced by `-`: `/home/me/proj` is `-home-me-proj`. A path too long
//! for a file name is cut and ends in a hash of the whole of it.
//! `ATELIER_STATE_DIR` names the directory that holds `projects/` instead
//! (tests that start the program use it, so none touches the real home).
//!
//! With no absolute `ATELIER_STATE_DIR`, `XDG_STATE_HOME` or `HOME` there
//! is nowhere to keep anything: [`dir`] is `None`, and each caller does
//! what it does when its file cannot be written.
//!
//! What a project may still hold is what its *user* wrote for atelier to
//! read — `.atelier/commands/`, `.atelier/skills/`, `.atelier/agents/`,
//! `.atelier/instructions.md`, `atelier.toml`. atelier creates none of it.
//!
//! # State from before
//!
//! Earlier versions kept all this in the project's `.atelier/`. The first
//! time a project with no state directory is used, its `session.json`,
//! `claude-session`, `todos.json` and `memory/` are **copied** over
//! ([`migrate`]); the project is left exactly as it was.
//!
//! # A subagent's worktree
//!
//! A worktree is a project root like any other, so it has a state
//! directory of its own, named after its path, which goes when the
//! worktree does ([`forget`]). Memory is the exception: notes are about
//! the project, so a worktree under `<state>/worktrees/` reads and writes
//! the notes of the project it was made for ([`shared`]).

use std::path::{Path, PathBuf};

/// Names the directory that holds `projects/`, instead of the one under
/// `XDG_STATE_HOME` or `HOME`.
pub const ENV_DIR: &str = "ATELIER_STATE_DIR";

/// The directory under the base that holds one directory per project.
const PROJECTS: &str = "projects";

/// The directory, in a project's state, of its subagents' worktrees.
pub const WORKTREES: &str = "worktrees";

/// Where a project kept atelier's state before it moved here.
pub const LEGACY: &str = ".atelier";

/// The longest slug, in bytes: file names stop at 255 on most file
/// systems, and a name beside it (a temporary one) needs room too.
const MAX_SLUG_BYTES: usize = 200;

/// The directory that holds `projects/`, from the environment as `var`
/// gives it. Only absolute paths count, as everywhere else.
fn base_from(var: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let abs = |key: &str| var(key).map(PathBuf::from).filter(|p| p.is_absolute());
    abs(ENV_DIR).or_else(|| {
        abs("XDG_STATE_HOME")
            .or_else(|| abs("HOME").map(|h| h.join(".local/state")))
            .map(|state| state.join("atelier"))
    })
}

/// The directory that holds `projects/`, resolved where it exists (project
/// paths are compared with it).
#[cfg(not(test))]
fn base() -> Option<PathBuf> {
    let base = base_from(&|key| std::env::var_os(key))?;
    Some(base.canonicalize().unwrap_or(base))
}

/// In unit tests: a directory of this test process's own, whatever the
/// environment says — tests run in parallel in one process and none may
/// write under the real home. Directories left by test processes that are
/// gone are removed on the way.
#[cfg(test)]
fn base() -> Option<PathBuf> {
    use std::sync::OnceLock;
    const PREFIX: &str = "atelier-test-state-";
    static BASE: OnceLock<PathBuf> = OnceLock::new();
    let base = BASE.get_or_init(|| {
        let tmp = std::env::temp_dir();
        for entry in std::fs::read_dir(&tmp).into_iter().flatten().flatten() {
            let name = entry.file_name();
            let pid = name.to_str().and_then(|n| n.strip_prefix(PREFIX));
            if let Some(pid) = pid
                && Path::new("/proc/self").exists()
                && !Path::new("/proc").join(pid).exists()
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        let dir = tmp.join(format!("{PREFIX}{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a state directory for the tests");
        dir.canonicalize()
            .expect("resolving the tests' state directory")
    });
    Some(base.clone())
}

/// `root` as its state directory is named after it: resolved, or, for a
/// directory that does not exist, made absolute.
fn canonical(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| {
        let absolute = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
        crate::tools::normalize(&absolute)
    })
}

/// The name of the state directory of the project at the absolute path
/// `path`: the path with its separators turned into `-`.
pub fn slug(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut slug: String = text
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            c => c,
        })
        .collect();
    // A path that is too long, or that is not text (two of those can read
    // the same), is told apart by a hash of its bytes.
    let exact = path.to_str().is_some();
    if slug.len() > MAX_SLUG_BYTES || !exact {
        let mut end = slug.len().min(MAX_SLUG_BYTES - 17);
        while !slug.is_char_boundary(end) {
            end -= 1;
        }
        slug.truncate(end);
        slug.push_str(&format!(
            "-{:016x}",
            fnv1a(path.as_os_str().as_encoded_bytes())
        ));
    }
    slug
}

/// FNV-1a, 64 bits: the same number for the same path in every build,
/// which the standard library's hasher does not promise.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The state directory of the project at `root`, whether or not it exists
/// yet; `None` when the environment names no place for it. What an earlier
/// version left in the project's `.atelier/` is copied over the first time.
pub fn dir(root: &Path) -> Option<PathBuf> {
    let projects = base()?.join(PROJECTS);
    let root = canonical(root);
    let dir = projects.join(slug(&root));
    // A worktree of ours is a fresh checkout: what it has under `.atelier/`
    // is the repository's, not state anyone left there.
    if owner(&root, &projects).is_none() && std::fs::symlink_metadata(&dir).is_err() {
        migrate(&root, &dir);
    }
    Some(dir)
}

/// The state directory of the project `root` belongs to: its own, or, for
/// a subagent's worktree, that of the project the worktree was made for.
pub fn shared(root: &Path) -> Option<PathBuf> {
    let projects = base()?.join(PROJECTS);
    owner(&canonical(root), &projects).or_else(|| dir(root))
}

/// For a root that is a subagent's worktree — `<projects>/<slug>/worktrees/
/// <id>`, or a directory in one: the state directory it is in.
fn owner(root: &Path, projects: &Path) -> Option<PathBuf> {
    let mut parts = root.strip_prefix(projects).ok()?.components();
    let name = parts.next()?;
    if parts.next()?.as_os_str() != WORKTREES {
        return None;
    }
    parts.next()?;
    Some(projects.join(name))
}

/// Remove the state directory of the project at `root`: for a root that
/// is itself gone (a subagent's worktree that was removed).
pub fn forget(root: &Path) {
    if let Some(projects) = base().map(|b| b.join(PROJECTS)) {
        let _ = std::fs::remove_dir_all(projects.join(slug(&canonical(root))));
    }
}

/// The files of a project's `.atelier/` that are carried over, and the one
/// directory. Spilled tool output and worktrees are not: the first is
/// scratch, the second are checkouts git knows by their path.
const CARRIED_FILES: [&str; 3] = ["session.json", "claude-session", "todos.json"];
const CARRIED_DIR: &str = "memory";

/// Copy what an earlier version kept in `<root>/.atelier/` to `dir`, which
/// does not exist. Nothing in the project is changed. Only real files and
/// directories are read — a link in a project could lead anywhere — and
/// the copy appears whole or not at all, so two processes starting at once
/// do not find half of it. Best effort: what cannot be copied is not.
fn migrate(root: &Path, dir: &Path) {
    let legacy = root.join(LEGACY);
    let is_file = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file());
    let is_dir = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    if !is_dir(&legacy) {
        return;
    }
    let files: Vec<&str> = CARRIED_FILES
        .into_iter()
        .filter(|name| is_file(&legacy.join(name)))
        .collect();
    let notes: Vec<PathBuf> = match is_dir(&legacy.join(CARRIED_DIR)) {
        true => std::fs::read_dir(legacy.join(CARRIED_DIR))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_file(p))
            .collect(),
        false => Vec::new(),
    };
    if files.is_empty() && notes.is_empty() {
        return;
    }
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        return;
    };
    let partial = parent.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let copy = || -> std::io::Result<()> {
        std::fs::create_dir_all(&partial)?;
        for name in &files {
            std::fs::copy(legacy.join(name), partial.join(name))?;
        }
        if !notes.is_empty() {
            std::fs::create_dir(partial.join(CARRIED_DIR))?;
        }
        for note in &notes {
            if let Some(name) = note.file_name() {
                std::fs::copy(note, partial.join(CARRIED_DIR).join(name))?;
            }
        }
        // Fails if another process got there first: its copy stands.
        std::fs::rename(&partial, dir)
    };
    if copy().is_err() {
        let _ = std::fs::remove_dir_all(&partial);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atelier-test-statedir-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn the_slug_is_the_path_with_hyphens() {
        assert_eq!(slug(Path::new("/home/me/proj")), "-home-me-proj");
        assert_eq!(slug(Path::new("/home/me/my proj.d")), "-home-me-my proj.d");
        assert_eq!(slug(Path::new("/")), "-");
    }

    #[test]
    fn a_long_path_is_cut_and_stays_its_own() {
        let long = format!("/home/me/{}", "é".repeat(150));
        let other = format!("{long}/b");
        let (a, b) = (slug(Path::new(&long)), slug(Path::new(&other)));
        assert!(
            a.len() <= MAX_SLUG_BYTES && b.len() <= MAX_SLUG_BYTES,
            "{a}"
        );
        assert!(a.starts_with("-home-me-éé"), "{a}");
        assert_ne!(a, b);
        // The same path, the same name: in this run and in the next.
        assert_eq!(a, slug(Path::new(&long)));
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        // A path within the limit is not touched.
        let fits = format!("/{}", "a".repeat(MAX_SLUG_BYTES - 1));
        assert_eq!(slug(Path::new(&fits)), format!("-{}", "a".repeat(199)));
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_text_gets_a_hash() {
        use std::os::unix::ffi::OsStrExt;
        let a = Path::new(std::ffi::OsStr::from_bytes(b"/p/\xff"));
        let b = Path::new(std::ffi::OsStr::from_bytes(b"/p/\xfe"));
        assert_ne!(slug(a), slug(b));
        assert!(slug(a).starts_with("-p-"));
    }

    #[test]
    fn the_base_comes_from_the_environment() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                vars.iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| std::ffi::OsString::from(v))
            }
        };
        let base = |vars| base_from(&env(vars));
        assert_eq!(
            base(&[("HOME", "/home/me")]),
            Some(PathBuf::from("/home/me/.local/state/atelier"))
        );
        assert_eq!(
            base(&[("HOME", "/home/me"), ("XDG_STATE_HOME", "/s")]),
            Some(PathBuf::from("/s/atelier"))
        );
        assert_eq!(
            base(&[("XDG_STATE_HOME", "/s"), (ENV_DIR, "/elsewhere")]),
            Some(PathBuf::from("/elsewhere"))
        );
        // Relative paths name nothing; then there is no base at all.
        assert_eq!(
            base(&[(ENV_DIR, "rel"), ("XDG_STATE_HOME", "s"), ("HOME", "/h")]),
            Some(PathBuf::from("/h/.local/state/atelier"))
        );
        assert_eq!(base(&[("HOME", "me")]), None);
        assert_eq!(base(&[]), None);
    }

    #[test]
    fn a_project_has_a_directory_named_after_its_path_and_nothing_is_made() {
        let root = scratch("named");
        let dir = dir(&root).unwrap();
        assert_eq!(dir.file_name().unwrap().to_str().unwrap(), slug(&root));
        assert_eq!(dir.parent().unwrap().file_name().unwrap(), PROJECTS);
        assert!(!dir.starts_with(&root));
        // Asking creates nothing, there or in the project.
        assert!(!dir.exists());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        // However the root is spelled.
        assert_eq!(super::dir(&root.join("sub/..")).unwrap(), dir);
        assert_eq!(shared(&root).unwrap(), dir);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn what_an_earlier_version_left_in_the_project_is_copied_once() {
        let root = scratch("legacy");
        let legacy = root.join(LEGACY);
        std::fs::create_dir_all(legacy.join("memory")).unwrap();
        std::fs::create_dir_all(legacy.join("tool-output")).unwrap();
        std::fs::create_dir_all(legacy.join("worktrees/agent-1")).unwrap();
        std::fs::create_dir_all(legacy.join("commands")).unwrap();
        std::fs::write(legacy.join("session.json"), "{}").unwrap();
        std::fs::write(legacy.join("claude-session"), "abc").unwrap();
        std::fs::write(legacy.join("todos.json"), "[]").unwrap();
        std::fs::write(legacy.join("memory/note.md"), "a note").unwrap();
        std::fs::write(legacy.join("tool-output/ab12cd.txt"), "spilled").unwrap();
        std::fs::write(legacy.join("commands/go.md"), "go").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hostname", legacy.join("memory/link.md")).unwrap();

        let dir = dir(&root).unwrap();
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
        assert_eq!(read("session.json"), "{}");
        assert_eq!(read("claude-session"), "abc");
        assert_eq!(read("todos.json"), "[]");
        assert_eq!(read("memory/note.md"), "a note");
        for not in ["tool-output", "worktrees", "commands", "memory/link.md"] {
            assert!(!dir.join(not).exists(), "{not}");
        }
        // Copied, not moved.
        assert!(legacy.join("session.json").is_file());
        assert!(legacy.join("memory/note.md").is_file());
        // Once: the project's copy no longer counts.
        std::fs::write(legacy.join("todos.json"), "[1]").unwrap();
        std::fs::remove_file(dir.join("claude-session")).unwrap();
        let again = super::dir(&root).unwrap();
        assert_eq!(
            std::fs::read_to_string(again.join("todos.json")).unwrap(),
            "[]"
        );
        assert!(!again.join("claude-session").exists());

        forget(&root);
        assert!(!dir.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_project_with_nothing_to_carry_gets_no_directory() {
        let root = scratch("nothing");
        std::fs::create_dir_all(root.join(".atelier/commands")).unwrap();
        std::fs::create_dir_all(root.join(".atelier/tool-output")).unwrap();
        assert!(!dir(&root).unwrap().exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_worktree_has_its_own_state_and_its_projects_memory() {
        let root = scratch("owner");
        let state = dir(&root).unwrap();
        let worktree = state.join(WORKTREES).join("agent-1");
        // A checkout of a repository that committed its `.atelier/`.
        std::fs::create_dir_all(worktree.join("sub/.atelier")).unwrap();
        std::fs::write(worktree.join("sub/.atelier/todos.json"), "[]").unwrap();
        for inside in [worktree.clone(), worktree.join("sub")] {
            let own = dir(&inside).unwrap();
            assert_ne!(own, state);
            assert_eq!(own.parent(), state.parent());
            assert!(!own.exists(), "nothing is carried over from a worktree");
            assert_eq!(shared(&inside).unwrap(), state);
        }
        // Not everything in a state directory is a worktree.
        assert_ne!(shared(&state.join("memory")).unwrap(), state);
        std::fs::remove_dir_all(&state).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }
}
