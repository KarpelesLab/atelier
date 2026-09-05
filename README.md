# atelier

A minimal-TUI AI coding harness written in Rust. An agent loop that talks to
OpenAI-compatible model APIs, runs tools inside a project directory, speaks MCP,
and continuously feeds itself fresh context (git status, diagnostics, …) — from
a deliberately small terminal interface.

See [ROADMAP.md](ROADMAP.md) for the milestone plan and design rationale.

## Status

Early. **M0 is working**: a streaming conversation with the configured endpoint,
rendering the model's thinking separately from its answer. Tools, the inline
TUI, MCP, and context helpers are in progress (see the roadmap).

## Design in one breath

- **Bring your own API.** OpenAI-compatible endpoints only (no Claude/ChatGPT
  subscription backends — their ToS forbids it). HTTP is [`rsurl`](https://crates.io/crates/rsurl).
- **Minimal, append-only interface.** One input line plus a status strip;
  everything else prints to the terminal scrollback and is never redrawn.
- **Project-scoped.** Launched inside a project directory; tools, context, and
  permissions are anchored there.
- **The agent is fed, not just prompted.** Per-turn context providers inject the
  current repo state so the model reasons about now, not a stale snapshot.
- **One binary, organized by module** — not an internal crate workspace.

## Build & run

Requires a recent Rust toolchain (edition 2024, MSRV 1.95).

```sh
cargo run
```

Configuration is via environment (an `atelier.toml` overlay lands later):

| Variable            | Default                          | Meaning                         |
|---------------------|----------------------------------|---------------------------------|
| `ATELIER_BASE_URL`  | `http://192.168.0.50:11400/v1`   | OpenAI-compatible endpoint base |
| `ATELIER_MODEL`     | `qwen3.8-unc:q4`                 | Model id (thinking + vision)    |
| `ATELIER_API_KEY`   | *(unset)*                        | Optional bearer token           |

## Commands

Type `/help` for the list. Notable ones:

- `/models` — list the models the endpoint offers
- `/mcp` — list configured MCP servers
- `/mcp add <name> <command> [args...]` — connect an MCP server and register
  its tools (namespaced `mcp__<name>__<tool>`); the server is saved to
  `atelier.toml`
- `/mcp remove <name>` — drop a server and its tools
- `/quit` — exit (also Ctrl-D on an empty input)

## Project settings — `atelier.toml`

Durable, user-editable settings live in `atelier.toml` at the project root
(created and updated by `/mcp add`). MCP servers configured there are launched
and connected at startup:

```toml
[[mcp]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
```

## Layout

```
src/
├─ main.rs        entry: config, wiring, run loop
├─ config.rs      configuration
├─ provider/      OpenAI-compatible client: streaming chat, tool-calls
├─ agent/         agent loop, conversation state, turn orchestration
├─ tools/         built-in tools + registry
├─ mcp/           MCP client
├─ context/       per-turn context helpers
└─ tui/           minimal inline interface  [feature = "tui"]
```

## License

MIT — see [LICENSE](LICENSE).
