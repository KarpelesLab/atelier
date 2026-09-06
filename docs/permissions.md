# Permissions

atelier's approval model is keyed on **confinement**, not on whether an
action mutates something: a tool that provably cannot leave the project
directory runs without asking; a tool that can run arbitrary, unbounded code
always asks (or is pre-authorized).

## Confined vs. unconfined

- **Confined = automatic.** `read`, `write`, `edit`, `grep`, `glob`, and `ls`
  all resolve paths through `tools::confine` (`ToolCtx::resolve`), which
  rejects anything that normalizes outside the project root. Because they
  cannot escape, they override `Tool::requires_approval` to `false` and never
  prompt — including `write` and `edit`. The `node` tool's `fs` is confined
  the same way (see [Scripting](scripting.md)), so a plain `node` call (no
  `network`) is also auto-approved.
- **Unconfined = ask.** `Tool::requires_approval` defaults to `true`, and
  only the tools above override it. That leaves:
  - **`bash`** — runs anything via `sh -c`.
  - **Every MCP tool** (`mcp__<server>__<tool>`) — `McpTool` doesn't override
    the default, so any tool from any connected server always requires
    approval, regardless of what it actually does.
  - **`node` with `network: true`** — opts into `fetch`/`httpGet`/
    `httpRequest`, at which point the call is treated as unconfined
    (`NodeTool::requires_approval` returns `true` exactly when `network` is
    set).

## The approval prompt

When a tool call needs approval, the agent loop (`Session::send`) first
prints any [risk signals](#risk-signals-bash-only) for the call, then asks
via `Ui::ask_approval`, offering three answers:

| Answer     | Effect |
|------------|--------|
| **Once**   | Run this one call; ask again next time. |
| **Always** | Run it, and remember this **tool name** (not the specific arguments) as approved for the rest of this session — persisted immediately to `atelier.toml` under `[permissions] allow`, so it's remembered across restarts too. |
| **Deny**   | Refuse. The model receives `"error: the user denied permission to run this tool."` as the tool result and can adapt (try something else, ask the user, explain why it's stuck) instead of the harness silently blocking it. |

"Always" grants apply to the whole tool name, so approving `bash` once
approves every future `bash` call (any command), and approving one MCP
server's tool doesn't approve another tool from the same or a different
server — each `mcp__<server>__<tool>` name is independent.

## Headless mode: `ATELIER_APPROVE`

Set `ATELIER_APPROVE=all` (also accepts `yes` or `1`) to skip every approval
prompt for the session — for CI or scripted/unattended runs. This is checked
before the per-tool `allow` set, so it overrides everything unconditionally;
there is no way to auto-approve only some tool classes via this variable.

## Risk signals (`bash` only)

Before asking for approval on a `bash` call, `src/risk.rs` scans the command
string for patterns worth calling out explicitly, so the prompt shows *why*
something is risky rather than just an opaque command line. Each matched
category prints as `⚠ <signal>` above the prompt:

| Signal | Detects |
|--------|---------|
| recursive force delete (`rm -rf`) | `rm` with both a recursive and a force flag, or a recursive `rm` targeting `/` or `/*` |
| runs with elevated privileges (sudo) | `sudo`, `doas`, or `su` |
| network access | `curl`, `wget`, `nc`, or `ftp` |
| pipes downloaded content into a shell | a pipe (`\|`) into `sh`/`bash`/`zsh`/`dash`/`ksh` (optionally through `sudo`/`env`) |
| touches paths outside the project | a mutating command (`rm`, `mv`, `cp`, `chmod`, `chown`, `chgrp`, `dd`, `tee`, `mkdir`, `rmdir`, `ln`, `shred`, `truncate`) or a `>`/`>>` redirect targeting an absolute, `~`-relative, or `..`-traversing path |
| modifies shell startup files | a path containing `.bashrc`, `.zshrc`, `.profile`, or `.bash_profile` |
| changes file permissions/ownership | `chmod` or `chown` |
| raw disk/device operation | `dd`, any `mkfs*`, or a redirect into `/dev/*` |
| possible fork bomb | the literal `:(){` pattern |
| destructive git operation | `git push --force`/`-f`, `git reset --hard`, or `git clean` with a force flag |

Signals are informational only — they don't block the call or change whether
approval is required, and they only ever apply to `bash` (MCP and `node`
calls show no risk signals today). A command can trigger multiple distinct
signals at once (each category is reported at most once per call).

## Scope not yet covered

- Approval is per tool **name**, not per call — there's no way to, say,
  always-allow `bash` for read-only commands but still prompt for writes.
- MCP tools have no risk-signal analysis; approval is a flat yes/no per tool
  name regardless of what the underlying server does.
- `node`'s `fs` has no path-based approval carve-outs beyond the
  in-project/network split — see [Scripting](scripting.md) for what's
  planned (`⏳ async fs; out-of-project fs via approval` in the roadmap).
