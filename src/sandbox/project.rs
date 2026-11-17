//! The project's own commands in the guest: its hooks and its diagnostics
//! checkers.
//!
//! With the sandbox on, `bash` runs unasked in a guest that can write the
//! project. A command atelier runs by itself *on the host* and that executes
//! what is in the project — a project hook calling `./scripts/check.sh`,
//! `cargo check` running a `build.rs` — would then be the way out: the agent
//! edits the script in the guest, and atelier runs it outside. So those go
//! through [`run`](super::run) too, the entry point `bash` uses, and this
//! module holds what they need on top of it, which `bash` does not: an
//! environment and a standard input (the guest is handed neither), a command
//! given as words rather than as a shell line, and whether a program is in
//! the image at all.
//!
//! What stays on the host, and why, is in `docs/sandbox.md` ("What runs
//! where").

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use super::{Output, Setup};

/// The longest command line that carries a command's input inside it. The
/// kernel takes no argument longer than 128 KiB, and the line is one
/// argument of the helper process; past this the input goes through a file.
const MAX_INLINE: usize = 96 * 1024;

/// Where the guest's shell looks for programs: the `PATH` the guest is
/// given (a test asks a guest for it, so the two cannot drift apart).
const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// `word` as one word of a shell command line.
pub fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The shell line that runs `argv` as it is, with no word of it expanded —
/// for a command that is run without a shell on the host.
pub fn join(argv: &[String]) -> String {
    let words: Vec<String> = argv.iter().map(|word| quote(word)).collect();
    words.join(" ")
}

/// Why `what` ("project hooks", "checkers") did not run with the sandbox on
/// and unable to run anything: said instead of running it on the host.
pub fn unavailable(what: &str, why: &str) -> String {
    format!(
        "not run: with the sandbox on, {what} run in it, and it is unavailable here ({why}). \
         Nothing was run on the host instead"
    )
}

/// Whether the image has a program called `program` where the guest's shell
/// would find it. Looked up in the root filesystem from outside, without
/// following links: in an image they point into the image (`/bin/busybox`),
/// not into the host.
pub fn in_image(setup: &Setup, program: &str) -> bool {
    if program.contains('/') {
        return false;
    }
    GUEST_PATH.split(':').any(|dir| {
        let path = setup.rootfs.join(dir.trim_start_matches('/')).join(program);
        std::fs::symlink_metadata(path).is_ok_and(|meta| !meta.is_dir())
    })
}

/// [`run`](super::run) for a command that reads its standard input and its
/// environment: `sh -c command` in a fresh guest, in the project, with
/// `env` exported and `input` on its stdin.
///
/// The guest has no stdin and no environment of the host's, so both travel
/// in the command line: the variables as `export`s, the input as a
/// here-document (which ends it with a newline if it had none). An input
/// too large for that, or one that is not text, is put in a file at the top
/// of the project — the one place both sides can see — for the length of
/// the run.
pub fn run_with_input(
    command: &str,
    env: &[(&str, String)],
    input: &[u8],
    project_root: &Path,
    setup: &Setup,
    timeout: Duration,
) -> Result<Output> {
    let mut line = String::new();
    for (name, value) in env {
        line.push_str(&format!("export {name}={}; ", quote(value)));
    }
    line.push_str("sh -c ");
    line.push_str(&quote(command));
    if input.is_empty() {
        line.push_str(" </dev/null");
        return super::run(&line, project_root, setup, timeout);
    }
    if let Ok(text) = std::str::from_utf8(input)
        && !text.contains('\0')
        && line.len() + text.len() <= MAX_INLINE
    {
        let mark = delimiter(text);
        let newline = if text.ends_with('\n') { "" } else { "\n" };
        line.push_str(&format!(" <<'{mark}'\n{text}{newline}{mark}\n"));
        return super::run(&line, project_root, setup, timeout);
    }
    let file = InputFile::create(project_root, input)?;
    line.push_str(&format!(" <{}", quote(&file.0.display().to_string())));
    super::run(&line, project_root, setup, timeout)
}

/// A here-document delimiter that is no line of `text`.
fn delimiter(text: &str) -> String {
    let mut n = 0;
    loop {
        let mark = format!("ATELIER_INPUT_{n}");
        if !text.lines().any(|line| line == mark) {
            return mark;
        }
        n += 1;
    }
}

/// A command's input as a file in the project, removed when dropped.
struct InputFile(PathBuf);

impl InputFile {
    fn create(project_root: &Path, input: &[u8]) -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = project_root.join(format!(
            ".atelier-sandbox-input-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // `create_new`: never through a link the guest left at that name.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("writing {}", path.display()))?;
        let file_guard = InputFile(path);
        std::io::Write::write_all(&mut file, input)
            .with_context(|| format!("writing {}", file_guard.0.display()))?;
        Ok(file_guard)
    }
}

impl Drop for InputFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Outcome, testing};
    use super::*;

    fn project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atelier-sandbox-project-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn words_are_quoted_one_by_one() {
        let argv = ["echo", "it's", "$HOME; rm", ""].map(String::from);
        assert_eq!(join(&argv), r#"'echo' 'it'\''s' '$HOME; rm' ''"#);
    }

    #[test]
    fn the_delimiter_is_no_line_of_the_input() {
        assert_eq!(delimiter("{}"), "ATELIER_INPUT_0");
        assert_eq!(
            delimiter("a\nATELIER_INPUT_0\nATELIER_INPUT_1\n"),
            "ATELIER_INPUT_2"
        );
    }

    #[test]
    fn a_program_is_looked_for_in_the_image_not_on_the_host() {
        let root = project("image");
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        std::fs::create_dir_all(root.join("bin/sub")).unwrap();
        std::fs::write(root.join("usr/bin/ruff"), b"").unwrap();
        // As an image has it: a link that leads nowhere on the host.
        #[cfg(unix)]
        std::os::unix::fs::symlink("/bin/busybox-of-the-image", root.join("bin/npx")).unwrap();
        #[cfg(not(unix))]
        std::fs::write(root.join("bin/npx"), b"").unwrap();
        let setup = Setup {
            rootfs: root.clone(),
            memory_mb: 64,
            interp: false,
            network: Vec::new(),
            helper: None,
            notes: Vec::new(),
        };
        assert!(in_image(&setup, "ruff"));
        assert!(in_image(&setup, "npx"));
        assert!(!in_image(&setup, "sub"), "a directory is not a program");
        // Whatever the host has.
        assert!(!in_image(&setup, "sh"));
        assert!(!in_image(&setup, "../usr/bin/ruff"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Through a real guest (see `testing::live_setup`): the environment
    /// and the input arrive, by the command line and by the file.
    #[test]
    fn a_command_in_the_guest_gets_its_environment_and_its_input() {
        let Some(setup) = testing::live_setup() else {
            return;
        };
        let root = project("input");
        let run = |command: &str, input: &[u8]| {
            let env = [("ATELIER_EVENT", "it's $HOME".to_string())];
            let out = run_with_input(command, &env, input, &root, &setup, Duration::from_secs(60))
                .unwrap();
            assert_eq!(
                out.outcome,
                Outcome::Exited(0),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap()
        };
        // A command with its own quotes, lines and a comment at its end.
        assert_eq!(
            run(
                "echo \"$ATELIER_EVENT\"\ncat; uname -n # the guest",
                b"{\"a\":\"it's\\n\"}"
            ),
            "it's $HOME\n{\"a\":\"it's\\n\"}\nnixvm\n"
        );
        assert_eq!(run("wc -c", b""), "0\n");
        // One that does not read its input is not held up by it.
        assert_eq!(run("echo done", &[b'x'; 80_000]), "done\n");
        assert_eq!(run("wc -c", &[b'x'; 80_000]), "80001\n");
        // Too large for a command line, and then not text: through a file,
        // byte for byte, which is gone afterwards.
        assert_eq!(run("wc -c", &vec![b'x'; 300_000]), "300000\n");
        assert_eq!(run("od -An -c", b"a\0b"), "   a  \\0   b\n");
        let left: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().collect();
        assert!(left.is_empty(), "{left:?}");
        // What `in_image` assumes of the guest.
        assert_eq!(run("echo \"$PATH\"", b""), format!("{GUEST_PATH}\n"));
        assert!(in_image(&setup, "sh"));
        assert!(!in_image(&setup, "no-such-program"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
