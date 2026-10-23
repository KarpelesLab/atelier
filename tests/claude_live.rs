//! Live tests of the Claude backend against the real `claude` CLI, through
//! the real `atelier` binary. Ignored by default: they need Claude Code
//! installed and signed in, and run a few cheap Haiku turns on that account.
//!
//! ```sh
//! cargo test --test claude_live -- --ignored
//! ```

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

/// A fresh, empty project directory.
fn project(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("atelier-claude-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `atelier --claude -p <prompt>` in `dir`, on Haiku, no auto-approval.
fn run(dir: &PathBuf, prompt: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atelier"))
        .args(["--claude", "-p", prompt])
        .current_dir(dir)
        .env("ATELIER_MODEL", "haiku")
        .env_remove("ATELIER_APPROVE")
        .output()
        .expect("running atelier")
}

/// claude calls atelier's own (confined, so unprompted) `read` tool, which
/// runs in the atelier process via the `mcp-serve` relay.
#[test]
#[ignore = "drives the real claude CLI on your account"]
fn claude_uses_atelier_tools() {
    let dir = project("read");
    std::fs::write(dir.join("secret.txt"), "pumpernickel\n").unwrap();
    let out = run(
        &dir,
        "Use the mcp__atelier__read tool (not your own Read tool) to read secret.txt, \
         then reply with only the word it contains.",
    );
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "stderr: {stderr}");
    // Shown under atelier's own tool name, not `mcp__atelier__read`.
    assert!(stderr.contains("⚙ read "), "stderr: {stderr}");
    assert!(
        stdout.to_lowercase().contains("pumpernickel"),
        "stdout: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// atelier's `bash` needs approval under atelier's policy — claude doesn't
/// ask separately — and `--print` (nobody to ask) denies it.
#[test]
#[ignore = "drives the real claude CLI on your account"]
fn atelier_policy_gates_its_tools() {
    let dir = project("bash");
    let out = run(
        &dir,
        "Use the mcp__atelier__bash tool (not your own Bash tool) to run `touch marker`. \
         If it is denied, do not retry or use another tool; just say so.",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(stderr.contains("⚙ bash "), "stderr: {stderr}");
    assert!(stderr.contains("denied permission"), "stderr: {stderr}");
    assert!(!dir.join("marker").exists(), "the denied command ran");
    let _ = std::fs::remove_dir_all(&dir);
}
