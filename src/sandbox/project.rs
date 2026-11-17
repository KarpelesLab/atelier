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
//! environment and a standard input, a command given as words rather than
//! as a shell line, and whether a program is in the image at all.
//!
//! What stays on the host, and why, is in `docs/sandbox.md` ("What runs
//! where").

use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use super::{Output, Setup};

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
/// would find it. Looked up in the root filesystem from outside
/// ([`in_root`](super::in_root)), without following links: in an image they
/// point into the image (`/bin/busybox`), not into the host.
pub fn in_image(setup: &Setup, program: &str) -> bool {
    if program.contains('/') {
        return false;
    }
    GUEST_PATH
        .split(':')
        .any(|dir| super::in_root(setup, &format!("{}/{program}", dir.trim_start_matches('/'))))
}

/// [`run`](super::run) for a command that reads its standard input and its
/// environment: `sh -c command` in a fresh guest, in the project, with
/// `env` in its environment and `input` on its stdin, byte for byte.
/// Neither goes through the command line or a file: the helper process
/// that runs the guest is handed both, and gives them to it.
pub fn run_with_input(
    command: &str,
    env: &[(&str, String)],
    input: &[u8],
    project_root: &Path,
    setup: &Setup,
    timeout: Duration,
) -> Result<Output> {
    super::run_with(command, env, Some(input), project_root, setup, timeout)
}

#[cfg(test)]
mod tests {
    use super::super::{Outcome, testing};
    use super::*;
    use std::path::PathBuf;

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
            image: false,
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
    /// and the input arrive as they are, and nothing is left in the project.
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
            "it's $HOME\n{\"a\":\"it's\\n\"}nixvm\n"
        );
        assert_eq!(run("wc -c", b""), "0\n");
        // One that does not read its input is not held up by it.
        assert_eq!(run("echo done", &[b'x'; 80_000]), "done\n");
        assert_eq!(run("wc -c", &[b'x'; 80_000]), "80000\n");
        // More than a command line or a pipe holds, and then not text:
        // byte for byte.
        assert_eq!(run("wc -c", &vec![b'x'; 300_000]), "300000\n");
        assert_eq!(run("od -An -c", b"a\0b"), "   a  \\0   b\n");
        // An environment variable is not a line of shell.
        assert_eq!(run("env | grep -c '^ATELIER_'", b""), "1\n");
        let left: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().collect();
        assert!(left.is_empty(), "{left:?}");
        // What `in_image` assumes of the guest.
        assert_eq!(run("echo \"$PATH\"", b""), format!("{GUEST_PATH}\n"));
        assert!(in_image(&setup, "sh"));
        assert!(!in_image(&setup, "no-such-program"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
