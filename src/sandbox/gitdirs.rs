//! Git directories the guest needs besides the project's files: which one,
//! if any, is mounted from outside the project, and what stays read-only in
//! each.
//!
//! A checkout's git data is not always in its `.git`. In a linked worktree
//! and in a submodule `.git` is a file, `gitdir: <path>`, and the data is in
//! the main repository (`<main>/.git/worktrees/<name>`, sharing
//! `<main>/.git`) or the superproject (`<super>/.git/modules/<name>`). The
//! guest sees the project and nothing else, so git there would find no
//! repository. [`plan`] decides what to add.
//!
//! # The rule
//!
//! The `.git` file comes with the project, and a project is not trusted: a
//! cloned or unpacked one may carry a `.git` file naming `~/.ssh` or `/`.
//! So the file is never believed by itself. A directory is mounted only if
//! it **names this project back**, in a file the project could not have
//! written:
//!
//! - a linked worktree's `<common>/worktrees/<name>/gitdir` holds the path
//!   of the project's `.git` file (git writes it at `git worktree add`);
//! - a submodule's — or a `git init --separate-git-dir`'s — `config` has
//!   `core.worktree` pointing at the project.
//!
//! and only if it has a git directory's shape (`HEAD`, `objects`, `refs`),
//! with a worktree's `commondir` leading exactly two levels up, to the
//! directory `worktrees/<name>` is in. Whoever can write such a file
//! outside the project already has what the mount would give. A directory
//! that contains the project or is inside it is never mounted: the first
//! would bring a checkout's files along, the second is there already.
//!
//! What is mounted is the git directory alone — for a worktree, the common
//! directory (`<main>/.git`), never `<main>` — at the path the `.git` file
//! spells, which is the path git in the guest will look for.
//!
//! # What the guest may write there
//!
//! As in the project's own `.git`, what decides which programs the host
//! runs later is read-only ([`GIT_DIR_FILES`]): `config`, `config.worktree`
//! and `hooks`, and the files that say which repository's configuration a
//! git directory uses (`commondir`; for a worktree, `gitdir` too). In a
//! worktree's common directory it is the other way round: everything is
//! read-only except what a commit needs — `objects`, `refs`, `logs/refs`,
//! `packed-refs`, and the worktree's own `worktrees/<name>`. The main
//! checkout's `HEAD` and index and the other worktrees' state can be read
//! (git does, to know which branches are checked out) but not changed.

use std::path::{Component, Path, PathBuf};

/// A git directory outside the project that the guest is given.
#[derive(Debug, Clone, PartialEq)]
pub struct Mount {
    /// The host directory, resolved.
    pub host: PathBuf,
    /// Where it appears in the guest: the path the project's `.git` file
    /// leads to, as spelled.
    pub guest: String,
    /// What the guest may write, relative to `host`; `None` is everything.
    pub writable: Option<Vec<String>>,
    /// What stays read-only whatever `writable` says, relative to `host`.
    pub protected: Vec<String>,
    /// What this is, for `atelier sandbox status`.
    pub what: &'static str,
}

/// What [`plan`] found for a project.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// Paths under the project's `.git` to keep read-only, relative to the
    /// project.
    pub protected: Vec<String>,
    /// The git directory to mount from outside the project, if there is one
    /// and it passed.
    pub mount: Option<Mount>,
    /// Why a `.git` file was not followed.
    pub refused: Option<String>,
}

/// What a worktree's common directory lets the guest write, besides the
/// worktree's own directory. `reftable` is the reference store of
/// repositories made with `--ref-format=reftable`.
const SHARED_WRITABLE: [&str; 6] = [
    "objects",
    "refs",
    "logs/refs",
    "packed-refs",
    "packed-refs.lock",
    "reftable",
];

/// What stays read-only in any git directory: its hooks and configuration,
/// and `commondir` — which every git directory may have, not only a linked
/// worktree's, and which names the directory whose `config` and `hooks`
/// git uses instead. A guest that could create `.git/commondir` would
/// choose the host's next `core.fsmonitor`.
const GIT_DIR_FILES: [&str; 4] = ["hooks", "config", "commondir", "config.worktree"];

/// How far and how wide [`inside`] looks for submodules' git directories.
const MAX_DEPTH: usize = 8;
const MAX_NESTED: usize = 512;

/// The git side of a guest for the project at `project`.
pub fn plan(project: &Path) -> Plan {
    let dot = project.join(".git");
    if dot.is_dir() {
        return Plan {
            protected: inside(&dot, ".git"),
            ..Plan::default()
        };
    }
    // A pointer file, or nothing yet: protected whole, since rewriting it
    // chooses the repository, hooks and all.
    let mut plan = Plan {
        protected: vec![".git".to_string()],
        ..Plan::default()
    };
    if std::fs::symlink_metadata(&dot).is_ok_and(|m| m.is_file()) {
        match follow(project, &dot) {
            Ok(Found::Outside(mount)) => plan.mount = Some(mount),
            Ok(Found::Inside(paths)) => plan.protected.extend(paths),
            Err(why) => plan.refused = Some(why),
        }
    }
    plan
}

/// The paths to keep read-only in the git directory `dir`, each prefixed
/// with `prefix` (the directory's path from the mount's root; empty for the
/// root itself): its [`GIT_DIR_FILES`], those of the submodules whose git
/// directories are under its `modules`, and for each linked worktree it
/// has, the files that tie the worktree to it.
///
/// Only what exists when the guest starts: a submodule added by a command
/// is covered from the next one on.
fn inside(dir: &Path, prefix: &str) -> Vec<String> {
    let at = |rest: &str| match prefix {
        "" => rest.to_string(),
        prefix => format!("{prefix}/{rest}"),
    };
    let mut out: Vec<String> = GIT_DIR_FILES.iter().map(|file| at(file)).collect();
    for name in subdirectories(&dir.join("worktrees")) {
        out.extend(worktree_files(&at(&format!("worktrees/{name}"))));
        // A worktree's submodules keep their git directories in it.
        let nested = dir.join("worktrees").join(&name).join("modules");
        modules(
            &nested,
            &at(&format!("worktrees/{name}/modules")),
            0,
            &mut out,
        );
    }
    modules(&dir.join("modules"), &at("modules"), 0, &mut out);
    out
}

/// What may not change in a linked worktree's own git directory at `at`:
/// [`GIT_DIR_FILES`] (`config` and `hooks`, which git does not read there,
/// are kept from appearing), and `gitdir`, by which the repository names
/// the worktree's checkout.
fn worktree_files(at: &str) -> Vec<String> {
    GIT_DIR_FILES
        .iter()
        .chain(&["gitdir"])
        .map(|file| format!("{at}/{file}"))
        .collect()
}

/// Add the [`GIT_DIR_FILES`] of every git directory under `dir` (a
/// `modules` directory: submodule names may hold slashes, and submodules
/// have submodules) to `out`.
fn modules(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    for name in subdirectories(dir) {
        if out.len() >= MAX_NESTED {
            return;
        }
        let (path, at) = (dir.join(&name), format!("{prefix}/{name}"));
        if path.join("HEAD").is_file() {
            out.extend(GIT_DIR_FILES.iter().map(|file| format!("{at}/{file}")));
            modules(
                &path.join("modules"),
                &format!("{at}/modules"),
                depth + 1,
                out,
            );
        } else {
            modules(&path, &at, depth + 1, out);
        }
    }
}

/// The names of the real directories (no links) in `dir`, sorted.
fn subdirectories(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// Where a `.git` file leads.
enum Found {
    /// A git directory in the project (`--separate-git-dir` to a place of
    /// the project's own): nothing to mount, these paths to keep read-only.
    Inside(Vec<String>),
    Outside(Mount),
}

/// Where the pointer file `dot` (the project's `.git`) leads, if that is a
/// git directory outside the project which names the project back.
fn follow(project: &Path, dot: &Path) -> Result<Found, String> {
    if !project.is_absolute() {
        return Err("the project path is not absolute".to_string());
    }
    let text = read_small(dot).ok_or("cannot read the .git file")?;
    let target = text
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or("the .git file is not a `gitdir:` pointer")?;
    // As git in the guest will read it: against the project, by spelling.
    // The guest has no links on the way (the directories above a mount are
    // its own, empty ones), so `..` there is what it is here.
    let spelled = lexical(&project.join(target));
    let real = canonical_dir(&spelled).ok_or_else(|| {
        format!(
            "the .git file points at {}, which is not a directory",
            spelled.display()
        )
    })?;
    let project_real = canonical_dir(project).ok_or("cannot resolve the project path")?;
    if let Ok(rel) = real.strip_prefix(&project_real) {
        return Ok(Found::Inside(inside(&real, &utf8(rel)?)));
    }
    let mount = if std::fs::symlink_metadata(real.join("commondir")).is_ok() {
        worktree(&spelled, &real, &project_real)?
    } else {
        separate(project, &spelled, &real, &project_real)?
    };
    // Neither around the project nor in it, on the host and in the guest.
    let guest = Path::new(&mount.guest);
    if apart(&mount.host, &project_real) && apart(guest, &lexical(project)) {
        Ok(Found::Outside(mount))
    } else {
        Err(format!(
            "the git directory {} and the project contain one another",
            mount.host.display()
        ))
    }
}

/// A linked worktree: `spelled` is `<common>/worktrees/<name>`. The mount
/// is the common directory.
fn worktree(spelled: &Path, real: &Path, project_real: &Path) -> Result<Mount, String> {
    let shape = || {
        format!(
            "{} is not a linked worktree's directory (<repository>/worktrees/<name>)",
            spelled.display()
        )
    };
    let name = spelled
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(shape)?;
    let guest_common = spelled
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "worktrees"))
        .and_then(Path::parent)
        .ok_or_else(shape)?;
    let common = canonical_dir(guest_common).ok_or_else(shape)?;
    if real != common.join("worktrees").join(name) {
        return Err(shape());
    }
    // `commondir` leads where the guest will find the mount: two levels up.
    let leads = read_small(&real.join("commondir"))
        .map(|text| lexical(&spelled.join(text.trim())))
        .is_some_and(|to| to == guest_common);
    if !leads {
        return Err(format!(
            "{}/commondir does not lead to the repository the worktree is in",
            real.display()
        ));
    }
    if !is_git_dir(&common) {
        return Err(format!("{} is not a git directory", common.display()));
    }
    // The repository names this project as that worktree.
    let back = read_small(&real.join("gitdir"))
        .map(|text| spelled.join(text.trim()))
        .filter(|p| p.file_name().is_some_and(|n| n == ".git"))
        .and_then(|p| canonical_dir(p.parent()?));
    if back.as_deref() != Some(project_real) {
        return Err(format!(
            "{} does not name this directory as its worktree (`git worktree repair` \
             in the main checkout, if it was moved)",
            real.display()
        ));
    }
    let own = format!("worktrees/{name}");
    let mut writable: Vec<String> = SHARED_WRITABLE.iter().map(|p| p.to_string()).collect();
    writable.push(own.clone());
    let mut protected = worktree_files(&own);
    modules(
        &real.join("modules"),
        &format!("{own}/modules"),
        0,
        &mut protected,
    );
    Ok(Mount {
        host: common,
        guest: utf8(guest_common)?,
        writable: Some(writable),
        protected,
        what: "the repository of this linked worktree",
    })
}

/// A git directory of its own somewhere else — a submodule's in the
/// superproject's `.git/modules`, or one made with `--separate-git-dir`.
/// The mount is that directory.
fn separate(
    project: &Path,
    spelled: &Path,
    real: &Path,
    project_real: &Path,
) -> Result<Mount, String> {
    if !is_git_dir(real) {
        return Err(format!("{} is not a git directory", real.display()));
    }
    let named = read_small(&real.join("config"))
        .and_then(|text| core_worktree(&text))
        .map(|value| lexical(&spelled.join(value)));
    // By spelling too: that is where git in the guest will look for the
    // checkout.
    let back = named
        .as_deref()
        .filter(|to| *to == lexical(project))
        .and_then(canonical_dir);
    if back.as_deref() != Some(project_real) {
        return Err(format!(
            "{} does not name this directory as its checkout (core.worktree)",
            real.display()
        ));
    }
    Ok(Mount {
        host: real.to_path_buf(),
        guest: utf8(spelled)?,
        writable: None,
        protected: inside(real, ""),
        what: "this checkout's git directory",
    })
}

/// What git itself asks of a git directory.
fn is_git_dir(dir: &Path) -> bool {
    dir.join("HEAD").is_file() && dir.join("objects").is_dir() && dir.join("refs").is_dir()
}

/// `core.worktree` in a git configuration file's text. Only the plain form
/// git writes; a value this misses is a directory not mounted.
fn core_worktree(config: &str) -> Option<String> {
    let mut in_core = false;
    let mut found = None;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            in_core = line.eq_ignore_ascii_case("[core]");
        } else if in_core
            && let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("worktree")
        {
            let value = value.trim();
            found = Some(
                match value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
                    Some(quoted) => quoted.replace("\\\"", "\"").replace("\\\\", "\\"),
                    None => value.to_string(),
                },
            );
        }
    }
    found.filter(|v| !v.is_empty())
}

/// A small regular file's text (never through a link, never a FIFO).
fn read_small(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > 1 << 20 {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// `path` with links resolved, if it is a directory.
fn canonical_dir(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok().filter(|p| p.is_dir())
}

/// `path` with `.` and `..` taken out by spelling alone.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            part => out.push(part),
        }
    }
    out
}

/// Neither path is the other or inside it.
fn apart(a: &Path, b: &Path) -> bool {
    !a.starts_with(b) && !b.starts_with(a)
}

fn utf8(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{} is not UTF-8", path.display()))
}

/// What `atelier sandbox status` says about the plan.
pub fn status_lines(plan: &Plan) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(mount) = &plan.mount {
        out.push(format!(
            "git directory: {} ({}) — {}",
            mount.host.display(),
            mount.what,
            match &mount.writable {
                Some(writable) => format!("read-only except {}", writable.join(", ")),
                None => format!("read-write except {}", GIT_DIR_FILES.join(", ")),
            }
        ));
    }
    if let Some(why) = &plan.refused {
        out.push(format!(
            "note: git will find no repository in the sandbox — {why}"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::time::Duration;

    use super::*;
    use crate::sandbox::{Outcome, run, testing};

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("atelier-gitdirs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    /// The host's git, deaf to the machine's configuration. `false` if it
    /// failed or is not installed.
    fn git(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "-c",
                "protocol.file.allow=always",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .is_ok_and(|out| out.status.success())
    }

    fn git_says(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository at `dir` with one commit. `false` without git.
    fn repository(dir: &Path) -> bool {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["init", "-q"])
            && git(dir, &["add", "a.txt"])
            && git(dir, &["commit", "-q", "-m", "first"])
    }

    /// A git directory's shape, by hand: what the checks look at.
    fn fake_git_dir(dir: &Path) {
        std::fs::create_dir_all(dir.join("objects")).unwrap();
        std::fs::create_dir_all(dir.join("refs")).unwrap();
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(dir.join("config"), "[core]\n").unwrap();
    }

    /// A linked worktree's layout, by hand: `<base>/main/.git` with
    /// `worktrees/wt`, and the project at `<base>/wt`.
    fn fake_worktree(base: &Path) -> (PathBuf, PathBuf) {
        let (common, project) = (base.join("main/.git"), base.join("wt"));
        fake_git_dir(&common);
        let own = common.join("worktrees/wt");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(own.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        std::fs::write(own.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            own.join("gitdir"),
            format!("{}\n", project.join(".git").display()),
        )
        .unwrap();
        std::fs::write(project.join(".git"), format!("gitdir: {}\n", own.display())).unwrap();
        (common, project)
    }

    #[test]
    fn a_plain_repository_needs_no_mount_and_guards_its_submodules() {
        let dir = temp("plain");
        let dot = dir.join(".git");
        fake_git_dir(&dot);
        fake_git_dir(&dot.join("modules/sub"));
        fake_git_dir(&dot.join("modules/group/lib"));
        fake_git_dir(&dot.join("modules/sub/modules/inner"));
        std::fs::create_dir_all(dot.join("worktrees/other")).unwrap();
        let plan = plan(&dir);
        assert_eq!(plan.mount, None);
        assert_eq!(plan.refused, None);
        assert_eq!(&plan.protected[..2], [".git/hooks", ".git/config"]);
        for path in [
            ".git/commondir",
            ".git/config.worktree",
            ".git/modules/sub/commondir",
            ".git/worktrees/other/commondir",
            ".git/worktrees/other/config.worktree",
            ".git/modules/sub/config",
            ".git/modules/sub/hooks",
            ".git/modules/group/lib/config",
            ".git/modules/sub/modules/inner/hooks",
        ] {
            assert!(plan.protected.iter().any(|p| p == path), "{path}");
        }
        // No repository at all: `.git` may not appear, and nothing is said.
        let empty = temp("none");
        let plan = super::plan(&empty);
        assert_eq!(plan.protected, [".git"]);
        assert_eq!((plan.mount, plan.refused), (None, None));
        for dir in [dir, empty] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_linked_worktree_gets_its_repository_and_little_of_it_writable() {
        let base = temp("wt");
        let (common, project) = fake_worktree(&base);
        let plan = plan(&project);
        assert_eq!(plan.refused, None);
        assert_eq!(plan.protected, [".git"]);
        let mount = plan.mount.expect("a mount");
        // The git directory, not the checkout it belongs to.
        assert_eq!(mount.host, common);
        assert_eq!(mount.guest, common.to_str().unwrap());
        let writable = mount.writable.expect("a list");
        assert!(writable.contains(&"objects".to_string()));
        assert!(writable.contains(&"worktrees/wt".to_string()));
        for kept in ["config", "hooks", "HEAD", "index", "worktrees", "modules"] {
            assert!(!writable.iter().any(|w| w == kept), "{kept}");
        }
        for path in ["worktrees/wt/commondir", "worktrees/wt/gitdir"] {
            assert!(mount.protected.iter().any(|p| p == path), "{path}");
        }

        // Spelled relative to the project, as newer gits can write it.
        std::fs::write(project.join(".git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();
        let again = super::plan(&project).mount.expect("a mount");
        assert_eq!((again.host, again.guest), (mount.host, mount.guest));
        let _ = std::fs::remove_dir_all(base);
    }

    /// What a repository that came from somewhere else can put in `.git`
    /// to be shown the host: none of it is followed.
    #[test]
    fn a_dot_git_file_is_not_believed_by_itself() {
        let base = temp("hostile");
        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let refused = |pointer: &str| {
            std::fs::write(project.join(".git"), pointer).unwrap();
            let plan = plan(&project);
            assert_eq!(plan.mount, None, "{pointer}");
            assert_eq!(plan.protected, [".git"]);
            plan.refused.unwrap_or_else(|| panic!("{pointer}: silent"))
        };

        // The classics.
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        for pointer in [
            "gitdir: /\n".to_string(),
            "gitdir: /etc\n".to_string(),
            format!("gitdir: {home}\n"),
            format!("gitdir: {home}/.ssh\n"),
            "gitdir: ..\n".to_string(),
            "gitdir: ../..\n".to_string(),
            "gitdir: /proc/self/root\n".to_string(),
            "gitdir:\n".to_string(),
            "not a pointer\n".to_string(),
        ] {
            refused(&pointer);
        }

        // A real repository, even a real worktree's directory — of some
        // other checkout: it does not name this project.
        let (common, _) = fake_worktree(&base);
        let why = refused(&format!("gitdir: {}\n", common.display()));
        assert!(why.contains("core.worktree"), "{why}");
        let why = refused(&format!("gitdir: {}/worktrees/wt\n", common.display()));
        assert!(why.contains("does not name this directory"), "{why}");

        // A directory that does name the project, but whose `commondir`
        // would make the mount something else: the parent's parent of a
        // directory named like a worktree's is not thereby a repository…
        let lure = base.join("secrets/worktrees/x");
        std::fs::create_dir_all(&lure).unwrap();
        std::fs::write(lure.join("commondir"), "../..\n").unwrap();
        let back = format!("{}\n", project.join(".git").display());
        std::fs::write(lure.join("gitdir"), &back).unwrap();
        let why = refused(&format!("gitdir: {}\n", lure.display()));
        assert!(why.contains("not a git directory"), "{why}");
        // …and a `commondir` that leads anywhere else is not followed.
        fake_git_dir(&base.join("secrets"));
        for elsewhere in ["/", "..", "../../..", &home] {
            std::fs::write(lure.join("commondir"), elsewhere).unwrap();
            let why = refused(&format!("gitdir: {}\n", lure.display()));
            assert!(why.contains("commondir"), "{elsewhere}: {why}");
        }

        // The project itself dressed as a worktree's directory, so that
        // "the repository" would be the directory two levels up.
        let nest = base.join("worktrees/p");
        fake_git_dir(&base);
        std::fs::create_dir_all(&nest).unwrap();
        std::fs::write(nest.join("commondir"), "../..\n").unwrap();
        std::fs::write(nest.join("gitdir"), format!("{}/.git\n", nest.display())).unwrap();
        std::fs::write(nest.join(".git"), "gitdir: .\n").unwrap();
        let plan = super::plan(&nest);
        assert_eq!(plan.mount, None, "{:?}", plan.refused);

        // A git directory that names the project and holds it.
        let around = base.join("around");
        let held = around.join("checkout");
        fake_git_dir(&around);
        std::fs::create_dir_all(&held).unwrap();
        std::fs::write(
            around.join("config"),
            format!("[core]\n\tworktree = {}\n", held.display()),
        )
        .unwrap();
        std::fs::write(held.join(".git"), "gitdir: ..\n").unwrap();
        let plan = super::plan(&held);
        assert_eq!(plan.mount, None);
        assert!(plan.refused.unwrap().contains("contain one another"));

        // Through a link: the link is the project's, the target is not.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&common, project.join("link")).unwrap();
            refused("gitdir: link\n");
            refused("gitdir: link/worktrees/wt\n");
        }
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn core_worktree_is_read_as_git_writes_it() {
        let read = |text: &str| core_worktree(text);
        assert_eq!(
            read("[core]\n\tbare = false\n\tworktree = ../../../sub\n"),
            Some("../../../sub".to_string())
        );
        assert_eq!(
            read("[Core]\n  WorkTree = \"/a b/c\"\n[remote \"origin\"]\n\turl = x\n"),
            Some("/a b/c".to_string())
        );
        // Another section's key is not it.
        assert_eq!(read("[other]\n\tworktree = /\n"), None);
        assert_eq!(read("[core]\n\tbare = false\n"), None);
        assert_eq!(
            lexical(Path::new("/a/b/../c/./d/..")),
            PathBuf::from("/a/c")
        );
        assert_eq!(lexical(Path::new("/../..")), PathBuf::from("/"));
    }

    /// What a guest run said and how it ended.
    fn guest(command: &str, project: &Path, setup: &crate::sandbox::Setup) -> (Outcome, String) {
        let out = run(command, project, setup, Duration::from_secs(120)).unwrap();
        let mut said = String::from_utf8_lossy(&out.stdout).into_owned();
        said.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.outcome, said)
    }

    /// `false` (after saying so) if the image has no git to try.
    fn guest_has_git(project: &Path, setup: &crate::sandbox::Setup) -> bool {
        let found = guest("command -v git", project, setup).0 == Outcome::Exited(0);
        if !found {
            eprintln!("skipped: no git in the sandbox's root filesystem");
        }
        found
    }

    const COMMIT: &str = "git -c user.name=G -c user.email=g@example.com commit -q";
    const STASH: &str = "git -c user.name=G -c user.email=g@example.com stash";

    /// A linked worktree, for real: status, diff, log, add and commit in
    /// the guest, and what the guest may not touch in the repository.
    #[test]
    fn git_works_in_a_linked_worktree_in_the_guest() {
        let Some(setup) = testing::live_setup() else {
            return;
        };
        let base = temp("live-wt");
        let (main, project) = (base.join("main"), base.join("wt"));
        if !repository(&main)
            || !git(&main, &["worktree", "add", "-q", "-b", "side", "../wt"])
            || !git(&main, &["worktree", "add", "-q", "-b", "third", "../other"])
        {
            eprintln!("skipped: no git on the host");
            return;
        }
        if !guest_has_git(&project, &setup) {
            return;
        }
        std::fs::write(main.join("secret.txt"), "the main checkout's\n").unwrap();
        std::fs::write(main.join(".git/hooks/pre-commit"), "#!/bin/sh\nexit 1\n").unwrap();
        let hooks_before = std::fs::read_dir(main.join(".git/hooks")).unwrap().count();
        let config_before = std::fs::read_to_string(main.join(".git/config")).unwrap();

        let (outcome, said) = guest(
            &format!(
                "echo two >> a.txt && echo new > b.txt && git status --short && \
                 git diff --stat && git log --oneline && git add a.txt b.txt && \
                 {COMMIT} -m second --no-verify && git log --oneline && git status --short && \
                 git branch -q extra && git rev-parse --abbrev-ref HEAD && \
                 echo three >> a.txt && {STASH} -q && git stash pop -q && \
                 git reset -q --hard HEAD"
            ),
            &project,
            &setup,
        );
        assert_eq!(outcome, Outcome::Exited(0), "{said}");
        assert!(
            said.contains(" M a.txt") && said.contains("?? b.txt"),
            "{said}"
        );
        assert!(said.contains("first") && said.contains("second"), "{said}");
        assert!(said.contains("\nside\n"), "{said}");
        // The host's git agrees, from the worktree and from the main
        // checkout, which is where it was.
        assert_eq!(
            git_says(&project, &["log", "--format=%s", "side"]),
            "second\nfirst"
        );
        assert_eq!(git_says(&project, &["status", "--short"]), "");
        assert_eq!(git_says(&main, &["log", "--format=%s", "main"]), "first");
        assert_eq!(
            git_says(&main, &["log", "--format=%s", "extra"]),
            "second\nfirst"
        );
        assert!(git(&main, &["fsck", "--strict"]));

        // Of the main checkout the guest sees the git directory, not the
        // files; the directory above holds nothing else.
        let (_, said) = guest(
            &format!(
                "ls -A {main}; cat {main}/secret.txt; ls -A {base}",
                main = main.display(),
                base = base.display()
            ),
            &project,
            &setup,
        );
        assert!(said.starts_with(".git\n"), "{said}");
        assert!(said.contains("No such file"), "{said}");
        assert!(!said.contains("the main checkout's"), "{said}");
        assert!(!said.contains("other"), "{said}");

        // What decides which programs the host runs, and what belongs to
        // the other checkouts: by a few routes each.
        let common = main.join(".git");
        let own = common.join("worktrees/wt");
        let attempts = [
            "echo x > $C/hooks/post-commit".to_string(),
            "echo x >> $C/hooks/pre-commit".to_string(),
            "rm $C/hooks/pre-commit".to_string(),
            "echo x >> $C/config".to_string(),
            "git config core.fsmonitor /tmp/x".to_string(),
            "git config --worktree core.fsmonitor /tmp/x".to_string(),
            "echo '[core]' > $C/worktrees/wt/config.worktree".to_string(),
            "echo x > $C/HEAD".to_string(),
            "echo x > $C/index".to_string(),
            "echo x > $C/worktrees/other/HEAD".to_string(),
            "rm -r $C/worktrees/other".to_string(),
            "mkdir $C/worktrees/more".to_string(),
            "git worktree add -q /tmp/more".to_string(),
            format!("echo {} > $C/worktrees/wt/commondir", project.display()),
            "echo /elsewhere/.git > $C/worktrees/wt/gitdir".to_string(),
            "rm $C/worktrees/wt/commondir".to_string(),
            "mv $C/worktrees/wt $C/worktrees/gone".to_string(),
            "mv $C/worktrees $C/objects/w".to_string(),
            "mv $C/hooks $C/objects/hooks".to_string(),
            "mv $C/objects/pack $C/hooks2".to_string(),
            "ln -s $C/config /tmp/c && echo x >> /tmp/c".to_string(),
            "ln $C/config $C/objects/config && echo x >> $C/objects/config".to_string(),
            "cd $C && echo x > hooks/post-commit".to_string(),
            "echo x > $C/objects/../hooks/post-commit".to_string(),
            "mkdir $C/modules".to_string(),
            "echo 'gitdir: /tmp' > .git".to_string(),
            "rm .git".to_string(),
        ];
        let own_before = (
            std::fs::read_to_string(own.join("commondir")).unwrap(),
            std::fs::read_to_string(own.join("gitdir")).unwrap(),
        );
        let other_head = std::fs::read_to_string(common.join("worktrees/other/HEAD")).unwrap();
        let main_head = std::fs::read_to_string(common.join("HEAD")).unwrap();
        for attempt in attempts {
            let command = format!("C={}; {attempt}", common.display());
            let (outcome, said) = guest(&command, &project, &setup);
            assert_ne!(outcome, Outcome::Exited(0), "`{attempt}` succeeded: {said}");
            let after = |what: &str| format!("after `{attempt}`: {what}");
            assert_eq!(
                std::fs::read_to_string(common.join("config")).unwrap(),
                config_before,
                "{}",
                after("config")
            );
            assert_eq!(
                std::fs::read_dir(common.join("hooks")).unwrap().count(),
                hooks_before,
                "{}",
                after("hooks")
            );
            assert_eq!(
                std::fs::read_to_string(common.join("hooks/pre-commit")).unwrap(),
                "#!/bin/sh\nexit 1\n",
                "{}",
                after("pre-commit")
            );
            assert_eq!(
                (
                    std::fs::read_to_string(own.join("commondir")).unwrap(),
                    std::fs::read_to_string(own.join("gitdir")).unwrap(),
                ),
                own_before,
                "{}",
                after("commondir, gitdir")
            );
            assert!(
                !own.join("config.worktree").exists(),
                "{}",
                after("config.worktree")
            );
            assert_eq!(
                std::fs::read_to_string(common.join("worktrees/other/HEAD")).unwrap(),
                other_head,
                "{}",
                after("the other worktree")
            );
            assert_eq!(
                std::fs::read_to_string(common.join("HEAD")).unwrap(),
                main_head,
                "{}",
                after("the main HEAD")
            );
            assert!(!common.join("modules").exists(), "{}", after("modules"));
            assert!(
                !common.join("worktrees/more").exists(),
                "{}",
                after("worktrees")
            );
            assert!(
                std::fs::read_to_string(project.join(".git"))
                    .unwrap()
                    .starts_with("gitdir: "),
                "{}",
                after(".git")
            );
        }
        // Still a worktree the host's git knows.
        assert_eq!(git_says(&project, &["status", "--short"]), "");
        assert_eq!(
            git_says(&main, &["worktree", "list", "--porcelain"])
                .matches("worktree ")
                .count(),
            3
        );
        let _ = std::fs::remove_dir_all(base);
    }

    /// A submodule's checkout as the project (its git directory is in the
    /// superproject's `.git/modules`), and a project that holds one.
    #[test]
    fn git_works_in_a_submodule_and_around_one_in_the_guest() {
        let Some(setup) = testing::live_setup() else {
            return;
        };
        let base = temp("live-sub");
        let (lib, sup) = (base.join("lib"), base.join("super"));
        if !repository(&lib)
            || !repository(&sup)
            || !git(
                &sup,
                &["submodule", "-q", "add", lib.to_str().unwrap(), "dep"],
            )
            || !git(&sup, &["commit", "-q", "-m", "with dep"])
        {
            eprintln!("skipped: no git on the host");
            return;
        }
        let project = sup.join("dep");
        if !guest_has_git(&project, &setup) {
            return;
        }
        std::fs::write(sup.join("secret.txt"), "the superproject's\n").unwrap();
        let module = sup.join(".git/modules/dep");
        let config_before = std::fs::read_to_string(module.join("config")).unwrap();

        // In the submodule.
        let (outcome, said) = guest(
            &format!(
                "echo two >> a.txt && git status --short && git diff --stat && \
                 git add a.txt && {COMMIT} -m in-dep && git log --oneline && git status --short"
            ),
            &project,
            &setup,
        );
        assert_eq!(outcome, Outcome::Exited(0), "{said}");
        assert!(
            said.contains(" M a.txt") && said.contains("in-dep"),
            "{said}"
        );
        assert_eq!(git_says(&project, &["log", "--format=%s"]), "in-dep\nfirst");
        assert!(git(&project, &["fsck", "--strict"]));
        // The superproject is not there: neither its files nor its `.git`
        // beyond this module.
        let (_, said) = guest(
            &format!(
                "ls -A {sup}; ls -A {sup}/.git; ls -A {sup}/.git/modules",
                sup = sup.display()
            ),
            &project,
            &setup,
        );
        assert_eq!(said, ".git\ndep\nmodules\ndep\n", "{said}");
        for attempt in [
            "echo x > $M/hooks/post-commit",
            "echo x >> $M/config",
            "echo /tmp > $M/commondir",
            "echo '[core]' > $M/config.worktree",
            "git config core.fsmonitor /tmp/x",
            "mv $M/hooks $M/h",
            "ln -s $M/config /tmp/c && echo x >> /tmp/c",
            "echo 'gitdir: /tmp' > .git",
        ] {
            let command = format!("M={}; {attempt}", module.display());
            let (outcome, said) = guest(&command, &project, &setup);
            assert_ne!(outcome, Outcome::Exited(0), "`{attempt}` succeeded: {said}");
            assert_eq!(
                std::fs::read_to_string(module.join("config")).unwrap(),
                config_before,
                "after `{attempt}`"
            );
            assert!(
                !module.join("hooks/post-commit").exists(),
                "after `{attempt}`"
            );
            assert!(!module.join("commondir").exists(), "after `{attempt}`");
            assert!(
                !module.join("config.worktree").exists(),
                "after `{attempt}`"
            );
        }

        // Around it: the superproject as the project sees the submodule's
        // new commit, records it, and cannot reconfigure the submodule.
        let (outcome, said) = guest(
            &format!(
                "git status --short && git submodule status && git add dep && \
                 {COMMIT} -m bump && git log --oneline && git -C dep log --oneline && \
                 echo three >> dep/a.txt && git -C dep add a.txt && \
                 {inner} -m from-above && git status --short",
                inner = COMMIT.replacen("git ", "git -C dep ", 1)
            ),
            &sup,
            &setup,
        );
        assert_eq!(outcome, Outcome::Exited(0), "{said}");
        assert!(said.contains(" M dep") && said.contains("bump"), "{said}");
        assert_eq!(
            git_says(&project, &["log", "--format=%s"]),
            "from-above\nin-dep\nfirst"
        );
        for attempt in [
            "echo x >> .git/modules/dep/config",
            "echo ../../../evil > .git/modules/dep/commondir",
            "echo ../evil > .git/commondir",
            "echo x > .git/modules/dep/hooks/post-checkout",
            "git -C dep config core.fsmonitor /tmp/x",
            "mv .git/modules m",
        ] {
            let (outcome, said) = guest(attempt, &sup, &setup);
            assert_ne!(outcome, Outcome::Exited(0), "`{attempt}` succeeded: {said}");
            assert_eq!(
                std::fs::read_to_string(module.join("config")).unwrap(),
                config_before,
                "after `{attempt}`"
            );
            assert!(
                !module.join("hooks/post-checkout").exists(),
                "after `{attempt}`"
            );
            assert!(!module.join("commondir").exists(), "after `{attempt}`");
            assert!(!sup.join(".git/commondir").exists(), "after `{attempt}`");
        }
        let _ = std::fs::remove_dir_all(base);
    }

    /// The hostile `.git` file, in a guest: the directory it names is not
    /// there, and git finds no repository.
    #[test]
    fn a_hostile_dot_git_file_gets_nothing_mounted_in_the_guest() {
        let Some(setup) = testing::live_setup() else {
            return;
        };
        let base = temp("live-hostile");
        let (victim, project) = (base.join("victim"), base.join("project"));
        std::fs::create_dir_all(&project).unwrap();
        // A real repository next door, with something to steal.
        fake_git_dir(&victim.join(".git"));
        std::fs::write(victim.join(".git/config"), "[remote]\n\turl = token\n").unwrap();
        std::fs::write(victim.join("id_rsa"), "PRIVATE\n").unwrap();
        for pointer in [
            format!("gitdir: {}\n", victim.display()),
            format!("gitdir: {}/.git\n", victim.display()),
            "gitdir: ../victim/.git\n".to_string(),
            "gitdir: /\n".to_string(),
            format!("gitdir: {}\n", base.display()),
        ] {
            std::fs::write(project.join(".git"), &pointer).unwrap();
            let (_, said) = guest(
                &format!(
                    "cat {v}/id_rsa {v}/.git/config; ls -A {v}; echo \"[$(ls -A {b})]\"; \
                     git status 2>&1; echo status=$?",
                    v = victim.display(),
                    b = base.display()
                ),
                &project,
                &setup,
            );
            assert!(
                !said.contains("PRIVATE") && !said.contains("token"),
                "{pointer}: {said}"
            );
            // Next to the project there is nothing.
            assert!(said.contains("[project]"), "{pointer}: {said}");
            assert!(said.contains("not a git repository"), "{pointer}: {said}");
            assert!(!said.contains("status=0"), "{pointer}: {said}");
        }
        let _ = std::fs::remove_dir_all(base);
    }
}
