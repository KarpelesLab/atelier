# The embedding protocol: `atelier serve`

atelier's own interface is deliberately small: an input line, a status
strip, scrollback. Anything richer — an editor integration, a web page,
another program driving an agent — lives *outside* atelier and talks to it
through this protocol.

```sh
atelier serve [--claude] [--continue] [--mode MODE] [--approval-timeout SECS]
```

`atelier serve` runs one session in the current directory and speaks
**JSON-RPC 2.0 over stdio**: the client writes requests on atelier's stdin
and reads responses, notifications and atelier's own requests from its
stdout. It is [`--print --output-format stream-json`](headless.md) grown the
other half of the conversation: the same events, plus what a one-way stream
cannot do — answer approval prompts, steer or interrupt a running turn, run
slash commands.

| Flag | Meaning |
|------|---------|
| `--claude` | Use the [Claude backend](claude.md). The protocol is the same on every backend |
| `-c`, `--continue` | Resume this directory's saved session first (see [Sessions](sessions.md)) |
| `--mode MODE` | The [permission mode](permissions.md) to start in: `plan`, `ask-edits`, `auto-edit` (default), `yolo` |
| `--approval-timeout SECS` | How long an [`approval/request`](#approvalrequest) or a [`question/request`](#questionrequest) waits for the client's answer. Without it they wait for as long as it takes |

Everything else — the model, the endpoint, `atelier.toml`, instruction
files, hooks, MCP servers — is configured as for any other run (see
[Configuration](configuration.md)).

## Transport

- **One message per line**, UTF-8, each a JSON object with `"jsonrpc":
  "2.0"`. No `Content-Length` framing, no batches (a JSON array is an
  invalid request).
- **stdout carries protocol messages and nothing else**, flushed after each.
  Logs and warnings go to stderr; a client may show or drop them.
- Messages flow both ways. The client sends *requests* (answered once each,
  with `result` or `error`) and may send *notifications* (a request without
  an `id`: carried out, never answered). atelier sends responses,
  *notifications* (the events of a turn), and two *requests* of its own that
  the client must answer. Ids of the two directions are independent:
  atelier's request ids are numbers it chooses; the client's may be numbers
  or strings.
- Requests are handled in the order they arrive. Those that need the
  session idle (`turn/start`, `command/run`, `shutdown`) are refused while
  it is busy rather than queued; the others are answered at once, even in
  the middle of a turn.
- **End of input is a shutdown.** When stdin closes, atelier stops what is
  running and exits (status 0).

## Lifecycle

```
client                                   atelier
  │ initialize ─────────────────────────────▶│
  │◀──────────────────────────────── result  │
  │ turn/start {text} ──────────────────────▶│
  │◀──────────────────────── result {turn:1} │
  │◀───────── event/text, event/tool_start…  │   the turn runs
  │◀──────────────────── approval/request    │   …and may ask
  │ result {decision} ──────────────────────▶│
  │ turn/steer, turn/interrupt ─────────────▶│   any time while it runs
  │◀──────────────────── turn/ended {turn:1} │
  │ command/run, session/status ────────────▶│   between turns
  │ shutdown ───────────────────────────────▶│
  │◀──────────────────────────────── result  │   then atelier exits
```

## Methods (client → atelier)

### `initialize`

Must be the first request; anything else before it fails with `-32002`.
Requests written right behind it are held until it is done, so a client
need not wait for the result before sending more.

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"client":{"name":"my-editor","version":"1.2"},"protocol":1}}
```

| Param | | |
|-------|-|-|
| `client` | optional | `{name, version}`: who is connecting (logged to stderr) |
| `protocol` | optional | The protocol version the client was written for |

Result:

```json
{"protocol":1,
 "atelier":{"version":"0.1.1"},
 "session":{"cwd":"/home/me/project","model":"qwen3-coder","backend":"openai","mode":"auto-edit",
            "tools":["apply_patch","ask","bash","…"]},
 "capabilities":{"steer":true,"interrupt":true,"approvals":true,"questions":true,
                 "images":true,"commands":true,"approval_timeout":null}}
```

- `protocol` is the version atelier speaks — `1`. It answers with its own
  whatever the client asked for; a client that needs another should
  disconnect. Within a version, fields and methods are only ever added:
  ignore what you don't know.
- `session.backend` is `openai` (any OpenAI-compatible endpoint), `chatgpt`
  (signed in with ChatGPT) or `claude`. `session.tools` are the tools
  advertised to the model, configured MCP servers' included: they are
  connected during `initialize`, and what connecting had to report follows
  as [`event/idle`](#eventidle) notifications.
- `capabilities.approval_timeout` is `--approval-timeout` in seconds, or
  `null`.

### `turn/start`

Send a prompt. Returns at once with the turn's id; the turn runs in the
background, reports through [notifications](#notifications-atelier--client),
and closes with [`turn/ended`](#turnended).

| Param | | |
|-------|-|-|
| `text` | required | The prompt |
| `images` | optional | Images to attach: file paths or `data:` URLs |
| `max_turns` | optional | Stop the turn after this many model requests (status `max_turns`). With the Claude backend the cap is claude's own and applies from when claude is launched |

Result: `{"turn": 1}`. Turn ids count up from 1.

One turn at a time: while one runs (or a command does), `turn/start` fails
with `-32001` — use [`turn/steer`](#turnsteer) to add to the running turn.

### `turn/steer`

Queue a message into the running turn, the way typing a line mid-turn does
in the terminal interface. The model gets it at its next step — after the
tool calls in progress, before its next request — and
[`event/delivered`](#notifications-atelier--client) says when.

Params: `{"text": "…"}`. Result: `{"turn": 1, "queued": 1}` (`queued`:
messages now waiting). Fails with `-32003` when no turn is running.

A message the turn never took (it ended first) comes back in
`turn/ended`'s `undelivered`, for the client to send as a new turn or drop.

### `turn/interrupt`

Stop the running turn: the equivalent of Ctrl-C. Result: `{"turn": 1}` —
the stop was *requested*; the turn has ended when `turn/ended` arrives, with
status `interrupted`. Fails with `-32003` when no turn is running.

- The turn stops at its next opportunity: the next streamed chunk, or
  between tool calls. A tool that is already running finishes, and a model
  request still waiting for its first byte is not cut short.
- A pending `approval/request` or `question/request` is cancelled (deny /
  declined) and the client is told with
  [`request/cancelled`](#requestcancelled).
- Background work is not killed: `bash` jobs started with `background:
  true` keep running, and so do claude's background agents.
- What the model had said so far stays in the conversation, marked as
  stopped by the user.

### `command/run`

Run a slash command (`/help` lists them) — `/model x`, `/compact`,
`/mode plan`, `/mcp add …`, a [custom command](commands.md) — between turns.
Its output arrives as `event/info` notifications (without a `turn`), then
the result:

Params: `{"line": "/mode plan"}`.

| `kind` | `handled` | Meaning |
|--------|-----------|---------|
| `handled` | `true` | The command ran; nothing more to do |
| `prompt` | `false` | The line is not a command. `text` is the line: send it with `turn/start` if that is what was meant |
| `expanded` | `false` | A custom command expanded into a prompt. `text` is the expanded prompt; atelier does not start the turn itself — the client decides. `model` is the model the command file asks for (`model:` in its front matter), or `null`: `turn/start` takes no model, so a client that honours it switches with `/model` before the turn and back after |
| `quit` | `false` | `/quit`: leaving is the client's call (`shutdown`) |
| `config` | `false` | `/config`: the settings screen belongs to the terminal interface; a client presents its own |

Fails with `-32001` while a turn or another command runs. A command may
itself ask for approval (a custom command's `` !`shell` `` lines do).

### `session/status`

Answered at once, also in the middle of a turn.

```json
{"model":"qwen3-coder","backend":"openai","mode":"plan","cwd":"/home/me/project",
 "usage":{"context_tokens":5210,"output_tokens":1873},"context_limit":8000,
 "busy":true,"turn":2,"queued":0,"background":0}
```

| Field | Meaning |
|-------|---------|
| `model`, `backend`, `mode`, `cwd` | As in `initialize`, kept current (`/model`, `/mode`) |
| `usage.context_tokens` | Context size of the latest model request |
| `usage.output_tokens` | Output tokens generated this session |
| `context_limit` | The context size past which the conversation is compacted |
| `busy` | A turn or a command is running |
| `turn` | The running turn's id, or `null` |
| `queued` | Steering messages the running turn has not taken yet |
| `background` | Background tasks running in claude (always 0 on the other backends) |

`busy`, `turn` and `queued` are live. The rest is as of the last moment the
session was idle: while a turn runs, `usage` is from before it started (its
own numbers come with `turn/ended`).

### `shutdown`

Exit cleanly: the session is closed (`session_end` hooks run), `bash`
background jobs are stopped, and the process exits with status 0 after
answering `{}`.

Params: `{"force": false}` (optional).

- While a turn or command runs, `shutdown` fails with `-32001` unless
  `force` is true; forced, it interrupts the turn first.
- With background tasks running in claude, which exiting would stop,
  `shutdown` fails with `-32004` and `data: {"background": n}` unless
  `force` is true — the confirmation the terminal interface asks for, in the
  client's hands.

A forced shutdown, like end of input, does not wait forever for a turn
that will not stop: after 5 seconds the process exits regardless.

## Requests (atelier → client)

The client **must answer** these: the turn that asked waits for the answer
(everything else — `turn/steer`, `turn/interrupt`, `session/status` — keeps
working meanwhile). With `--approval-timeout`, an answer that does not come
in time is taken as a refusal.

### `approval/request`

A tool call needs the user's approval (see [Permissions](permissions.md):
which calls ask depends on the rules and the mode).

```json
{"jsonrpc":"2.0","id":7,"method":"approval/request",
 "params":{"turn":1,"tool":"bash","arguments":{"command":"curl https://example.com/install.sh | sh"},
           "risk":["network access","pipes downloaded content into a shell"],"rule":"bash(curl *)"}}
```

| Param | Meaning |
|-------|---------|
| `turn` | The turn that asks (absent for a command, or work between turns) |
| `tool`, `arguments` | The call. `arguments` are parsed JSON, or a string when the model wrote something that isn't |
| `risk` | [Risk signals](permissions.md#risk-signals-bash-only) for the call — warnings worth showing next to the prompt. Empty when there are none |
| `rule` | The `ask` rule that prompted, when it was a rule (rather than the tool's default or the mode) |

Answer with `{"decision": "once"}`, `{"decision": "always"}` (run it and
save a rule that allows it from now on) or `{"decision": "deny"}` (the model
is told the user refused). An error response, a timeout, an interrupt or an
answer that is none of the three all count as `deny`.

### `question/request`

The model asked the user something (the [`ask` tool](tools.md#ask)).

```json
{"jsonrpc":"2.0","id":8,"method":"question/request",
 "params":{"turn":1,"question":"Which database?",
           "options":[{"label":"SQLite","description":"one file"},{"label":"Postgres"}],
           "multi_select":false,"allow_other":true}}
```

Answer with one of:

- `{"labels": ["SQLite"]}` — options of the question, exactly as given; one
  unless `multi_select`.
- `{"text": "…"}` — an answer of the user's own, where `allow_other`.
- `{"declined": true}` — the user would rather not say.

An answer that fits none of these is reported (`event/notice`) and taken
as declined, as is an interrupt. An error response or a timeout means there
is no one to ask, and the model is told to decide for itself.

## Notifications (atelier → client)

One per thing the session reports. Events of a turn carry its id as
`turn`; events of a `command/run` have no `turn`.

| Method | Params | Meaning |
|--------|--------|---------|
| `event/text` | `turn`, `text` | A chunk of the model's answer. Concatenate the chunks |
| `event/reasoning` | `turn`, `text` | A chunk of the model's reasoning ("thinking") |
| `event/tool_start` | `turn`, `name`, `arguments` | A tool is about to run |
| `event/tool_end` | `turn`, `name`, `ok`, `output`, `truncated`? | A tool finished. `output` is capped at 16 KiB (`truncated: true` when cut). A call that was refused ends without having started |
| `event/info` | `turn`?, `text` | Informational output: a command's, or the harness's during a turn |
| `event/notice` | `turn`?, `text` | An out-of-band notice: a warning, something that went wrong |
| `event/activity` | `turn`, `text` | A line of progress from a subagent |
| `event/review` | `turn`, `text` | A note from the [parallel reviewer](review.md) |
| `event/delivered` | `turn`, `text` | A `turn/steer` message just reached the model |
| `event/idle` | `kind`, … | Output between turns; see below |
| `turn/ended` | see below | The turn is over |
| `request/cancelled` | `id`, `reason` | A request atelier made no longer needs an answer |

The payloads are those of the [`stream-json` events](headless.md#events-stream-json)
of the same name (`text`, `tool_start`, `tool_end`, …): the two interfaces
share one vocabulary.

### `turn/ended`

```json
{"jsonrpc":"2.0","method":"turn/ended",
 "params":{"turn":1,"status":"completed","steps":3,"duration_ms":8412,
           "usage":{"context_tokens":5210,"output_tokens":642}}}
```

| Field | Meaning |
|-------|---------|
| `status` | `completed`, `interrupted` (`turn/interrupt`), `max_turns` (the `max_turns` cap ran out), or `error` |
| `error` | What went wrong, for `error` and `max_turns` |
| `steps` | Model requests the turn took |
| `duration_ms` | Wall time |
| `usage.context_tokens` | Context size of the last request |
| `usage.output_tokens` | Output tokens this turn generated |
| `undelivered` | `turn/steer` messages the turn never took, if any |

By the time the client reads `turn/ended` the session is idle: the next
`turn/start` can follow immediately. A turn that ends in `error` or
`max_turns` leaves the conversation intact; another turn can continue it.

### `event/idle`

What the session produces on its own while no turn runs — a message from
another atelier instance (see [Coordinator](coordinator.md)), a background
`bash` job that ended, claude reporting on background work, a newer release
being available. `kind` is the event it would have been in a turn (`info`,
`notice`, `text`, `tool_start`, …) and the other params are that event's:

```json
{"jsonrpc":"2.0","method":"event/idle","params":{"kind":"info","text":"⚙ job 3 finished (exit 0, 42s): cargo test"}}
```

None of it starts a turn; the model hears of it with the next prompt.

### `request/cancelled`

`{"id": 7, "reason": "timeout"}` or `"interrupted"`: the request with that
id (an `approval/request` or `question/request`) was given up on, and was
taken as a refusal. Take the prompt down; an answer sent anyway is ignored.

## Errors

Failures are JSON-RPC error objects, `{"code", "message", "data"?}`. The
codes are stable:

| Code | Name | When |
|------|------|------|
| `-32700` | parse error | A line that is not JSON. Answered with `"id": null`; the session carries on |
| `-32600` | invalid request | JSON, but not a request, notification or answer (an array, a missing or non-string `method`). `id` is the message's, or `null` |
| `-32601` | method not found | Unknown method |
| `-32602` | invalid params | A required param is missing or of the wrong type |
| `-32001` | turn running | `turn/start`, `command/run` or unforced `shutdown` while a turn or command runs. `data: {"turn": n}` for a turn |
| `-32002` | not initialized | A request other than `initialize` came first |
| `-32003` | no turn running | `turn/steer` or `turn/interrupt` with nothing to act on |
| `-32004` | background running | Unforced `shutdown` with background tasks running. `data: {"background": n}` |
| `-32005` | already initialized | A second `initialize` |

A turn that fails is not a JSON-RPC error — `turn/start` had already
succeeded: it ends with `turn/ended` and `status: "error"`.

## A session, on the wire

`→` is the client writing, `←` is atelier. Auto-approve is off, so the shell
command asks.

```
→ {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"client":{"name":"demo","version":"0.1"}}}
← {"id":1,"jsonrpc":"2.0","result":{"atelier":{"version":"0.1.1"},"capabilities":{"approval_timeout":null,"approvals":true,"commands":true,"images":true,"interrupt":true,"questions":true,"steer":true},"protocol":1,"session":{"backend":"openai","cwd":"/home/me/project","mode":"auto-edit","model":"qwen3-coder","tools":["apply_patch","ask","bash","…","write"]}}}
→ {"jsonrpc":"2.0","id":2,"method":"turn/start","params":{"text":"run the tests and fix what fails"}}
← {"id":2,"jsonrpc":"2.0","result":{"turn":1}}
← {"jsonrpc":"2.0","method":"event/reasoning","params":{"text":"I should run the suite first.","turn":1}}
← {"jsonrpc":"2.0","id":1,"method":"approval/request","params":{"arguments":{"command":"cargo test"},"risk":[],"tool":"bash","turn":1}}
→ {"jsonrpc":"2.0","id":3,"method":"turn/steer","params":{"text":"only the parser tests matter"}}
← {"id":3,"jsonrpc":"2.0","result":{"queued":1,"turn":1}}
→ {"jsonrpc":"2.0","id":1,"result":{"decision":"once"}}
← {"jsonrpc":"2.0","method":"event/tool_start","params":{"arguments":{"command":"cargo test"},"name":"bash","turn":1}}
← {"jsonrpc":"2.0","method":"event/tool_end","params":{"name":"bash","ok":true,"output":"test parser::nested ... FAILED\n…","turn":1}}
← {"jsonrpc":"2.0","method":"event/delivered","params":{"text":"only the parser tests matter","turn":1}}
← {"jsonrpc":"2.0","method":"event/tool_start","params":{"arguments":{"new_string":"depth >= 8","old_string":"depth > 8","path":"src/parser.rs"},"name":"edit","turn":1}}
← {"jsonrpc":"2.0","method":"event/tool_end","params":{"name":"edit","ok":true,"output":"replaced 1 occurrence(s) in src/parser.rs","turn":1}}
← {"jsonrpc":"2.0","method":"event/text","params":{"text":"Fixed the off-by-one in ","turn":1}}
← {"jsonrpc":"2.0","method":"event/text","params":{"text":"`parser::nested`.","turn":1}}
← {"jsonrpc":"2.0","method":"turn/ended","params":{"duration_ms":8412,"status":"completed","steps":3,"turn":1,"usage":{"context_tokens":5210,"output_tokens":642}}}
→ {"jsonrpc":"2.0","id":4,"method":"command/run","params":{"line":"/mode plan"}}
← {"jsonrpc":"2.0","method":"event/info","params":{"text":"mode: plan — read-only: the agent investigates and proposes; changes are refused"}}
← {"id":4,"jsonrpc":"2.0","result":{"handled":true,"kind":"handled"}}
→ {"jsonrpc":"2.0","id":5,"method":"shutdown"}
← {"id":5,"jsonrpc":"2.0","result":{}}
```

Things a client should not rely on: the order of keys in a message, and the
order between a *response* and the *notifications* of the running turn (the
answer to `turn/steer` and the `event/delivered` it leads to come from two
different threads). The events of a turn are in order among themselves, a
turn's first event follows the `turn/start` result, and `turn/ended` is its
last.

## Building on it

**A minimal client** is a loop over stdout lines: a message with `method`
and `id` is a request to answer; with `method` only, a notification; with
`result` or `error`, the response to the request of that `id`.

**ACP (Agent Client Protocol).** Editors that speak ACP (Zed, the JetBrains
IDEs, Neovim plugins) do not need an adapter in front of this protocol:
[`atelier acp`](acp.md) is an ACP agent itself, a second front end over the
same session and the same wire (JSON-RPC lines, a reader thread and a
session thread). The two line up closely — `session/prompt` is `turn/start`
with the response held until `turn/ended` (`completed` → `end_turn`,
`interrupted` → `cancelled`, `max_turns` → `max_turn_requests`), the
`session/update` notifications are the `event/*` stream in ACP's vocabulary,
`session/request_permission` is `approval/request` (`allow_once` /
`allow_always` / `reject_once` being `once` / `always` / `deny`),
`session/cancel` is `turn/interrupt`, and ACP's session modes are the
permission modes. Where they differ, `serve` is the smaller and the more
atelier's own: it has mid-turn steering (`turn/steer`), free-form questions
(`question/request`) and `event/idle`, and no notion of an editor's files.
Write a front end for atelier against `serve`; point an existing ACP client
at `acp`.

**Another transport.** Nothing in the protocol depends on stdio beyond
"lines in, lines out": the messages carry no transport state, and all of it
is produced through one writer and consumed through one reader. A socket
transport — a Unix socket of its own, or the
[coordinator](coordinator.md)'s, which already carries line-delimited
JSON-RPC between instances — is that pair of ends pointed elsewhere, plus
what stdio gets for free: authenticating the peer, and deciding what a
disconnect means for a session that could outlive it.
