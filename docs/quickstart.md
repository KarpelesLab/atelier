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

Type `/help` to see the available slash commands (`/models`, `/model`, `/tools`, `/mcp`, `/new`,
`/image`, `/clear`, `/quit`, …) — see [tools](tools.md) and [MCP](mcp.md) for
what the agent itself can do, and [permissions](permissions.md) for how tool
approval works.

## Vision: `/image`

```
› /image screenshot.png
attached image (1 staged) — it will be sent with your next message
› what's wrong with this layout?
```

`/image <path>` reads a local image file, base64-encodes it as a `data:`
URL, and stages it (the MIME type is guessed from the extension — jpg/jpeg,
gif, webp, bmp, else PNG). Staged images are attached to your **next**
message (as OpenAI multimodal `image_url` content parts) and then cleared;
call `/image` more than once before sending to attach several at once. The
path is your own — unlike file tools, it isn't confined to the project root.
Vision support depends on the model behind your endpoint.

## Flags: `--continue` and `--print`

```sh
atelier --continue                       # or -c: resume the saved session
atelier --print "summarize src/main.rs"  # or -p: one-shot, non-interactive
atelier -p < prompt.txt                  # -p with no trailing args reads stdin
atelier --continue --print "keep going"  # resume, then run one prompt headlessly
```

`--print`/`-p` runs a single prompt to completion and exits instead of
opening the TUI/REPL — for scripts and CI. Only the model's final answer
goes to stdout (clean for piping); reasoning, tool activity, and
informational output all go to stderr. Because nothing can answer an
approval prompt in this mode, tool calls that need approval are denied
automatically unless `ATELIER_APPROVE=all` is set (see
[Permissions](permissions.md)). See [Sessions](sessions.md) for what
`--continue` restores and how compaction keeps long conversations bounded.

## The REPL vs. the inline TUI

atelier has two front ends over the same [`Session`](../src/agent/mod.rs)
agent loop:

- **Inline TUI** (`src/tui`, feature `tui`, on by default) — a single input
  line plus a reverse-video status strip (model · project dir · git branch ·
  turn count · token usage). Everything else — assistant text, reasoning,
  tool activity — is printed once to the terminal scrollback and never
  redrawn. Ctrl-C clears the current input; Ctrl-D on an empty line exits.
  While the model is working the strip shows a spinner and elapsed time
  (`⠋ working 12s`) and the input line stays live: press Enter to **queue**
  a message (echoed as `(queued)`, counted in the strip) instead of sending
  it — this is mid-turn steering: you can react to what the model is doing
  without waiting for the turn to end. Queued messages are handed to the
  model at its next step — between tool calls — or, if the turn has already
  ended, sent as the next turn; queued slash commands run once the turn is
  over, in order, exactly as if just typed. Note there is no way to cancel a
  turn already in flight — Ctrl-C never interrupts the model mid-response,
  it only clears the current (unsent) input, or — on an empty input while a
  turn runs — drops whatever is queued so far.
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
| `ATELIER_APPROVE`         | *(unset)*                        | Set to `all`, `yes`, or `1` to auto-approve every tool call (headless/CI runs, or `--print` with unconfined tools) |
| `ATELIER_CONTEXT_LIMIT`   | `8000`                           | Token threshold past which older history is compacted into a summary (see [Sessions](sessions.md)) |
| `ATELIER_HTTP_TIMEOUT_MS` | *(unset)*                        | Overrides the HTTP connect timeout (ms) for both chat streaming (default 60000ms) and `GET /models` (default 15000ms). Ignored if not a positive integer |
| `ATELIER_DEBUG`           | *(unset)*                        | If set (to anything), prints the raw outgoing chat-completion request JSON to stderr |
| `ATELIER_TRACE`           | *(unset)*                        | File path; appends one JSONL line per model request (messages + response) for after-the-fact inspection |

See [configuration](configuration.md) for the full picture including
`atelier.toml`.

## Sessions

The conversation is saved to `.atelier/session.json` under the project root
after each turn. Resume it in a later run with `--continue`/`-c` (you'll see
`resumed session (N message(s))`); start fresh at any time with `/new`, which
clears the in-memory history and deletes the saved file. Long conversations
are compacted automatically — older turns are summarized and dropped once
the context grows past a threshold — so you don't need to manage this by
hand. Add `.atelier/` to your project's `.gitignore`. See
[Sessions](sessions.md) for the full picture.

## Next steps

- [Configuration](configuration.md) — env vars + `atelier.toml`
- [Sessions](sessions.md) — persistence, `--continue`, compaction
- [Tools](tools.md) — the built-in tool set
- [Permissions](permissions.md) — the approval model
- [Scripting](scripting.md) — the `node` tool
- [MCP](mcp.md) — connecting external tool servers
