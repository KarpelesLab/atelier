# atelier as a library

The `atelier` program is a library and a five-line `main` that calls it. A
Rust program can depend on the same library and put an interface of its own
— a window, a web page, a bot — in front of the same agent, in the same
process, with no protocol in between.

```toml
[dependencies]
atelier = { git = "https://github.com/KarpelesLab/atelier", default-features = false }
```

`default-features = false` leaves out atelier's own terminal interface
(`tui`). The crate is not on crates.io, and its version is the program's:
pin a tag or a commit.

## The three pieces

```rust
use atelier::{Approval, Ui};

struct MyUi { /* your widgets */ }

impl Ui for MyUi {
    fn reasoning(&mut self, text: &str) { /* a chunk of thinking */ }
    fn content(&mut self, text: &str) { /* a chunk of the answer */ }
    fn tool_start(&mut self, name: &str, arguments: &str) {}
    fn tool_end(&mut self, name: &str, result: &str, ok: bool) {}
    fn info(&mut self, text: &str) {}
    fn notice(&mut self, text: &str) {}
    fn ask_approval(&mut self, tool: &str, arguments: &str) -> Approval {
        Approval::Deny // or ask the user: Once, Always, Deny
    }
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = atelier::app::helper(&args) {
        return code; // see "Helper processes" below
    }
    let mut ui = MyUi { /* … */ };
    let mut session = atelier::app::open_session(false, None).unwrap();
    session.send("what does this project do?", &mut ui).unwrap();
    atelier::app::finish(());
    std::process::ExitCode::SUCCESS
}
```

- **[`app::open_session`]** sets up a `Session` in the current directory
  exactly as the program does: configuration from the environment and the
  `atelier.toml` files, the built-in tools, permission rules, project trust,
  the link to the other instances on the machine. Its arguments are
  `--claude` and the permission mode.
- **[`Ui`]** is how a turn talks to the interface: streamed text and
  reasoning, tool calls and their results, file changes
  (`file_change`, with the before and after), approvals (`ask_approval`),
  questions from the model (`ask_user`), messages typed mid-turn
  (`take_queued`) and interruption (`interrupted`). Seven methods are
  required; the rest have defaults.
- **[`Session`]** is the conversation. `send` runs a turn and returns when
  it is over; `agent::dispatch` runs a line that may be a slash command.
  The rest is what atelier's own interfaces use: `set_model`, `set_mode`,
  `stage_images`, `resume`, `new_conversation`, the MCP calls, `usage`,
  `settings`, `poll_background` and `poll_peers` for what happens between
  turns.

`examples/embed.rs` is a complete program of forty lines:

```sh
cargo run --example embed -- "what does this project do?"
```

## What to know

- **Blocking, one thread.** `send` blocks until the turn ends and calls the
  `Ui` on the calling thread. An interface with an event loop runs the
  session on a thread of its own and talks to it over channels — which is
  how atelier's terminal interface (`src/tui`) and `atelier serve`
  (`src/serve.rs`) are built; both are short and worth reading first.
- **The current directory is the project.** A session works in the
  process's working directory, so one process holds one project's sessions.
- **Helper processes.** atelier starts copies of the running program for
  the coordinator, the local model's server, the Claude relay and the
  sandbox. Either call `atelier::app::helper(&args)` first thing in `main`,
  as above, so that your program answers those invocations; or name an
  installed `atelier` with `atelier::app::set_helper_exe(path)`.
- **stderr.** Setting up a session reports configuration warnings on
  stderr.
- **On the way out**, `atelier::app::finish` stops background `bash` jobs
  and language servers.
- **Everything is public, little is promised.** Every module is `pub`,
  because atelier's own interfaces are written against them and yours may
  need the same (`tools` to register a tool, `provider` to call a model,
  `permissions`, `mcp`, `coord`, …). What is meant to stay put is what the
  crate root re-exports: `Session`, `Ui`, `Approval`, `Question`, `Answer`,
  `FileChange`, `Config`, `Settings`, `Mode`, and the `app` functions named
  here. There is no semver promise before 1.0.

## Or a protocol

A front end that is not written in Rust, or that should live in another
process, drives the same session over JSON-RPC instead: see
[the embedding protocol](protocol.md) (`atelier serve`) and
[ACP](acp.md).
