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
    run_with(dir, prompt, false)
}

/// Like [`run`]; `approve_all` sets `ATELIER_APPROVE=all`.
fn run_with(dir: &PathBuf, prompt: &str, approve_all: bool) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_atelier"));
    cmd.args(["--claude", "-p", prompt])
        .current_dir(dir)
        .env("ATELIER_MODEL", "haiku")
        .env_remove("ATELIER_CLAUDE_BUILTINS")
        .env_remove("ATELIER_APPROVE");
    if approve_all {
        cmd.env("ATELIER_APPROVE", "all");
    }
    cmd.output().expect("running atelier")
}

fn texts(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A solid-color PNG (stored deflate, no compression), `n`×`n` pixels.
fn png(rgb: [u8; 3], n: u32) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xffff_ffffu32;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xedb8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let body = [kind, data].concat();
        out.extend(&body);
        out.extend(crc32(&body).to_be_bytes());
    }
    // Each row: filter byte 0, then the pixels.
    let row: Vec<u8> = std::iter::once(0)
        .chain(rgb.iter().copied().cycle().take(3 * n as usize))
        .collect();
    let raw = row.repeat(n as usize);
    // zlib: header, one stored block per 65535 bytes, adler32.
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push(u8::from(i == blocks.len() - 1));
        z.extend((b.len() as u16).to_le_bytes());
        z.extend((!(b.len() as u16)).to_le_bytes());
        z.extend(*b);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    z.extend(((b << 16) | a).to_be_bytes());
    let mut ihdr = Vec::new();
    ihdr.extend(n.to_be_bytes());
    ihdr.extend(n.to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
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

/// With claude's built-ins switched off (the default), an ordinary request
/// lands on atelier's tools without naming them.
#[test]
#[ignore = "drives the real claude CLI on your account"]
fn builtins_are_replaced_by_default() {
    let dir = project("replaced");
    std::fs::write(dir.join("notes.txt"), "the answer is quokka\n").unwrap();
    let out = run(&dir, "What animal is mentioned in notes.txt? One word.");
    let (stdout, stderr) = texts(&out);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(stderr.contains("⚙ read "), "stderr: {stderr}");
    assert!(
        !stderr.contains("⚙ Read "),
        "claude's Read was used: {stderr}"
    );
    assert!(stdout.to_lowercase().contains("quokka"), "stdout: {stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Long-running commands: background job, then wait on it (Monitor's job).
#[test]
#[ignore = "drives the real claude CLI on your account"]
fn background_jobs_replace_monitor() {
    let dir = project("bg");
    let out = run_with(
        &dir,
        "Run `sleep 3; echo BUILD-OK` as a background job, wait for it to finish, \
         then reply with exactly what it printed.",
        true,
    );
    let (stdout, stderr) = texts(&out);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(stderr.contains("⚙ bash "), "stderr: {stderr}");
    assert!(stderr.contains("⚙ bash_output "), "stderr: {stderr}");
    assert!(stdout.contains("BUILD-OK"), "stdout: {stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `read` returns images as images: the model sees the picture.
#[test]
#[ignore = "drives the real claude CLI on your account"]
fn read_shows_images() {
    let dir = project("image");
    std::fs::write(dir.join("swatch.png"), png([220, 20, 20], 32)).unwrap();
    let out = run(
        &dir,
        "Look at swatch.png. What single color fills it? Answer with one word.",
    );
    let (stdout, stderr) = texts(&out);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(stderr.contains("⚙ read "), "stderr: {stderr}");
    assert!(stdout.to_lowercase().contains("red"), "stdout: {stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}
