# atelier

A minimal-TUI AI coding harness written in Rust. An agent loop that talks to
OpenAI-compatible model APIs, runs tools inside a project directory, speaks
MCP, and continuously feeds itself fresh context (git status, diagnostics,
project layout) — from a deliberately small terminal interface.

See [ROADMAP.md](ROADMAP.md) for the milestone plan and design rationale.

## Design in one breath

- **Bring your own API.** OpenAI-compatible endpoints only (no Claude/ChatGPT
  subscription backends — their ToS forbids it). HTTP is [`rsurl`](https://crates.io/crates/rsurl).
- **Minimal, append-only interface.** One input line plus a status strip;
  everything else prints to the terminal scrollback and is never redrawn.
- **Project-scoped.** Launched inside a project directory; tools, context, and
  permissions are anchored there.
- **The agent is fed, not just prompted.** Per-turn context providers inject
  the current repo state (git status, diff, layout, `cargo check`
  diagnostics) so the model reasons about now, not a stale snapshot.
- **Confinement over coarse yes/no.** Every file tool and the `node`
  scripting tool are sandboxed to the project root and run without
  prompting; only genuinely unconfined execution (`bash`, MCP tools,
  networked scripts) asks for approval.
- **Sessions persist and compact themselves.** Conversations are saved to
  `.atelier/session.json` and resumable with `--continue`; once a
  conversation grows past a token threshold, older turns are summarized
  automatically instead of being sent in full on every request.
- **One binary, organized by module** — not an internal crate workspace.

## Quickstart

```sh
cargo build --release   # edition 2024, MSRV 1.95
cargo run                # launches in the current directory as project root
```

Type a message, or `/help` for commands. Set `ATELIER_BASE_URL` /
`ATELIER_MODEL` / `ATELIER_API_KEY` to point at your own endpoint. Add
`--continue`/`-c` to resume a saved session, or `--print`/`-p "..."` for a
non-interactive, scriptable one-shot run. See
[docs/quickstart.md](docs/quickstart.md) for the full walkthrough (env vars,
first conversation, the REPL/TUI fallback).

## Documentation

| Doc | Covers |
|-----|--------|
| [Quickstart](docs/quickstart.md) | Build/run, first conversation, env vars, `--print`, `/image`, REPL vs. inline TUI |
| [Sessions](docs/sessions.md) | Persistence, `--continue`/`/new`, automatic compaction |
| [Configuration](docs/configuration.md) | Env vars reference + `atelier.toml` (`[[mcp]]`, `[[mcp_http]]`, `[permissions]`) |
| [Tools](docs/tools.md) | Every built-in tool's parameters and behavior |
| [Permissions](docs/permissions.md) | The confinement/approval model, risk signals |
| [Scripting](docs/scripting.md) | The `node` tool: sandboxed JS, `fs`, optional network |
| [MCP](docs/mcp.md) | Connecting MCP servers (stdio + HTTP), tool namespacing |

## Layout

```
src/
├─ main.rs        entry: config, flags (--continue, --print), wiring, run loop
├─ config.rs      env-based runtime config
├─ settings.rs    atelier.toml (MCP servers, permissions)
├─ session.rs     on-disk session persistence (.atelier/session.json)
├─ headless.rs    --print: one-shot non-interactive mode
├─ provider/      OpenAI-compatible client: streaming chat, tool-calls, vision
├─ agent/         agent loop, conversation state, turn orchestration, compaction
├─ tools/         built-in tools + registry (read/write/edit/multiedit/bash/…)
├─ js/            the `node` tool: mediated JS runtime (fs, console, network)
├─ mcp/           MCP client (stdio + HTTP transports)
├─ context/       per-turn context helpers
├─ risk.rs        risk-signal detection for bash approval prompts
└─ tui/           minimal inline interface, mid-turn steering  [feature = "tui", default on]
```

## License

MIT — see [LICENSE](LICENSE).
