# Hooks

A hook is a shell command atelier runs when something happens in a session:
before a tool call, after one, when you submit a prompt, when the agent
stops. A hook can just watch (log, notify), and for some events it can
decide: refuse a tool call, rewrite its arguments, answer an approval
prompt, add context for the model, or send the agent back to work.

Formatting after an edit, protecting a path, and a notification when a turn
ends are not features of atelier — they are hooks, [shown below](#examples).

## Configuring

Hooks are `[[hooks]]` tables in `atelier.toml`:

```toml
[[hooks]]
event = "post_tool"            # which event (see the table below)
match = "edit|write|multiedit" # optional: regex on the tool name
command = "rustfmt --edition 2024 \"$ATELIER_FILE\""
timeout_secs = 30              # optional, default 60
```

| Field | Type | Meaning |
|-------|------|---------|
| `event` | string | One of the [events](#events) |
| `match` | string | A regex the **whole** tool name must match (`edit` does not match `multiedit`; write `edit\|multiedit` or `.*edit`). Only for the tool events (`pre_tool`, `post_tool`, `permission_request`); without it the hook runs for every tool |
| `command` | string | Run with `sh -c`, in the project root |
| `timeout_secs` | integer | The command — and everything it started — is killed after this long (default 60) |

Two files are read:

| File | Hooks in it |
|------|-------------|
| `<project>/atelier.toml` | the project's: run only once [you have trusted the project](#trust) |
| `$XDG_CONFIG_HOME/atelier/atelier.toml`, else `~/.config/atelier/atelier.toml` | yours, for every project: always run. Only `[[hooks]]` is read from this file |

Several hooks on one event run in order, yours first, then the project's in
the order written. Hooks are read when atelier starts; edits take effect at
the next start.

An entry atelier can't use — an unknown event, a `match` that isn't a valid
regex — is skipped with a warning at startup; the others still load.

## Trust

Hooks are arbitrary commands, and a project's `atelier.toml` can arrive with
a repository you just cloned. So **project hooks do not run until you have
trusted the project** — one decision that covers its hooks together with
everything else in the file that runs commands or grants access (MCP
servers, checkers, providers, allow rules). [Project trust](trust.md) has
the whole of it; for hooks it means:

- When an interactive session starts in a project you haven't trusted — or
  whose gated settings changed since you did — atelier lists them, hooks
  included, and runs none of the project's hooks. Read them, then type
  `/trust`.
- The decision is a SHA-256 over those settings (for a hook: event, `match`,
  command and timeout, in order), stored per project path in
  `$XDG_STATE_HOME/atelier/trust.json` (else `~/.local/state/atelier/`) —
  outside the project, so neither the repository nor the agent (whose file
  tools are confined to the project) can approve anything.
- Any change to the set — a `git pull` that edits a command, the agent
  editing `atelier.toml` — makes the project untrusted again at the next
  start.
- A headless run (`--print`) never asks. In an untrusted project the hooks
  are skipped, with one notice on stderr, unless `ATELIER_TRUST_PROJECT=1`
  (the project's settings, all of them) or `ATELIER_TRUST_HOOKS=1` (its hooks
  alone) is set — either runs them whatever they are, so set it only where
  you control the checkout (CI on your own repository).
- Hooks you approved with `/hooks trust` before the rest was gated keep
  running as long as they are unchanged; if the project configures other
  gated settings, those wait for `/trust`.

Hooks in your user-global file are yours and need no approval.

| Command | Does |
|---------|------|
| `/hooks` | lists every hook, where it comes from (`user`, `project`), and whether the project's are trusted |
| `/hooks trust` | the same as `/trust`: shows what the project's file asks for and accepts it as it is now. Hooks run from then on (`session_start` hooks: from the next session) |

## What a hook receives

**stdin** — one JSON object:

```json
{
  "event": "pre_tool",
  "cwd": "/home/you/project",
  "session": { "backend": "openai", "model": "qwen3-coder:30b" },
  "tool": "write",
  "arguments": { "path": "src/main.rs", "content": "…" }
}
```

`event`, `cwd` (the project root) and `session` (`backend` is `openai`,
`chatgpt` or `claude`) are always there; the rest depends on the event.

**Environment**, for hooks that don't want to parse JSON:

| Variable | Set to |
|----------|--------|
| `ATELIER_EVENT` | the event name |
| `ATELIER_PROJECT` | the project root |
| `ATELIER_TOOL` | the tool's name (tool events) |
| `ATELIER_FILE` | the file the tool call names — its `path` argument — as an absolute path with `.`/`..` resolved. Unset for tools that take no path (`bash`, `apply_patch`, …) |

## How a hook answers

| Exit status | stdout | Meaning |
|-------------|--------|---------|
| 0 | empty | carry on |
| 0 | a JSON object | a decision — see the fields below |
| 0 | other text | context, for the events that take context; ignored by the others |
| 2 | — | **block**, with stderr as the reason (what "block" means per event is in the table below) |
| anything else | — | the hook failed: one notice line for you, and the run continues as if the hook weren't there |

A hook that times out, or exits 0 with stdout that starts with `{` but isn't
valid JSON, or names an unknown `decision`, also counts as failed. A failed
hook never blocks anything: to refuse, exit 2 or say `deny`.

The JSON object may have:

| Field | Meaning |
|-------|---------|
| `decision` | `"deny"` (or `"block"`, the same thing), `"allow"`, or `"continue"` |
| `reason` | why — shown to the model for `deny` and `continue` |
| `context` | text to put in front of the model |
| `arguments` | `pre_tool` only: an object replacing the call's arguments |

An `allow` from a hook spares you the approval prompt; it never overrides a
`deny` [permission rule](permissions.md).

Fields an event doesn't use are ignored. Context reaches the model wrapped in
`<hook-context>…</hook-context>`, so it isn't mistaken for your words or the
tool's own output; it is cut at 16 KiB.

When several hooks answer: contexts are all kept; the first `deny` (or
`continue`, for `turn_end`) ends the chain; a later `deny` overrides an
earlier `allow`; a rewrite of the arguments is what the next hook sees.

## Events

| Event | When | Extra stdin fields | What the hook may do |
|-------|------|--------------------|----------------------|
| `session_start` | atelier has started, before the first prompt | — | add **context** (stdout text or `context`), sent with the first prompt |
| `session_end` | atelier is exiting | — | watch |
| `user_prompt` | you submitted a prompt (headless: each prompt) | `prompt` | **block** it (`deny` or exit 2: the prompt is not sent, the reason is shown as an error); add **context** for this turn |
| `pre_tool` | the model asked for a tool call, before the approval prompt | `tool`, `arguments` | **deny** it (the model gets a tool error with the reason); **`allow`** it, skipping the approval prompt; rewrite its **`arguments`** |
| `post_tool` | a tool call finished | `tool`, `arguments`, `ok`, `output` (cut at 16 KiB) | add **context**, appended to the result the model sees. The call already happened, so exit 2 or `deny` can't undo it: the reason is appended the same way |
| `permission_request` | atelier is about to ask you to approve a call | `tool`, `arguments` | answer for you: **`allow`** or **`deny`** (or exit 2). No decision: you are asked as usual |
| `pre_compact` | older history is about to be summarized | `messages` (how many) | watch |
| `post_compact` | the summary replaced them | `messages` | watch |
| `turn_end` | the model gave its final answer for a prompt | `answer` (cut at 16 KiB), `continues` | **`continue`** (or exit 2): the agent is sent back to work, with the reason as a new user message marked `[turn_end hook]` |
| `subagent_start`, `subagent_stop` | reserved | — | accepted in the file, never fired yet. A [subagent's](agents.md) own tool calls do fire `pre_tool`, `permission_request` and `post_tool`, like the agent's; the `task` call that starts it fires none of them, and its conversation gets no `user_prompt` or `turn_end` |

Notes on the details:

- **A [command file](commands.md)'s `` !`command` `` lines fire `pre_tool`**
  too, as `bash`, before they run (deny or allow; no `post_tool`).
- **`pre_tool` runs before the approval prompt**, on what the model sent. If
  it rewrites the arguments, the rewritten ones are what you are asked to
  approve and what runs. `allow` skips the whole approval step for that one
  call.
- **`permission_request` only fires when atelier would have asked.** With
  approvals off (`ATELIER_APPROVE=all`, which `--print` implies) or for a
  tool you always allow, there is no request and no hook.
- **`turn_end` can loop**, so it is capped: after 3 continues in a row for
  one prompt, a fourth is refused with a notice and the turn ends. `continues`
  tells the hook how many it has used, so it can give up by itself first. It
  does not fire for a turn you stopped (Ctrl-C), or one that ended in an
  error or at `--max-turns`. In atelier's own loop, model requests made
  after a continue count toward the same `--max-turns`.
- **`session_end`** runs when atelier exits normally (`/quit`, Ctrl-D, the
  end of a `--print` run), not if the process is killed.
- A hook runs synchronously: the agent waits for it. Keep hooks fast, or
  start slow work in the background (`… &`) — atelier doesn't wait for
  background processes that outlive the hook.

### With the Claude backend

With [`--claude`](claude.md) the Claude Code CLI runs the loop, and atelier
sees only part of it:

| Event | Claude backend |
|-------|----------------|
| `session_start`, `session_end`, `user_prompt`, `turn_end` | fire as above (`turn_end` gets the text of claude's final result; a `continue` is sent to claude as the next user message) |
| `pre_tool`, `post_tool`, `permission_request` | fire for **atelier's tools** called by claude (the default setup, where they replace claude's own). Not for claude's built-in tools (`ATELIER_CLAUDE_BUILTINS=keep`), subagents' internals, or the MCP servers claude connects itself — use Claude Code's own hooks for those |
| `pre_compact`, `post_compact` | never: claude compacts its own context |

## Examples

### Format after an edit

```toml
[[hooks]]
event = "post_tool"
match = "edit|write|multiedit"
timeout_secs = 30
command = '''
case "$ATELIER_FILE" in *.rs) ;; *) exit 0 ;; esac
before=$(cksum < "$ATELIER_FILE")
out=$(rustfmt --edition 2024 "$ATELIER_FILE" 2>&1) || { echo "rustfmt failed: $out"; exit 0; }
[ "$before" = "$(cksum < "$ATELIER_FILE")" ] ||
  echo "rustfmt reformatted this file; read it again before editing it further"
'''
```

Silent when there was nothing to do. When the formatter changed the file the
model is told — atelier's edit tools refuse to edit a file that changed
since the model last read it, so it needs to know to re-read. A formatter
error is reported to the model as plain-text context rather than failing the
hook, because the model is the one who can fix the syntax error.

### Block edits to a protected path

```toml
[[hooks]]
event = "pre_tool"
match = "edit|write|multiedit|notebook_edit"
command = '''
case "$ATELIER_FILE" in
  "$ATELIER_PROJECT"/migrations/*|*/.env|*/.env.*)
    echo "$ATELIER_FILE is protected: applied migrations are never edited, and .env is the user's" >&2
    exit 2 ;;
esac
'''
```

Exit 2 denies the call; the model receives
`error: blocked by a pre_tool hook: …/migrations/001.sql is protected: …` as
the tool's result and can change course. This guards the file tools, which
name their target in `path`. `bash` and `apply_patch` can write files too and
set no `ATELIER_FILE`: to cover them, match them as well and inspect
`arguments` on stdin (`jq -r .arguments.command`, `.arguments.patch`) — a
text match on a shell command is a speed bump for an honest model, not a
sandbox.

### Notify when a turn ends

Put this one in your own `~/.config/atelier/atelier.toml`, so it applies
everywhere and needs no approval:

```toml
[[hooks]]
event = "turn_end"
command = "notify-send atelier \"finished in $(basename \"$ATELIER_PROJECT\")\" >/dev/null 2>&1 || true"
```

(On macOS: `osascript -e 'display notification "finished" with title "atelier"'`.)
It prints nothing and exits 0, so it only watches.

### Don't stop while the tests fail

```toml
[[hooks]]
event = "turn_end"
timeout_secs = 300
command = '''
[ "$(jq -r .continues)" -ge 2 ] && exit 0      # give up after two rounds
out=$(cargo test --quiet 2>&1) && exit 0
printf '%s' "$out" | tail -n 40 | jq -Rs '{decision: "continue", reason: ("the tests fail:\n" + .)}'
'''
```

The JSON is built with `jq` so the test output is escaped properly; a
hand-assembled string with a stray quote would be invalid JSON, which is a
failed hook, not a `continue`.
