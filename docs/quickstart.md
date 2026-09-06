# Quickstart

## Build

Requires a recent Rust toolchain (edition 2024, MSRV 1.95).

```sh
cargo build --release
```

The `tui` feature (the inline terminal interface) is on by default. To build a
headless binary without it (drops the `crossterm` dependency entirely):

```sh
cargo build --release --no-default-features
```

## Run

```sh
cargo run
```

atelier treats the current working directory as the **project root**: every
file tool, `bash`, and the `node` scripting tool are confined to it (see
[permissions](permissions.md)). Launch it from inside the project you want to
work on.

On startup it connects any MCP servers configured in `atelier.toml` (see
[MCP](mcp.md)), then either opens the inline TUI or, if not attached to a real
terminal, falls back to a plain line-based REPL (see below).

## First conversation

```
atelier — model qwen3.8-unc:q4 @ http://192.168.0.50:11400/v1
type a message, or /help for commands.

› read main.rs and tell me what the entry point does
⚙ read {"path":"main.rs"}
✓ 1  fn main() { ...
The entry point ...
```

Type `/help` to see the available slash commands (`/models`, `/mcp`, `/new`,
`/quit`, …) — see [tools](tools.md) and [MCP](mcp.md) for what the agent itself can do,
and [permissions](permissions.md) for how tool approval works.

## The REPL vs. the inline TUI

atelier has two front ends over the same [`Session`](../src/agent/mod.rs)
agent loop:

- **Inline TUI** (`src/tui`, feature `tui`, on by default) — a single input
  line plus a reverse-video status strip (model · project dir · git branch ·
  turn count · token usage). Everything else — assistant text, reasoning,
  tool activity — is printed once to the terminal scrollback and never
  redrawn. Ctrl-C clears the current input; Ctrl-D on an empty line exits.
- **Plain REPL** (`src/agent::repl`) — reads lines from stdin with a `›`
  prompt and prints straight to stdout, no raw mode, no redraw. Used for
  headless/scripted runs.

The TUI is only used when both stdin *and* stdout are a real terminal
(`IsTerminal`); **if either is redirected — piped input, captured output, a
CI job — atelier automatically falls back to the plain REPL**, which reads
stdin to EOF. You don't need to select this explicitly.

## Environment variables

| Variable                  | Default                          | Meaning |
|----------------------------|-----------------------------------|---------|
| `ATELIER_BASE_URL`        | `http://192.168.0.50:11400/v1`   | OpenAI-compatible endpoint base URL |
| `ATELIER_MODEL`           | `qwen3.8-unc:q4`                 | Model id to request |
| `ATELIER_API_KEY`         | *(unset)*                        | Optional bearer token sent as `Authorization: Bearer <key>` |
| `ATELIER_APPROVE`         | *(unset)*                        | Set to `all`, `yes`, or `1` to auto-approve every tool call (headless/CI runs) |
| `ATELIER_HTTP_TIMEOUT_MS` | *(unset)*                        | Overrides the HTTP connect timeout (ms) for both chat streaming (default 60000ms) and `GET /models` (default 15000ms). Ignored if not a positive integer |
| `ATELIER_DEBUG`           | *(unset)*                        | If set (to anything), prints the raw outgoing chat-completion request JSON to stderr |

See [configuration](configuration.md) for the full picture including
`atelier.toml`.

## Sessions

The conversation is saved to `.atelier/session.json` under the project root
after each turn. Resume it in a later run:

```sh
cargo run -- --continue   # or -c
```

`--continue` restores the prior history (you'll see `resumed session (N
message(s))`). Start fresh at any time with the `/new` command, which clears the
in-memory history and deletes the saved session. Add `.atelier/` to your
project's `.gitignore`.

## Next steps

- [Configuration](configuration.md) — env vars + `atelier.toml`
- [Tools](tools.md) — the built-in tool set
- [Permissions](permissions.md) — the approval model
- [Scripting](scripting.md) — the `node` tool
- [MCP](mcp.md) — connecting external tool servers
