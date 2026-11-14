//! atelier as a library: an interface of your own in front of the agent.
//!
//! ```sh
//! cargo run --example embed -- "what does this project do?"
//! ```
//!
//! The session is the one `atelier` itself would open in this directory
//! (same configuration, tools, permission rules); only the interface
//! differs — here, a few lines on stdout that refuse every approval.

use std::io::Write;
use std::process::ExitCode;

use atelier::{Approval, FileChange, Ui};

/// What a turn reports, and what it asks. A graphical interface would put
/// these in its own widgets, and answer `ask_approval` from a dialog.
struct Plain;

impl Ui for Plain {
    fn reasoning(&mut self, _text: &str) {}

    fn content(&mut self, text: &str) {
        print!("{text}");
        let _ = std::io::stdout().flush();
    }

    fn tool_start(&mut self, name: &str, arguments: &str) {
        println!("\n[{name} {arguments}]");
    }

    fn tool_end(&mut self, name: &str, _result: &str, ok: bool) {
        println!("[{name}: {}]", if ok { "ok" } else { "failed" });
    }

    fn file_change(&mut self, _tool: &str, change: &FileChange) {
        println!("[changed {}]", change.path.display());
    }

    fn turn_end(&mut self) {
        println!();
    }

    fn info(&mut self, text: &str) {
        println!("{text}");
    }

    fn notice(&mut self, text: &str) {
        eprintln!("{text}");
    }

    fn ask_approval(&mut self, tool: &str, _arguments: &str) -> Approval {
        eprintln!("[{tool} needs approval: refused]");
        Approval::Deny
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // atelier starts copies of the running program for its helper
    // processes (the coordinator, the local model's server, …): let it.
    if let Some(code) = atelier::app::helper(&args) {
        return code;
    }
    let prompt = args.join(" ");
    if prompt.is_empty() {
        eprintln!("usage: embed PROMPT");
        return ExitCode::from(2);
    }

    let mut ui = Plain;
    let result = atelier::app::open_session(false, None).and_then(|mut session| {
        for line in session.start_hooks(false) {
            ui.notice(&line);
        }
        for line in session.connect_configured_mcp() {
            ui.notice(&line);
        }
        let sent = session.send(&prompt, &mut ui);
        // Background jobs and language servers do not outlive the program.
        atelier::app::finish(sent)
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
