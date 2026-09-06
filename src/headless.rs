//! Non-interactive, one-shot mode: `--print`/`-p`.
//!
//! Runs a single prompt to completion and exits, instead of dropping into the
//! REPL/TUI. Intended for scripting and CI, so stdout carries *only* the
//! model's final answer — reasoning, tool activity, and informational
//! messages all go to stderr, so `atelier -p "..." | some-pipe` sees just the
//! answer text.

use std::io::{self, Read, Write};

use anyhow::Result;

use crate::agent::{Approval, Session, Ui};

/// Build the one-shot prompt from the CLI arguments that followed
/// `--print`/`-p`, joined with spaces. `None` means no arguments followed the
/// flag, signaling the caller should read the prompt from stdin instead.
pub fn prompt_from_args(args: &[String]) -> Option<String> {
    if args.is_empty() {
        None
    } else {
        Some(args.join(" "))
    }
}

/// Read the one-shot prompt from stdin to EOF (used when `--print`/`-p` is
/// given with no trailing arguments).
pub fn read_stdin_prompt() -> Result<String> {
    let mut s = String::new();
    io::stdin().read_to_string(&mut s)?;
    Ok(s.trim().to_string())
}

/// Run `prompt` to completion against `session` and print the answer to
/// stdout. Any error from the send (network, provider, etc.) propagates to
/// the caller, which prints it.
pub fn run(mut session: Session, prompt: &str) -> Result<()> {
    let mut ui = PrintUi;
    session.send(prompt, &mut ui)
}

/// A [`Ui`] for `--print`: only the model's answer goes to stdout so it stays
/// clean for piping. Everything else — reasoning, tool activity, info and
/// notices — goes to stderr.
struct PrintUi;

impl Ui for PrintUi {
    fn reasoning(&mut self, text: &str) {
        // Discard from stdout; surface on stderr for anyone watching the run.
        eprint!("{text}");
    }

    fn content(&mut self, text: &str) {
        print!("{text}");
        io::stdout().flush().ok();
    }

    fn tool_start(&mut self, name: &str, arguments: &str) {
        eprintln!("⚙ {name} {arguments}");
    }

    fn tool_end(&mut self, name: &str, result: &str, ok: bool) {
        let mark = if ok { "✓" } else { "✗" };
        eprintln!("{mark} {name}: {result}");
    }

    fn turn_end(&mut self) {
        println!();
        io::stdout().flush().ok();
    }

    fn info(&mut self, text: &str) {
        eprintln!("{text}");
    }

    /// Headless runs must never block on stdin waiting for a human. When a
    /// tool needs approval and auto-approve is off, deny it — the model is
    /// told the tool was denied and can try to proceed without it. Set
    /// `ATELIER_APPROVE=all` (see `Session::new`) to allow tools in headless
    /// runs; `Session` then skips approval entirely, so this is only ever
    /// reached when a tool genuinely wants a human in the loop and none is
    /// available.
    fn ask_approval(&mut self, _tool: &str, _arguments: &str) -> Approval {
        Approval::Deny
    }

    fn notice(&mut self, text: &str) {
        eprintln!("{text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_from_args_none_when_empty() {
        let args: Vec<String> = Vec::new();
        assert_eq!(prompt_from_args(&args), None);
    }

    #[test]
    fn prompt_from_args_single() {
        let args: Vec<String> = vec!["hello".to_string()];
        assert_eq!(prompt_from_args(&args), Some("hello".to_string()));
    }

    #[test]
    fn prompt_from_args_joins_with_spaces() {
        let args: Vec<String> = vec![
            "hello".to_string(),
            "there".to_string(),
            "world".to_string(),
        ];
        assert_eq!(
            prompt_from_args(&args),
            Some("hello there world".to_string())
        );
    }
}
