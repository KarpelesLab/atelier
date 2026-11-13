//! Skills: directories of instructions (and scripts, references) the agent
//! loads when a task calls for them.
//!
//! A skill is a directory holding a `SKILL.md` whose front matter gives a
//! `name` and a `description`. Only those two are advertised to the model
//! every turn (see `context::skills`); the body and the other files cost
//! nothing until the model asks for them with the `skill` tool. That is the
//! whole point of the format: many skills installed, few tokens spent.
//!
//! # Where they live
//!
//! In order of precedence (the first skill found with a name wins):
//!
//! 1. `<project>/.atelier/skills/`, `<project>/.claude/skills/`,
//!    `<project>/.agents/skills/` — the latter two are where other harnesses
//!    keep theirs, read as they are
//! 2. the user-global ones ([`global_dirs`]): `~/.config/atelier/skills`,
//!    `~/.claude/skills`, `~/.agents/skills`
//!
//! Nothing is cached; the directories are re-read whenever skills are listed
//! or loaded, so an edited or newly installed skill is seen at once.
//!
//! # What a repository may point at
//!
//! A skill directory may be a symlink — that is how people install them
//! globally — but a *project* skill must resolve inside the project, so a
//! repository cannot present some directory of the user's as a skill. And a
//! skill's files are only ever read from inside the skill's (resolved)
//! directory: see [`Skill::read`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::commands::{absolute_env, config_home, front_matter};

/// The file that makes a directory a skill.
const SKILL_FILE: &str = "SKILL.md";

/// Longest skill name accepted (the Agent Skills convention).
const MAX_NAME_CHARS: usize = 64;

/// Most of a skill's other files listed when it is loaded.
const MAX_LISTED_FILES: usize = 100;

/// How deep below a skill directory its files are listed.
const MAX_DEPTH: usize = 4;

/// Largest file [`Skill::read`] returns; skills are instructions, and a
/// bigger file is better searched or run than loaded whole.
const MAX_FILE_BYTES: u64 = 256 * 1024;

/// One discovered skill.
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The skill's directory, as found (not resolved through symlinks).
    pub dir: PathBuf,
    /// Found under a user-global directory rather than in the project.
    pub global: bool,
}

/// What a scan of the skill directories found.
#[derive(Debug, Default)]
pub struct Found {
    /// Usable skills, by precedence then name.
    pub skills: Vec<Skill>,
    /// One line per `SKILL.md` that was skipped, saying why.
    pub invalid: Vec<String>,
}

/// The user-global skill directories from the environment, in precedence
/// order. Empty when no home directory is known.
pub fn global_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    dirs.extend(config_home().map(|c| c.join("atelier").join("skills")));
    if let Some(home) = absolute_env("HOME") {
        dirs.push(home.join(".claude").join("skills"));
        dirs.push(home.join(".agents").join("skills"));
    }
    dirs
}

/// Scan for skills. `global` is the list of user-global directories —
/// [`global_dirs`] in production, temp directories (or none) in tests, which
/// is why it is passed in rather than read here.
pub fn discover(root: &Path, global: &[PathBuf]) -> Found {
    let project = [".atelier", ".claude", ".agents"].map(|d| root.join(d).join("skills"));
    let mut found = Found::default();
    for (dir, is_global) in project
        .iter()
        .map(|d| (d, false))
        .chain(global.iter().map(|d| (d, true)))
    {
        scan(dir, is_global, root, &mut found);
    }
    found
}

/// Add the skills directly under `dir` to `found`.
fn scan(dir: &Path, global: bool, root: &Path, found: &mut Found) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    // Directory order is arbitrary; make "first found" mean something.
    dirs.sort();
    for skill_dir in dirs {
        let file = skill_dir.join(SKILL_FILE);
        if !file.is_file() {
            continue;
        }
        match load(&skill_dir, global, root) {
            // An earlier directory already defined it.
            Ok(skill) if found.skills.iter().any(|s| s.name == skill.name) => {}
            Ok(skill) => found.skills.push(skill),
            Err(e) => found.invalid.push(format!("{}: {e:#}", file.display())),
        }
    }
}

/// Read the skill in `dir`, checking what [`Skill`] promises.
fn load(dir: &Path, global: bool, root: &Path) -> Result<Skill> {
    if !global {
        let (real, root) = (dir.canonicalize()?, root.canonicalize()?);
        if !real.starts_with(&root) {
            bail!("a project skill must not be a link out of the project");
        }
    }
    let text = read_confined(dir, SKILL_FILE)?;
    let (fields, _) = front_matter(&text);
    let field = |key: &str| {
        fields
            .iter()
            .find(|(k, v)| *k == key && !v.is_empty())
            .map(|(_, v)| v.clone())
            .with_context(|| format!("no `{key}` in the front matter"))
    };
    let name = field("name")?;
    if name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_whitespace) {
        bail!("`name` must be one word of at most {MAX_NAME_CHARS} characters");
    }
    Ok(Skill {
        name,
        description: field("description")?,
        dir: dir.to_path_buf(),
        global,
    })
}

/// Read the file `relative` inside `dir`, refusing anything that resolves
/// (through `..` or a symlink) outside the directory `dir` itself resolves
/// to.
fn read_confined(dir: &Path, relative: &str) -> Result<String> {
    let base = dir
        .canonicalize()
        .with_context(|| format!("resolving {}", dir.display()))?;
    let path = base
        .join(relative)
        .canonicalize()
        .with_context(|| format!("no file {relative:?} in this skill"))?;
    if !path.starts_with(&base) {
        bail!("{relative:?} is outside the skill's directory");
    }
    let meta = std::fs::metadata(&path)?;
    if !meta.is_file() {
        bail!("{relative:?} is not a file");
    }
    if meta.len() > MAX_FILE_BYTES {
        bail!(
            "{relative:?} is {} bytes, over the {MAX_FILE_BYTES}-byte limit for a skill file",
            meta.len()
        );
    }
    std::fs::read_to_string(&path).with_context(|| format!("reading {relative:?}"))
}

impl Skill {
    /// The instructions: `SKILL.md` without its front matter, read now.
    pub fn body(&self) -> Result<String> {
        let text = read_confined(&self.dir, SKILL_FILE)?;
        Ok(front_matter(&text).1.trim().to_string())
    }

    /// One of the skill's own files, by its path relative to the skill's
    /// directory. Confined to that directory: a global skill lives outside
    /// the project, where the `read` tool would have to ask first, and this
    /// must not become a way to read anything else out there.
    pub fn read(&self, relative: &str) -> Result<String> {
        read_confined(&self.dir, relative)
    }

    /// The skill's files other than `SKILL.md`, as sorted relative paths,
    /// and whether the listing was cut short. Hidden files are left out and
    /// symlinked directories are not entered.
    pub fn files(&self) -> (Vec<String>, bool) {
        let mut out = Vec::new();
        list_files(&self.dir, "", 0, &mut out);
        out.retain(|f| f != SKILL_FILE);
        out.sort();
        let truncated = out.len() > MAX_LISTED_FILES;
        out.truncate(MAX_LISTED_FILES);
        (out, truncated)
    }
}

fn list_files(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        // Stop collecting well past what is shown; enough to know it was cut.
        if out.len() > MAX_LISTED_FILES + 1 {
            return;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|n| !n.starts_with('.')) else {
            continue;
        };
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => {
                if depth < MAX_DEPTH {
                    list_files(&entry.path(), &format!("{prefix}{name}/"), depth + 1, out);
                }
            }
            Ok(_) => out.push(format!("{prefix}{name}")),
            Err(_) => {}
        }
    }
}

/// The lines `/skills` prints: each skill with where it comes from, then the
/// `SKILL.md` files that were skipped and why.
pub fn command(root: &Path, global: &[PathBuf]) -> Vec<String> {
    let found = discover(root, global);
    let mut out = Vec::new();
    if found.skills.is_empty() {
        out.push("no skills found".to_string());
        out.push(
            "  a skill is a directory with a SKILL.md, under .atelier/skills/ \
             (also .claude/skills/, .agents/skills/, and the same in your home)"
                .to_string(),
        );
    } else {
        out.push(format!("{} skill(s):", found.skills.len()));
    }
    for s in &found.skills {
        out.push(format!("  {} — {}", s.name, s.description));
        // Project skills by their path in the project, global ones in full.
        let dir = s.dir.strip_prefix(root).unwrap_or(&s.dir);
        out.push(format!("      {}", dir.display()));
    }
    if !found.invalid.is_empty() {
        out.push("skipped:".to_string());
        out.extend(found.invalid.iter().map(|l| format!("  {l}")));
    }
    out
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};

    /// A fresh project root and one global skills directory under the system
    /// temp dir — never the real home.
    pub fn dirs(name: &str) -> (PathBuf, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("atelier-skills-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (root, global) = (base.join("project"), base.join("global"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        // Resolved, so paths compare equal where the temp dir is a symlink.
        (root.canonicalize().unwrap(), global.canonicalize().unwrap())
    }

    /// Write a well-formed skill into `skills_dir/<name>/`.
    pub fn skill(skills_dir: &Path, name: &str, description: &str, body: &str) -> PathBuf {
        let dir = skills_dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
        )
        .unwrap();
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{dirs, skill};
    use super::*;

    /// The skill called `name`, if there is one.
    fn find(root: &Path, global: &[PathBuf], name: &str) -> Option<Skill> {
        discover(root, global)
            .skills
            .into_iter()
            .find(|s| s.name == name)
    }

    #[test]
    fn finds_skills_in_every_directory_project_first() {
        let (root, global) = dirs("discover");
        skill(
            &root.join(".atelier/skills"),
            "release",
            "cut a release",
            "A",
        );
        skill(&root.join(".claude/skills"), "release", "shadowed", "B");
        skill(&root.join(".claude/skills"), "pdf", "read PDFs", "C");
        skill(&root.join(".agents/skills"), "charts", "draw charts", "D");
        skill(&global, "pdf", "shadowed too", "E");
        skill(&global, "notes", "take notes", "F");
        // Not skills: a directory without SKILL.md, a stray file.
        std::fs::create_dir_all(root.join(".atelier/skills/empty")).unwrap();
        std::fs::write(root.join(".atelier/skills/README.md"), "x").unwrap();

        let found = discover(&root, std::slice::from_ref(&global));
        let seen: Vec<(&str, &str, bool)> = found
            .skills
            .iter()
            .map(|s| (s.name.as_str(), s.description.as_str(), s.global))
            .collect();
        assert_eq!(
            seen,
            [
                ("release", "cut a release", false),
                ("pdf", "read PDFs", false),
                ("charts", "draw charts", false),
                ("notes", "take notes", true),
            ]
        );
        assert!(found.invalid.is_empty(), "{:?}", found.invalid);

        assert_eq!(find(&root, &[global], "pdf").unwrap().body().unwrap(), "C");
        assert!(find(&root, &[], "notes").is_none());
    }

    #[test]
    fn invalid_skills_are_skipped_and_noted() {
        let (root, _) = dirs("invalid");
        let skills = root.join(".atelier/skills");
        for (dir, text) in [
            ("no-front-matter", "Just instructions."),
            ("no-description", "---\nname: x\n---\nBody"),
            ("no-name", "---\ndescription: something\n---\nBody"),
            (
                "bad-name",
                "---\nname: two words\ndescription: d\n---\nBody",
            ),
        ] {
            std::fs::create_dir_all(skills.join(dir)).unwrap();
            std::fs::write(skills.join(dir).join("SKILL.md"), text).unwrap();
        }
        skill(&skills, "good", "works", "ok");

        let found = discover(&root, &[]);
        assert_eq!(found.skills.len(), 1);
        assert_eq!(found.skills[0].name, "good");
        assert_eq!(found.invalid.len(), 4, "{:?}", found.invalid);
        let all = found.invalid.join("\n");
        assert!(all.contains("no-front-matter/SKILL.md: no `name`"), "{all}");
        assert!(
            all.contains("no-description/SKILL.md: no `description`"),
            "{all}"
        );
        assert!(all.contains("bad-name/SKILL.md: `name` must be"), "{all}");

        let lines = command(&root, &[]).join("\n");
        assert!(
            lines.contains("1 skill(s):\n  good — works\n      .atelier/skills/good"),
            "{lines}"
        );
        assert!(lines.contains("skipped:\n  "), "{lines}");
    }

    #[test]
    fn lists_and_reads_its_own_files_only() {
        let (root, global) = dirs("files");
        let dir = skill(&global, "pdf", "read PDFs", "See reference.md.");
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts/extract.py"), "print('hi')").unwrap();
        std::fs::write(dir.join("reference.md"), "The reference.").unwrap();
        std::fs::write(dir.join(".hidden"), "x").unwrap();
        std::fs::write(global.join("outside.txt"), "not part of the skill").unwrap();

        let s = find(&root, &[global], "pdf").unwrap();
        assert_eq!(
            s.files(),
            (
                vec!["reference.md".to_string(), "scripts/extract.py".to_string()],
                false
            )
        );
        assert_eq!(s.read("reference.md").unwrap(), "The reference.");
        assert_eq!(s.read("scripts/extract.py").unwrap(), "print('hi')");

        let err = s.read("../outside.txt").unwrap_err().to_string();
        assert!(err.contains("outside the skill's directory"), "{err}");
        assert!(s.read("/etc/hostname").is_err());
        assert!(s.read("missing.md").is_err());
        assert!(
            s.read("scripts")
                .unwrap_err()
                .to_string()
                .contains("not a file")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_lead_out_of_a_skill() {
        use std::os::unix::fs::symlink;
        let (root, global) = dirs("symlinks");
        let secret = global.join("secret.txt");
        std::fs::write(&secret, "private").unwrap();

        // A file link out of the skill is listed but not readable.
        let dir = skill(&root.join(".atelier/skills"), "leaky", "d", "body");
        symlink(&secret, dir.join("link.txt")).unwrap();
        let s = find(&root, &[], "leaky").unwrap();
        let err = s.read("link.txt").unwrap_err().to_string();
        assert!(err.contains("outside the skill's directory"), "{err}");

        // A project skill that is itself a link out of the project is
        // refused; the same link among the global skills is how one installs
        // a skill, and works.
        let elsewhere = skill(&global.join("store"), "linked", "d", "linked body");
        symlink(&elsewhere, root.join(".atelier/skills/linked")).unwrap();
        let found = discover(&root, &[]);
        assert!(found.skills.iter().all(|s| s.name != "linked"));
        assert!(
            found
                .invalid
                .iter()
                .any(|l| l.contains("out of the project")),
            "{:?}",
            found.invalid
        );

        let installed = global.join("installed");
        std::fs::create_dir_all(&installed).unwrap();
        symlink(&elsewhere, installed.join("linked")).unwrap();
        let s = find(&root, &[installed], "linked").unwrap();
        assert_eq!(s.body().unwrap(), "linked body");
    }

    #[test]
    fn says_so_when_there_are_none() {
        let (root, global) = dirs("none");
        assert_eq!(command(&root, &[global])[0], "no skills found");
    }
}
