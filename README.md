# atelier

A minimal-TUI AI coding harness written in Rust. An agent loop that talks to
OpenAI-compatible model APIs, runs tools inside a project directory, speaks
MCP, and continuously feeds itself fresh context (git status, diagnostics,
project layout) — from a deliberately small terminal interface.

See [ROADMAP.md](ROADMAP.md) for the milestone plan and design rationale.

## Design in one breath

- **Bring your own API.** OpenAI-compatible endpoints by default — no
  subscription backend required. HTTP is [`rsurl`](https://crates.io/crates/rsurl).
- **Sign in with ChatGPT.** `/login` runs OpenAI's own sanctioned OAuth flow
  so you can run atelier on your own ChatGPT plan instead of an API key —
  a separate backend over the Responses API, distinct from the
  bring-your-own-API default above. See
  [docs/chatgpt.md](docs/chatgpt.md).
- **Or front your Claude Code.** `--claude` makes atelier a frontend for your
  own installed Claude Code CLI: the unmodified `claude` binary does its own
  auth and model calls. See [docs/claude.md](docs/claude.md).
- **Minimal, append-only interface.** One input line plus a status strip;
  everything else prints to the terminal scrollback and is never redrawn.
- **Project-scoped.** Launched inside a project directory; tools, context, and
  permissions are anchored there.
- **The agent is fed, not just prompted.** Per-turn context providers inject
  the current repo state (git status, diff, layout, and what the project's
  own lint/typecheck commands say after each edit) so the model reasons
  about now, not a stale snapshot.
- **Your rules, where you already keep them.** `AGENTS.md` (or `CLAUDE.md`,
  `.cursorrules`) is read from your user config and every directory from the
  repository root down, with `@path` imports; a subdirectory's file applies
  once the agent works there. See
  [Configuration](docs/configuration.md#instruction-files).
- **Confinement over coarse yes/no.** Every file tool and the `node`
  scripting tool are sandboxed to the project root and run without
  prompting; only genuinely unconfined execution (`bash`, MCP tools,
  networked scripts) asks for approval.
- **Sessions persist and compact themselves.** Conversations are saved to
  `.atelier/session.json` and resumable with `--continue`; once a
  conversation grows past a token threshold, old tool output is pruned and,
  if needed, older turns become a structured hand-off summary (`/compact` on
  demand, `/context` to see where the window goes). Requests are laid out so
  each is a prefix of the next, which keeps the inference server's prompt
  cache warm from turn to turn.
- **A parallel reviewer, opt-in.** Turn on the "subconscious" (`/review on`)
  and a second, read-only model pass watches the exchange and edits,
  posting short 💭 notes into the dialog — it can comment but never act.
- **One binary, organized by module** — not an internal crate workspace.

## Install

Linux (x86_64 or aarch64) — a static binary from the latest
[release](https://github.com/KarpelesLab/atelier/releases):

```sh
case $(uname -m) in
  x86_64)  target=x86_64-unknown-linux-fullrust ;;
  aarch64) target=aarch64-unknown-linux-musl ;;
esac
curl -fsSL "https://github.com/KarpelesLab/atelier/releases/latest/download/atelier-$target.gz" \
  | gunzip > ~/.local/bin/atelier
chmod +x ~/.local/bin/atelier
```

The x86_64 binary is built with [fullrust](https://github.com/KarpelesLab/fullrust):
it contains no libc and needs nothing but a Linux kernel. The arm64 one is
static against musl.

Release binaries **keep themselves up to date**: at most once a day, at
startup, atelier checks for a newer release in the background, verifies the
download against the release's `SHA256SUMS`, and swaps it in for the next launch
(with a one-line notice). `atelier update` does it on demand;
`ATELIER_NO_UPDATE=1` turns the background check off. Builds from source
never update themselves.

Publishing a release: [release-plz](https://release-plz.dev) keeps a release
PR open on master (version bump + `CHANGELOG.md`, from the commit messages).
Merging it tags `vX.Y.Z` and creates the release;
`.github/workflows/release-binaries.yml` then builds the binaries, attaches
them with their checksums, and publishes it.

## Quickstart

```sh
cargo build --release   # edition 2024, MSRV 1.95
cargo run                # launches in the current directory as project root
```

Type a message, or `/help` for commands. Set `ATELIER_BASE_URL` /
`ATELIER_MODEL` / `ATELIER_API_KEY` to point at your own endpoint. Add
`--continue`/`-c` to resume a saved session, or `--print`/`-p "..."` for a
non-interactive, scriptable run (JSON output, step limits, schema-checked
answers: [docs/headless.md](docs/headless.md)). See
[docs/quickstart.md](docs/quickstart.md) for the full walkthrough (env vars,
first conversation, the REPL/TUI fallback).

## Documentation

| Doc | Covers |
|-----|--------|
| [Quickstart](docs/quickstart.md) | Build/run, first conversation, env vars, `--print`, `/image`, REPL vs. inline TUI |
| [Headless runs](docs/headless.md) | `--print` for scripts and CI: `--output-format json\|stream-json` and its events, `--input-format stream-json`, `--max-turns`, `--output-schema`, `--goal`, exit codes; `atelier review` (a read-only review of a change, for you or CI) |
| [Troubleshooting](docs/troubleshooting.md) | `atelier doctor`: a checklist of the setup (config files, endpoint, model, instruction files, hooks, MCP, …); `atelier debug prompt`: the exact first request a prompt would make, without sending it |
| [GitHub Action](docs/github-action.md) | `uses: KarpelesLab/atelier@v0`: review pull requests with inline comments, run a prompt on a schedule, answer `@atelier` in issue and pull request comments — inputs, example workflows, the safety model |
| [Embedding protocol](docs/protocol.md) | `atelier serve`: the session as JSON-RPC 2.0 over stdio, for editors and other front ends — start, steer and interrupt turns, answer approvals and questions, run commands; events, error codes, how ACP maps onto it |
| [Sessions](docs/sessions.md) | Persistence, `--continue`/`/new`, automatic compaction |
| [Configuration](docs/configuration.md) | Env vars reference + `atelier.toml` (`[[mcp]]`, `[[mcp_http]]`, `[permissions]`, `[review]`), named providers, a model per role, effort/sampling variants, the usage log and `atelier stats`, instruction files (`AGENTS.md`), the `/config` settings screen |
| [Tools](docs/tools.md) | Every built-in tool's parameters and behavior |
| [Memory](docs/memory.md) | What the agent remembers between sessions: the `memory` tool, `.atelier/memory/`, `/memory` |
| [Custom commands](docs/commands.md) | Your own slash commands as Markdown files: `.atelier/commands/`, `$ARGUMENTS`/`$1`, `` !`command` `` output |
| [Skills](docs/skills.md) | `SKILL.md` directories the agent loads on demand: `.atelier/skills/` (also `.claude/skills/`, `.agents/skills/`), the `skill` tool, `/skills` |
| [Goals](docs/goals.md) | `/goal <condition>` and `--goal`: keep the agent working until a condition holds, checked by a judge at each turn's end; `[goals]` bounds |
| [Hooks](docs/hooks.md) | Commands run on events (before/after a tool call, on a prompt, at turn end) that can observe or decide: `[[hooks]]`, the stdin/stdout contract, trust, `/hooks`, examples |
| [Subagents](docs/agents.md) | The `task` tool: `explore` and `general`, your own agents as Markdown files in `.atelier/agents/` (also `.claude/agents/`), the step cap, parallel runs, background agents that report when done, worktree isolation, `/agents` |
| [Project trust](docs/trust.md) | What a project's `atelier.toml` may not do until you accept it (start MCP servers, run checkers and hooks, add providers, allow commands): `/trust`, `atelier trust`, `ATELIER_TRUST_PROJECT`, what is gated and why |
| [Permissions](docs/permissions.md) | The confinement/approval model, rules, modes (`plan`, `ask-edits`, `auto-edit`, `yolo`), risk signals |
| [Sandboxed `bash`](docs/sandbox.md) | Experimental (`--features sandbox`): commands in a nixvm guest that sees only the project — setup, what works there, measurements, what is missing |
| [Scripting](docs/scripting.md) | The `node` tool: sandboxed JS, `fs`, optional network, calling other tools from a script (code mode) |
| [MCP](docs/mcp.md) | Connecting MCP servers (stdio + HTTP), tool namespacing, deferred tools and code mode; [`atelier mcp`](docs/mcp.md#atelier-as-an-mcp-server): atelier's own tools as an MCP server for another agent |
| [Review](docs/review.md) | The parallel "subconscious" reviewer: `/review`, `[review]`, `ATELIER_REVIEW` — and how it differs from `atelier review` |
| [Sign in with ChatGPT](docs/chatgpt.md) | `/login`/`/logout`, the Responses API backend, token storage |
| [Claude backend](docs/claude.md) | `--claude`: driving your Claude Code CLI from atelier |
| [Coordinator](docs/coordinator.md) | How running ateliers find and message each other: `/peers`, `/send`, topics, the `peers`/`peer_send` tools |

## Layout

```
src/
├─ main.rs        entry: config, wiring, run loop, exit status
├─ cli.rs         command-line flags (--continue, --print and its options)
├─ config.rs      env-based runtime config
├─ settings.rs    atelier.toml (MCP servers, permissions)
├─ session.rs     on-disk session persistence (.atelier/session.json)
├─ headless.rs    --print: non-interactive runs, JSON output, exit codes
├─ serve.rs       serve: the embedding protocol (JSON-RPC 2.0 over stdio)
├─ events.rs      the event payloads --print's stream-json and serve share
├─ schema.rs      the JSON Schema subset behind --output-schema
├─ provider/      OpenAI-compatible client: streaming chat, tool-calls, vision
├─ claude/        Claude backend: drives the `claude` CLI; serves it atelier's tools (mcp-serve)
├─ agent/         agent loop, conversation state, turn orchestration, compaction
├─ tools/         built-in tools + registry (read/write/edit/multiedit/bash/…)
├─ js/            the `node` tool: mediated JS runtime (fs, console, network)
├─ mcp/           MCP client (stdio + HTTP transports); `atelier mcp`, the server
├─ context/       per-turn context helpers
├─ risk.rs        risk-signal detection for bash approval prompts
├─ sandbox.rs     bash commands in a nixvm guest                [feature = "sandbox", off]
├─ trust.rs       project trust: which atelier.toml settings wait for /trust
└─ tui/           minimal inline interface, mid-turn steering  [feature = "tui", default on]
```

## License

MIT — see [LICENSE](LICENSE).
