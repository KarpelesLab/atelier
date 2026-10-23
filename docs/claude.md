# Claude backend

With `--claude` (or `ATELIER_BACKEND=claude`), atelier stops calling a model
API itself and instead acts as a frontend for the **Claude Code CLI you have
already installed and signed in to**:

```sh
atelier --claude                         # interactive
ATELIER_MODEL=sonnet atelier --claude    # pick a model (passed as --model)
atelier --claude --continue              # resume the last Claude session here
atelier --claude -p "explain src/main.rs"
```

## How it works

atelier launches the unmodified `claude` binary in headless stream-json mode
(`src/claude/mod.rs`):

```
claude -p --input-format stream-json --output-format stream-json --verbose \
       --include-partial-messages --permission-prompt-tool stdio \
       --append-system-prompt <...> [--model M] [--resume ID] [--mcp-config JSON]
```

One process serves the whole conversation. Each turn is written to its stdin
as a JSON line (tagged with a `uuid`, whose `command_lifecycle` events mark
when that turn is done), and its events are rendered through the normal
interface:

| CLI event | atelier |
|-----------|---------|
| `stream_event` text / thinking deltas | streamed answer / dimmed reasoning |
| `assistant` message `tool_use` blocks | `⚙ tool args` |
| `user` message `tool_result` blocks | `✓` / `✗ result` |
| `control_request` `can_use_tool` | the usual approval prompt (`⚠` risk signals for `Bash`) |
| `result` | end of turn, token usage |

**What Claude Code owns:** authentication, the model, the agent loop, its
built-in tools, its permission rules (`~/.claude/settings.json`, project
`.claude/`), compaction, and the conversation itself (stored by Claude Code;
atelier only keeps the session id in `.atelier/claude-session`).

**What atelier adds:**

- **The interface.** Same TUI/REPL/`--print` as the OpenAI backend.
- **Its own tools, in place of claude's.** atelier's built-in tools
  (project-confined file tools with read-before-edit tracking,
  `apply_patch`, the sandboxed `node` runtime, `bash`, …) are offered to
  claude as the `mcp__atelier__*` MCP server, and **claude's overlapping
  built-ins are switched off** (see [Tools](#tools)). They run *in the atelier process*, with its
  approval policy: confined tools run without asking, `bash` and other
  unconfined tools ask through atelier's prompt (claude is told to allow the
  server wholesale, so nothing is asked twice). They show under their atelier
  names (`⚙ read …`). See [How atelier's tools reach claude](#how-ateliers-tools-reach-claude).
- **MCP servers.** Everything in `atelier.toml` (`[[mcp]]`, `[[mcp_http]]`)
  is passed via `--mcp-config` instead of being connected by atelier.
  `/mcp add`/`remove` relaunch claude on the next message, resuming the same
  session.
- **Per-turn context.** atelier's context providers (git status, diff,
  layout, diagnostics) are prepended to each message in an
  `<atelier-context>` block, deduplicated across turns.

## Tools

If you're using atelier you expect atelier's tools, so by default claude's
own versions are switched off (`--disallowedTools`) and atelier's replace
them; the system prompt tells claude which is which:

| claude built-in | atelier replacement |
|---|---|
| `Read` | `read` — text, images, PDFs (`pdftotext`), notebooks; outside the project with approval |
| `Write`, `Edit`, `MultiEdit` | `write`, `edit`, `multiedit`, `apply_patch` |
| `NotebookEdit` | `notebook_edit` |
| `Bash` | `bash` (`background: true` for long-running commands) |
| `Monitor`, `BashOutput`, `KillShell` | `bash_output` (`wait_ms`, `until`), `bash_kill` |
| `Glob`, `Grep`, `LS` | `glob`, `grep`, `ls`, `tree` |
| `WebFetch` | `web_fetch` (HTML → text, paged) |
| `TodoWrite` | `todo` |

Everything without an atelier counterpart stays: subagents (`Agent`/`Task`),
`WebSearch`, `LSP`, `AskUserQuestion`, plan mode, skills, worktrees, cron, …
Subagents get the same set (atelier's tools included). atelier's tools are
marked to load up front (`anthropic/alwaysLoad`), not deferred behind
`ToolSearch`.

`ATELIER_CLAUDE_BUILTINS=keep` leaves claude's built-ins on, next to
atelier's. If atelier's tools can't be offered (no Unix sockets, or an MCP
server already named `atelier`), claude's are never switched off.

## How atelier's tools reach claude

claude can only use extra tools from MCP servers it launches itself, while
atelier's tools need the live session (project root, read tracking, approval
prompt, "always allow" list). So the server claude launches is a relay:

```
claude ──stdio MCP──▶ atelier mcp-serve <socket> ──Unix socket──▶ atelier (your session)
```

atelier listens on a socket in a private (0700) temporary directory and
passes the relay a random token through the `--mcp-config` entry's
environment; connections without it are dropped. The relay only pipes bytes.
The session answers `initialize`/`tools/list` itself and runs each
`tools/call` in its event loop — during a turn, or while idle (a background
subagent using atelier's tools) — prompting through the normal interface.

The server is named `atelier`; if `atelier.toml` already configures an MCP
server by that name, atelier's tools aren't offered. Unix only.

## Stopping a turn, and background agents

**Background work is never killed unless you explicitly ask.** Background
subagents and shells live inside the `claude` process, and claude's own
`interrupt` request stops them along with the turn (as its TUI does on
Ctrl-C). So atelier never sends a plain interrupt:

- **Ctrl-C** (on an empty input with nothing queued) *preempts* the turn: it
  queues a short stop message and asks claude to deliver it now
  (`interrupt` with `send_now`). The running turn ends the way it would for
  a newly typed message, claude replies with one line, and background work
  carries on.
- When background work finishes, claude reports back in a turn of its own;
  atelier shows it as it arrives, even while idle at the prompt.
- Anything that would end the `claude` process waits for, or asks about,
  running background tasks: `/new` needs `/new force`; quitting (`/quit`,
  Ctrl-D) has to be repeated; an `/mcp` change applies once they finish;
  `--print` and piped input wait for them before exiting. The process is
  only relaunched after an error if it actually died.

`cargo test live_stop -- --ignored` checks the Ctrl-C property against the
real CLI, and `cargo test --test claude_live -- --ignored` checks atelier's
tools through the relay and as replacements — reading, images, background
jobs, approval policy (both run a few cheap Haiku turns on your account).

## Permissions

claude only asks when its own permission rules say a call needs approval;
those requests come to atelier's prompt. "Always" applies the permission
update Claude Code suggests, so it is remembered the way Claude Code would
remember it. `ATELIER_APPROVE=all` allows every request. In `--print` mode
without it, requests are denied (nobody is there to answer).

## Limitations (for now)

- atelier's tools can't push notifications into claude's turn, so a
  background job is waited on with `bash_output` rather than reporting in by
  itself the way claude's own background shells do.
- Messages typed mid-turn are sent as the next prompt after the turn ends,
  not injected into the running turn.
- Ctrl-C while claude is still on a *queued* message (behind a turn it
  started itself) stops that turn, and the queued message then runs.
- Subagent activity is shown only as its single top-level tool call.
- `/models` doesn't list anything; use `ATELIER_MODEL`.

## Terms of use

atelier never handles Claude credentials or offers a sign-in: `claude`
authenticates however you configured it, and requests go from your `claude`
process to Anthropic. Using an API key (`ANTHROPIC_API_KEY`) with Claude Code
is unambiguous. For use with a Claude subscription, review Anthropic's
current terms ([Claude Code legal and compliance](https://code.claude.com/docs/en/legal-and-compliance))
for your situation.
