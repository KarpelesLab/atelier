//! Risk-signal detection for unconfined tool calls (roadmap M9).
//!
//! Before the user is asked to approve an unconfined tool (notably `bash`),
//! [`signals`] returns short, human-readable warnings about what the call might
//! do — so the prompt shows *why* something is risky instead of an opaque
//! command string.
//!
//! # Contract / plan for the implementer (owns `src/risk.rs`)
//!
//! Implement [`signals`]: given a tool name and its raw JSON arguments, return a
//! list of concise warnings. For `bash`, parse the `command` field and scan for
//! dangerous patterns — network access (`curl`/`wget`/pipes to a shell),
//! privilege escalation (`sudo`), recursive/forced deletion (`rm -rf`),
//! writes/paths outside the project (absolute paths, `~`, `..`), overwriting
//! shell configs, `chmod`/`chown`, fork bombs, etc. Keep messages short (they
//! render one per line at the prompt). Return an empty Vec when nothing stands
//! out. Add unit tests. Do not edit files outside `src/risk.rs`.

/// Warnings about what a tool call might do, for the approval prompt.
pub fn signals(_tool: &str, _arguments: &str) -> Vec<String> {
    // Stub: no signals yet. Replaced by real detection.
    Vec::new()
}
