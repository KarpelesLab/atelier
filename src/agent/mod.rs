//! Agent loop: conversation state and turn orchestration.
//!
//! A [`Session`] owns the conversation and drives the tool-call loop: gather
//! context, call the provider, and while the model requests tools, execute them
//! (sandboxed, via the [`ToolRegistry`]) and feed the results back — repeating
//! until a turn produces no tool calls.
//!
//! Rendering goes through the [`Ui`] trait so the loop stays independent of the
//! terminal interface (the `tui` module supplies one; [`repl`] uses a plain
//! stdout one). Nothing here may depend on `tui`.

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;

use crate::config::Config;
use crate::context::{ContextItem, ContextProvider};
use crate::provider::{self, Completion, Message, StreamEvent, ToolCall};
use crate::tools::{FileState, ToolCtx, ToolRegistry};

const SYSTEM_PROMPT: &str = "\
You are atelier, an AI coding assistant operating inside a project directory. \
You have tools to read and modify files and run commands; use them to inspect \
the project and make changes rather than guessing. Keep responses concise. \
When you have completed the request, stop calling tools and give a short final \
answer.";

/// How the loop reports progress. Implemented by the TUI and by the plain REPL.
pub trait Ui {
    /// A chunk of the model's reasoning ("thinking").
    fn reasoning(&mut self, text: &str);
    /// A chunk of the model's answer.
    fn content(&mut self, text: &str);
    /// A tool is about to run, with its raw JSON arguments.
    fn tool_start(&mut self, name: &str, arguments: &str);
    /// A tool finished; `ok` is false when it returned an error result.
    fn tool_end(&mut self, name: &str, result: &str, ok: bool);
    /// The turn is complete (one full assistant response).
    fn turn_end(&mut self) {}
    /// An out-of-band notice (errors, status). Used by the TUI.
    #[allow(dead_code)]
    fn notice(&mut self, text: &str);
}

/// A conversation with its tools and context providers.
pub struct Session {
    cfg: Config,
    root: PathBuf,
    tools: ToolRegistry,
    context: Vec<Box<dyn ContextProvider>>,
    history: Vec<Message>,
    fstate: FileState,
}

impl Session {
    pub fn new(
        cfg: Config,
        root: PathBuf,
        tools: ToolRegistry,
        context: Vec<Box<dyn ContextProvider>>,
    ) -> Self {
        Self {
            cfg,
            root,
            tools,
            context,
            history: Vec::new(),
            fstate: FileState::new(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Run one user turn to completion, executing tool calls until the model
    /// stops requesting them. Progress is reported through `ui`.
    pub fn send(&mut self, user_input: &str, ui: &mut dyn Ui) -> Result<()> {
        self.history.push(Message::user(user_input));
        let context_msg = self.gather_context();

        loop {
            // Assemble the request: system prompt, fresh context, then history.
            let mut messages = Vec::with_capacity(self.history.len() + 2);
            messages.push(Message::system(SYSTEM_PROMPT));
            if let Some(ctx) = &context_msg {
                messages.push(Message::system(ctx.clone()));
            }
            messages.extend(self.history.iter().cloned());

            let specs = self.tools.specs();
            let completion: Completion =
                provider::stream_chat(&self.cfg, &messages, &specs, |ev| match ev {
                    StreamEvent::Reasoning(t) => ui.reasoning(t),
                    StreamEvent::Content(t) => ui.content(t),
                })?;

            self.history.push(Message::assistant_tool_calls(
                completion.content.clone(),
                completion.tool_calls.clone(),
            ));

            if completion.tool_calls.is_empty() {
                ui.turn_end();
                return Ok(());
            }

            for call in &completion.tool_calls {
                ui.tool_start(&call.name, &call.arguments);
                let (result, ok) = self.exec_tool(call);
                ui.tool_end(&call.name, &result, ok);
                self.history
                    .push(Message::tool_result(call.id.clone(), result));
            }
        }
    }

    /// Execute a single tool call, returning `(result_text, ok)`.
    fn exec_tool(&mut self, call: &ToolCall) -> (String, bool) {
        let args: Value = if call.arguments.trim().is_empty() {
            Value::Null
        } else {
            match serde_json::from_str(&call.arguments) {
                Ok(v) => v,
                Err(e) => return (format!("error: invalid arguments JSON: {e}"), false),
            }
        };
        let Some(tool) = self.tools.get(&call.name) else {
            return (format!("error: unknown tool '{}'", call.name), false);
        };
        let mut ctx = ToolCtx {
            project_root: &self.root,
            fstate: &mut self.fstate,
        };
        match tool.call(&mut ctx, args) {
            Ok(s) => (s, true),
            Err(e) => (format!("error: {e:#}"), false),
        }
    }

    /// Gather per-turn context into a single injected block, or `None`.
    fn gather_context(&self) -> Option<String> {
        let mut items: Vec<ContextItem> = self
            .context
            .iter()
            .filter_map(|p| p.gather(&self.root))
            .collect();
        if items.is_empty() {
            return None;
        }
        items.sort_by_key(|b| std::cmp::Reverse(b.priority));
        let mut s = String::from("Current project context:\n");
        for it in items {
            s.push_str(&format!("\n## {}\n{}\n", it.title, it.body));
        }
        Some(s)
    }
}

/// A plain stdout [`Ui`]: reasoning dimmed, answer normal, tools annotated.
pub struct StdoutUi {
    in_reasoning: bool,
}

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

impl StdoutUi {
    pub fn new() -> Self {
        Self {
            in_reasoning: false,
        }
    }
    fn end_reasoning(&mut self) {
        if self.in_reasoning {
            print!("{RESET}");
            println!();
            self.in_reasoning = false;
        }
    }
}

impl Default for StdoutUi {
    fn default() -> Self {
        Self::new()
    }
}

impl Ui for StdoutUi {
    fn reasoning(&mut self, text: &str) {
        if !self.in_reasoning {
            print!("{DIM}");
            self.in_reasoning = true;
        }
        print!("{text}");
        io::stdout().flush().ok();
    }
    fn content(&mut self, text: &str) {
        self.end_reasoning();
        print!("{text}");
        io::stdout().flush().ok();
    }
    fn tool_start(&mut self, name: &str, arguments: &str) {
        self.end_reasoning();
        println!("{DIM}⚙ {name} {arguments}{RESET}");
    }
    fn tool_end(&mut self, _name: &str, result: &str, ok: bool) {
        let mark = if ok { "✓" } else { "✗" };
        let preview: String = result.chars().take(200).collect();
        println!("{DIM}{mark} {preview}{RESET}");
    }
    fn turn_end(&mut self) {
        self.end_reasoning();
        println!();
    }
    fn notice(&mut self, text: &str) {
        eprintln!("{text}");
    }
}

/// A minimal stdin REPL driving a [`Session`] with [`StdoutUi`].
///
/// Used for headless runs and until the inline TUI (M2) lands.
pub fn repl(mut session: Session) -> Result<()> {
    eprintln!(
        "atelier — model {} @ {}",
        session.cfg.model, session.cfg.base_url
    );
    eprintln!("type a message, or /quit to exit, /models to list models.\n");

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
                match provider::list_models(session.config()) {
                    Ok(models) => println!("{}", models.join("\n")),
                    Err(e) => eprintln!("error: {e:#}"),
                }
                continue;
            }
            _ => {}
        }
        let mut ui = StdoutUi::new();
        if let Err(e) = session.send(input, &mut ui) {
            eprintln!("error: {e:#}");
        }
    }
    Ok(())
}
