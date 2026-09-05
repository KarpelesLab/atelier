//! atelier — a minimal-TUI AI coding harness over OpenAI-compatible APIs.
//!
//! M0: a plain stdin REPL that holds a streaming conversation with the
//! configured endpoint, rendering the model's thinking separately from its
//! answer. The real inline TUI arrives in M2 (`src/tui`).

mod agent;
mod config;
mod context;
mod mcp;
mod provider;
mod tools;
#[cfg(feature = "tui")]
mod tui;

use std::io::{self, Write};

use anyhow::Result;

use config::Config;
use provider::{Message, StreamEvent};

// ANSI: dim for the thinking channel, reset afterward.
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

fn main() -> Result<()> {
    let cfg = Config::from_env();
    eprintln!("atelier — model {} @ {}", cfg.model, cfg.base_url);
    eprintln!("type a message, or /quit to exit, /models to list models.\n");

    let mut history: Vec<Message> = Vec::new();
    let stdin = io::stdin();
    loop {
        print!("› ");
        io::stdout().flush().ok();

        let mut input = String::new();
        if stdin.read_line(&mut input)? == 0 {
            break; // EOF (Ctrl-D)
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        match input {
            "/quit" | "/exit" => break,
            "/models" => {
                match provider::list_models(&cfg) {
                    Ok(models) => println!("{}", models.join("\n")),
                    Err(e) => eprintln!("error: {e:#}"),
                }
                continue;
            }
            _ => {}
        }

        history.push(Message::user(input));

        // Stream the reply. Thinking is dimmed; the answer prints normally.
        let mut in_reasoning = false;
        let result = provider::stream_chat(&cfg, &history, |ev| match ev {
            StreamEvent::Reasoning(t) => {
                if !in_reasoning {
                    print!("{DIM}");
                    in_reasoning = true;
                }
                print!("{t}");
                io::stdout().flush().ok();
            }
            StreamEvent::Content(t) => {
                if in_reasoning {
                    println!("{RESET}");
                    in_reasoning = false;
                }
                print!("{t}");
                io::stdout().flush().ok();
            }
        });
        if in_reasoning {
            print!("{RESET}");
        }
        println!();

        match result {
            Ok(answer) => history.push(Message::assistant(answer)),
            Err(e) => {
                eprintln!("error: {e:#}");
                history.pop(); // drop the user turn we couldn't answer
            }
        }
    }

    Ok(())
}
