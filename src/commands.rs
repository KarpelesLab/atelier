//! File-defined slash commands: a Markdown file is a prompt template, and its
//! path is the command's name.
//!
//! # Where they live
//!
//! In order of precedence (the first file found for a name wins):
//!
//! 1. `<project>/.atelier/commands/`
//! 2. `<project>/.claude/commands/` — read-compatible with Claude Code
//! 3. the user-global directory ([`global_dir`])
//!
//! `foo.md` is `/foo`; `git/sync.md` is `/git:sync`. Built-in commands are
//! matched before these are consulted (see `agent::dispatch`), so a file
//! cannot shadow `/quit`. Nothing is cached: the directories are re-read on
//! every use, so an edited template applies to the next invocation.
//!
//! # Format
//!
//! Optional front matter between `---` lines (`key: value`, parsed by hand —
//! see [`front_matter`]) with `description`, `argument-hint` and `model` (the
//! model for the turn the command starts — see `agent::turn_model`); other
//! keys are ignored. The rest is the template.
//!
//! # Expansion, and why it is ordered the way it is
//!
//! The template is cut into text and `` !`command` `` segments **before**
//! anything is substituted, and each segment is expanded exactly once. So:
//!
//! - Only the template author decides what runs. Arguments typed by the user
//!   and the output of a command are inserted as data and never re-scanned,
//!   so neither can introduce a `` !`…` `` or a `$1` of its own.
//! - In text, `$ARGUMENTS` is everything typed after the command name and
//!   `$1`…`$9` are its shell words (missing ones are empty).
//! - A `` !`command` `` is passed to `sh -c` *unmodified*, with the words as
//!   the shell's own positional parameters and `$ARGUMENTS` in its
//!   environment. A `$1` the template placed there is therefore expanded by
//!   the shell as a value — `/log "; rm -rf ~"` hands `git log $1` one odd
//!   argument, not a second command. (Unquoted, the shell still word-splits
//!   and globs it; templates should write `"$1"`.)
//!
//! A command file comes from the repository, so its `!` lines are code
//! execution on the say-so of a file. Each one goes through the caller's
//! `approve` callback first — the same approval as the `bash` tool — and a
//! refusal abandons the whole expansion rather than send a prompt with holes.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// How long one `` !`command` `` may run before it is killed.
const SHELL_TIMEOUT: Duration = Duration::from_secs(10);

/// Most bytes of a command's output inserted into the prompt.
const MAX_SHELL_OUTPUT: usize = 16 * 1024;

/// How deep below a commands directory files are looked for (`a/b/c.md` is
/// `/a:b:c`). Bounds the walk; symlinked directories are not followed.
const MAX_DEPTH: usize = 4;

/// Longest description shown in `/help` when it is taken from the template's
/// first line rather than written in the front matter.
const MAX_DESCRIPTION_CHARS: usize = 80;

/// One command file.
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    /// Without the leading slash: `foo`, `git:sync`.
    pub name: String,
    /// From the front matter, else the template's first line.
    pub description: String,
    /// Shown after the name in `/help`, e.g. `<branch> [remote]`.
    pub argument_hint: Option<String>,
    /// The model to run the command's turn on, as `/model` takes it
    /// (`provider/model#variant`); the session's when unset.
    pub model: Option<String>,
    /// The prompt template.
    pub body: String,
    /// The file it was read from.
    pub path: PathBuf,
}

/// The user-global commands directory from the environment
/// (`$XDG_CONFIG_HOME/atelier/commands`, else `~/.config/atelier/commands`),
/// or `None` when neither variable gives an absolute path.
pub fn global_dir() -> Option<PathBuf> {
    Some(config_home()?.join("atelier").join("commands"))
}

/// `$XDG_CONFIG_HOME`, else `~/.config`.
pub fn config_home() -> Option<PathBuf> {
    absolute_env("XDG_CONFIG_HOME").or_else(|| Some(absolute_env("HOME")?.join(".config")))
}

/// An environment variable holding an absolute path.
pub fn absolute_env(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// Every command defined for the project at `root`, sorted by name. `global`
/// is the user-global directory — [`global_dir`] in production, a temp
/// directory (or `None`) in tests, which is why it is passed in.
pub fn list(root: &Path, global: Option<&Path>) -> Vec<Command> {
    let mut dirs = vec![
        root.join(".atelier").join("commands"),
        root.join(".claude").join("commands"),
    ];
    dirs.extend(global.map(Path::to_path_buf));

    let mut commands: Vec<Command> = Vec::new();
    for dir in dirs {
        let mut found = Vec::new();
        walk(&dir, "", 0, &mut found);
        for command in found {
            // Earlier directories take precedence.
            if !commands.iter().any(|c| c.name == command.name) {
                commands.push(command);
            }
        }
    }
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands
}

/// The command called `name` (no leading slash), if a file defines it.
pub fn find(root: &Path, global: Option<&Path>, name: &str) -> Option<Command> {
    list(root, global).into_iter().find(|c| c.name == name)
}

/// Collect the commands under `dir`, whose names start with `prefix`.
fn walk(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<Command>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        // `file_type` does not follow symlinks, so a link to a directory is
        // skipped: no loops, and no walking out of the commands directory.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth < MAX_DEPTH && valid_segment(file_name) {
                let prefix = format!("{prefix}{file_name}:");
                walk(&entry.path(), &prefix, depth + 1, out);
            }
            continue;
        }
        let Some(stem) = file_name.strip_suffix(".md") else {
            continue;
        };
        if !valid_segment(stem) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        out.push(parse(format!("{prefix}{stem}"), &text, entry.path()));
    }
}

/// Whether a file or directory name can be part of a command name: something
/// one can type after a slash, with no `:` of its own (the separator).
fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn parse(name: String, text: &str, path: PathBuf) -> Command {
    let (fields, body) = front_matter(text);
    let field = |key: &str| {
        fields
            .iter()
            .find(|(k, v)| *k == key && !v.is_empty())
            .map(|(_, v)| v.clone())
    };
    let description = field("description").unwrap_or_else(|| {
        let first = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let first = first.trim().trim_start_matches('#').trim_start();
        if first.chars().count() > MAX_DESCRIPTION_CHARS {
            let cut: String = first.chars().take(MAX_DESCRIPTION_CHARS).collect();
            format!("{cut}…")
        } else {
            first.to_string()
        }
    });
    Command {
        name,
        description,
        argument_hint: field("argument-hint"),
        model: field("model"),
        body: body.trim().to_string(),
        path,
    }
}

/// Split a Markdown file into its front matter and the rest.
///
/// Front matter is a first line of exactly `---`, then `key: value` lines, up
/// to the next `---` line. This is not YAML: values are single-line strings,
/// optionally wrapped in one pair of quotes; lines without a colon and `#`
/// comments are skipped. The one multi-line form understood is a list:
/// `- item` lines under a key that has no value of its own become that key's
/// value, joined with `, ` (so an agent file may write `tools:` either way).
/// A file that does not open with `---`, or never closes it, has no front
/// matter and is all body.
pub fn front_matter(text: &str) -> (Vec<(&str, String)>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (Vec::new(), text);
    };
    let mut fields: Vec<(&str, String)> = Vec::new();
    let mut offset = 0;
    // The last key had no value on its own line: `- item` lines are its list.
    let mut list_open = false;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        let line = line.trim();
        if line == "---" {
            return (fields, &rest[offset..]);
        }
        if line.starts_with('#') {
            continue;
        }
        if list_open
            && let Some(item) = line.strip_prefix("- ")
            && let Some((_, value)) = fields.last_mut()
        {
            if !value.is_empty() {
                value.push_str(", ");
            }
            value.push_str(unquote(item.trim()));
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            let value = unquote(value.trim());
            list_open = value.is_empty();
            fields.push((key.trim(), value.to_string()));
        }
    }
    (Vec::new(), text)
}

/// Strip one pair of matching quotes around a front-matter value.
fn unquote(s: &str) -> &str {
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

/// A piece of a template.
#[derive(Debug, PartialEq)]
enum Segment<'a> {
    Text(&'a str),
    /// The command between `` !` `` and `` ` ``.
    Shell(&'a str),
}

/// Cut a template at its `` !`command` `` substitutions. An unclosed or empty
/// one is left as text.
fn segments(body: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut rest = body;
    let mut text_len = 0;
    while let Some(i) = rest[text_len..].find("!`") {
        let start = text_len + i;
        let after = &rest[start + 2..];
        match after.find('`') {
            Some(end) if !after[..end].trim().is_empty() && !after[..end].contains('\n') => {
                if start > 0 {
                    out.push(Segment::Text(&rest[..start]));
                }
                out.push(Segment::Shell(after[..end].trim()));
                rest = &after[end + 1..];
                text_len = 0;
            }
            // Not a substitution: keep it in the running text and look on.
            _ => text_len = start + 2,
        }
    }
    if !rest.is_empty() {
        out.push(Segment::Text(rest));
    }
    out
}

/// Replace `$ARGUMENTS` and `$1`…`$9` in template text, in one pass so the
/// inserted values are not themselves expanded.
fn substitute(text: &str, arguments: &str, words: &[String]) -> String {
    let mut out = String::with_capacity(text.len() + arguments.len());
    let mut rest = text;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(tail) = after.strip_prefix("ARGUMENTS") {
            out.push_str(arguments);
            rest = tail;
        } else if let Some(n) = after.chars().next().and_then(positional) {
            out.push_str(words.get(n - 1).map_or("", String::as_str));
            rest = &after[1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// `'1'..='9'` as a positional-parameter number.
fn positional(c: char) -> Option<usize> {
    c.to_digit(10).filter(|n| *n > 0).map(|n| n as usize)
}

/// Whether the template refers to its arguments anywhere (text or shell).
fn uses_arguments(body: &str) -> bool {
    body.match_indices('$').any(|(i, _)| {
        let after = &body[i + 1..];
        after.starts_with("ARGUMENTS") || after.chars().next().and_then(positional).is_some()
    })
}

/// Expand `command` with `arguments` (everything typed after its name) into
/// the prompt to send.
///
/// `approve` is asked before each `` !`command` `` runs, with the command
/// line exactly as the template has it; `false` abandons the expansion (the
/// `Err` says so). A command that fails or times out does not: its output and
/// status are inserted, so the model sees what happened.
///
/// A template that never mentions its arguments gets them appended as a
/// final paragraph, so `/review the parser` works with a template that is
/// just instructions.
pub fn expand(
    command: &Command,
    arguments: &str,
    root: &Path,
    approve: &mut dyn FnMut(&str) -> bool,
) -> Result<String, String> {
    // Prose after a command name need not be valid shell words ("it's"):
    // fall back to splitting on whitespace rather than refuse the command.
    let words = crate::shlex::split(arguments)
        .unwrap_or_else(|_| arguments.split_whitespace().map(String::from).collect());

    let mut out = String::new();
    for segment in segments(&command.body) {
        match segment {
            Segment::Text(text) => out.push_str(&substitute(text, arguments, &words)),
            Segment::Shell(line) => {
                if !approve(line) {
                    return Err(format!("not sent — `{line}` was not approved to run"));
                }
                out.push_str(&run_shell(line, arguments, &words, root));
            }
        }
    }
    if !arguments.is_empty() && !uses_arguments(&command.body) {
        out.push_str("\n\n");
        out.push_str(arguments);
    }
    Ok(out)
}

/// Run one `` !`command` `` and return what to insert in its place: its
/// stdout, plus a bracketed note (and stderr) when it failed or was killed.
///
/// The arguments reach the shell as `$1`… and `$ARGUMENTS`, never as text
/// spliced into `line` (see the module docs).
fn run_shell(line: &str, arguments: &str, words: &[String], root: &Path) -> String {
    let spawned = Process::new("sh")
        .arg("-c")
        .arg(line)
        // `$0`, then the positional parameters.
        .arg("atelier-command")
        .args(words)
        .env("ARGUMENTS", arguments)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => return format!("[could not run `{line}`: {e}]"),
    };
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    // Wait on the pipe rather than the process: output is complete when the
    // pipe closes, and a command that leaves something holding it open (a
    // backgrounded child) is cut off at the deadline like any slow one.
    let deadline = Instant::now() + SHELL_TIMEOUT;
    let Ok(output) = stdout.recv_timeout(SHELL_TIMEOUT) else {
        let _ = child.kill();
        let _ = child.wait();
        return format!(
            "[`{line}` timed out after {}s and was killed]",
            SHELL_TIMEOUT.as_secs()
        );
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };

    let mut text = output.trim_end().to_string();
    if !status.is_some_and(|s| s.success()) {
        let errors = stderr
            .recv_timeout(Duration::from_millis(200))
            .unwrap_or_default();
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&match status {
            Some(status) => format!("[`{line}` failed: {status}]"),
            None => format!("[`{line}` did not exit and was killed]"),
        });
        if !errors.trim().is_empty() {
            text.push('\n');
            text.push_str(errors.trim_end());
        }
    }
    text
}

/// Read a pipe to its end on a thread, keeping at most [`MAX_SHELL_OUTPUT`]
/// bytes (the rest is read and dropped, so the writer never blocks).
fn drain(pipe: Option<impl Read + Send + 'static>) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut truncated = false;
        let mut chunk = [0u8; 4096];
        if let Some(mut pipe) = pipe {
            while let Ok(n) = pipe.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let room = MAX_SHELL_OUTPUT - kept.len();
                kept.extend_from_slice(&chunk[..n.min(room)]);
                truncated |= n > room;
            }
        }
        let mut text = String::from_utf8_lossy(&kept).into_owned();
        if truncated {
            text.push_str("\n[output truncated]");
        }
        let _ = tx.send(text);
    });
    rx
}

/// The lines `/help` adds for the custom commands, or nothing when there are
/// none. `builtin` names the commands a file cannot define; such files are
/// left out, since typing their name would never reach them.
pub fn help(root: &Path, global: Option<&Path>, builtin: &dyn Fn(&str) -> bool) -> Vec<String> {
    let commands: Vec<Command> = list(root, global)
        .into_iter()
        .filter(|c| !builtin(&c.name))
        .collect();
    if commands.is_empty() {
        return Vec::new();
    }
    let mut out = vec!["custom commands:".to_string()];
    for c in commands {
        let usage = match &c.argument_hint {
            Some(hint) => format!("/{} {hint}", c.name),
            None => format!("/{}", c.name),
        };
        // Same column as the built-in list above it.
        out.push(format!("  {usage:<37} {}", c.description));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh project root and global directory under the system temp dir —
    /// never the real home.
    fn dirs(name: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "atelier-commands-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let (root, global) = (base.join("project"), base.join("global"));
        std::fs::create_dir_all(&root).unwrap();
        (root, global)
    }

    fn write(path: PathBuf, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn command(body: &str) -> Command {
        parse("t".to_string(), body, PathBuf::from("t.md"))
    }

    /// Expand with every shell line approved, in a temp directory.
    fn expand_in(name: &str, body: &str, arguments: &str) -> String {
        let (root, _) = dirs(name);
        expand(&command(body), arguments, &root, &mut |_| true).unwrap()
    }

    #[test]
    fn front_matter_is_optional_and_tolerant() {
        let (fields, body) = front_matter(
            "---\ndescription: \"Sync: the branch\"\n# a comment\nargument-hint: <branch>\nmodel: x\nnot a field\n---\nBody\n",
        );
        assert_eq!(
            fields,
            vec![
                ("description", "Sync: the branch".to_string()),
                ("argument-hint", "<branch>".to_string()),
                ("model", "x".to_string()),
            ]
        );
        assert_eq!(body, "Body\n");

        // No front matter, and front matter that never closes: all body.
        assert_eq!(front_matter("Just text\n"), (Vec::new(), "Just text\n"));
        let open = "---\ndescription: x\nBody\n";
        assert_eq!(front_matter(open), (Vec::new(), open));
        // A rule further down is not front matter.
        let rule = "Intro\n---\nmore\n";
        assert_eq!(front_matter(rule), (Vec::new(), rule));

        // `- item` lines are the list of a key without a value — and only that.
        let (fields, _) =
            front_matter("---\ntools:\n  - read\n  - \"mcp__x__*\"\nname: a\n- stray\n---\n");
        assert_eq!(
            fields,
            vec![
                ("tools", "read, mcp__x__*".to_string()),
                ("name", "a".to_string()),
            ]
        );
    }

    #[test]
    fn names_come_from_paths_and_projects_win() {
        let (root, global) = dirs("discovery");
        write(
            root.join(".atelier/commands/review.md"),
            "---\ndescription: from atelier\n---\nA",
        );
        write(root.join(".claude/commands/review.md"), "from claude");
        write(root.join(".claude/commands/git/sync.md"), "Sync $1\nmore");
        write(global.join("review.md"), "from global");
        write(
            global.join("standup.md"),
            "---\nargument-hint: [day]\n---\n# What happened",
        );
        // Not commands: wrong extension, hidden, a name one could not type.
        write(root.join(".atelier/commands/notes.txt"), "x");
        write(root.join(".atelier/commands/.hidden.md"), "x");
        write(root.join(".atelier/commands/two words.md"), "x");
        write(root.join(".atelier/commands/a:b.md"), "x");

        let commands = list(&root, Some(&global));
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["git:sync", "review", "standup"]);
        assert_eq!(commands[0].description, "Sync $1");
        assert_eq!(commands[1].description, "from atelier");
        assert_eq!(commands[1].body, "A");
        assert_eq!(commands[2].description, "What happened");
        assert_eq!(commands[2].argument_hint.as_deref(), Some("[day]"));
        assert_eq!(commands[2].model, None);

        assert_eq!(
            find(&root, None, "git:sync").unwrap().path,
            root.join(".claude/commands/git/sync.md")
        );
        assert!(find(&root, None, "standup").is_none());
        assert!(find(&root, Some(&global), "../review").is_none());
    }

    #[test]
    fn a_command_may_name_its_model() {
        let named = command("---\nmodel: \"second/big#high\"\n---\nReview it.");
        assert_eq!(named.model.as_deref(), Some("second/big#high"));
        assert_eq!(named.body, "Review it.");
        // An empty value is no choice.
        assert_eq!(command("---\nmodel:\n---\nx").model, None);
        assert_eq!(command("Just a template").model, None);
    }

    #[test]
    fn edits_apply_to_the_next_lookup() {
        let (root, _) = dirs("reread");
        let path = root.join(".atelier/commands/x.md");
        write(path.clone(), "one");
        assert_eq!(find(&root, None, "x").unwrap().body, "one");
        write(path.clone(), "two");
        assert_eq!(find(&root, None, "x").unwrap().body, "two");
        std::fs::remove_file(path).unwrap();
        assert!(find(&root, None, "x").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_are_not_walked() {
        let (root, global) = dirs("symlink");
        write(global.join("elsewhere.md"), "x");
        std::fs::create_dir_all(root.join(".atelier/commands")).unwrap();
        std::os::unix::fs::symlink(&global, root.join(".atelier/commands/link")).unwrap();
        assert!(list(&root, None).is_empty());
    }

    #[test]
    fn arguments_are_substituted_once() {
        assert_eq!(
            expand_in("args", "Fix $1 in $2 ($3). All: $ARGUMENTS.", "a 'b c'"),
            "Fix a in b c (). All: a 'b c'."
        );
        // A value that looks like a placeholder is not expanded again, and
        // other dollars are left alone.
        assert_eq!(
            expand_in("args-once", "$1 costs $5 or $x, $0 $", "'$2' two"),
            "$2 costs  or $x, $0 $"
        );
        // Prose with an apostrophe is not valid shell words; it still works.
        assert_eq!(
            expand_in("args-prose", "Do: $ARGUMENTS [$2]", "it's broken"),
            "Do: it's broken [broken]"
        );
    }

    #[test]
    fn unused_arguments_are_appended() {
        assert_eq!(
            expand_in("append", "Review the diff.", "only the parser"),
            "Review the diff.\n\nonly the parser"
        );
        assert_eq!(
            expand_in("append-none", "Review the diff.", ""),
            "Review the diff."
        );
        assert_eq!(
            expand_in("append-at", "Read @src/$1", "a.rs"),
            "Read @src/a.rs"
        );
    }

    #[test]
    fn templates_are_cut_at_shell_substitutions() {
        assert_eq!(
            segments("a !`echo hi` b !`x`"),
            vec![
                Segment::Text("a "),
                Segment::Shell("echo hi"),
                Segment::Text(" b "),
                Segment::Shell("x"),
            ]
        );
        // Unclosed, empty, or spanning lines: text.
        for literal in ["wow!`", "a !`` b", "a !`one\ntwo` b", "plain `code`!"] {
            assert_eq!(segments(literal), vec![Segment::Text(literal)]);
        }
        assert_eq!(
            segments("no !`\n then !`ok`"),
            vec![Segment::Text("no !`\n then "), Segment::Shell("ok")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn shell_output_replaces_the_substitution() {
        let (root, _) = dirs("shell");
        std::fs::write(root.join("marker"), "x").unwrap();
        let mut asked = Vec::new();
        let text = expand(
            &command("Files: !`ls` and !`printf '%s|' \"$1\" \"$ARGUMENTS\"` done"),
            "one two",
            &root,
            &mut |line| {
                asked.push(line.to_string());
                true
            },
        )
        .unwrap();
        // Runs in the project root; the approval saw the template's own
        // text, and the arguments reached the shell as parameters.
        assert_eq!(text, "Files: marker and one|one two| done");
        assert_eq!(asked, ["ls", "printf '%s|' \"$1\" \"$ARGUMENTS\""]);
    }

    #[cfg(unix)]
    #[test]
    fn arguments_and_output_cannot_inject_commands() {
        let (root, _) = dirs("inject");
        let mut asked = 0;
        // The argument tries to end the command and start another, and to
        // smuggle in a substitution; the output prints one too.
        let text = expand(
            &command("!`echo $1` / $1 / !`printf '!\\140touch out-pwned\\140 $2'`"),
            "'; touch arg-pwned; echo !`touch text-pwned`' x",
            &root,
            &mut |_| {
                asked += 1;
                true
            },
        )
        .unwrap();
        assert_eq!(asked, 2, "only the template's two commands ran");
        assert!(text.starts_with("; touch arg-pwned; echo !`touch text-pwned` / "));
        assert!(text.ends_with("!`touch out-pwned` $2"), "{text}");
        let left: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().collect();
        assert!(left.is_empty(), "nothing was created: {left:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_command_abandons_the_expansion() {
        let (root, _) = dirs("refused");
        let err = expand(
            &command("a !`touch ran` b !`touch ran-too`"),
            "",
            &root,
            &mut |_| false,
        )
        .unwrap_err();
        assert!(err.contains("`touch ran` was not approved"), "{err}");
        assert!(!root.join("ran").exists() && !root.join("ran-too").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_command_is_reported_in_place() {
        let text = expand_in("fails", "Got: !`echo partial; echo why >&2; exit 3`", "");
        assert!(text.starts_with("Got: partial\n[`echo partial;"), "{text}");
        assert!(text.contains("failed: exit status: 3]\nwhy"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn long_output_is_capped() {
        let text = expand_in("capped", "!`head -c 100000 /dev/zero | tr '\\0' x`", "");
        assert!(text.ends_with("\n[output truncated]"));
        assert!(text.len() < MAX_SHELL_OUTPUT + 100);
    }

    #[test]
    fn help_lists_commands_except_shadowed_ones() {
        let (root, global) = dirs("help");
        assert!(help(&root, Some(&global), &|_| false).is_empty());
        write(
            root.join(".atelier/commands/fix.md"),
            "---\ndescription: fix an issue\nargument-hint: <number>\n---\nFix #$1",
        );
        write(root.join(".atelier/commands/quit.md"), "never reached");
        write(global.join("standup.md"), "Summarise yesterday");
        let lines = help(&root, Some(&global), &|name| name == "quit");
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0], "custom commands:");
        assert!(lines[1].starts_with("  /fix <number> ") && lines[1].ends_with(" fix an issue"));
        assert!(lines[2].starts_with("  /standup ") && lines[2].ends_with(" Summarise yesterday"));
    }
}
