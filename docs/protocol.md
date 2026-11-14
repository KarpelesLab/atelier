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
  session idle (`turn/start`, `command/run`, `settings/get`,
  `settings/set`, `shutdown`) are refused while
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
  │ session/history ────────────────────────▶│
  │ settings/get, settings/set ─────────────▶│
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
                 "images":true,"commands":true,"history":true,"file_changes":true,
                 "approval_timeout":null}}
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
  `null`. `history` and `file_changes` say this build has
  [`session/history`](#sessionhistory) and sends
  [`event/file_change`](#eventfile_change).

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
| `config` | `false` | `/config`: the settings screen belongs to the terminal interface; a client presents its own, with [`settings/get`](#settingsget) and [`settings/set`](#settingsset) |

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

### `session/history`

The conversation so far, for a client that did not see it happen: one that
resumed a session ([`atelier serve --continue`](sessions.md)), or reconnected
to its own. Oldest first, as items ready to show.

Params (both optional):

| Param | | |
|-------|-|-|
| `limit` | default 200, at most 1000 | How many items to return |
| `before` | default: the end | Return the items before this `index` |

A page is the *last* `limit` items before `before`: ask without `before`
for the end of the conversation, then with `before` set to the result's
`first` for the page above it, while `more` is true.

```json
{"items":[
   {"index":0,"type":"summary","text":"## Goal\nMake the parser tests pass…"},
   {"index":1,"type":"user","text":"now fix the off-by-one","images":0},
   {"index":2,"type":"assistant","text":"Looking at the parser."},
   {"index":3,"type":"tool","id":"call_1","name":"read","arguments":{"path":"src/parser.rs"},
    "ok":true,"output":"…"},
   {"index":4,"type":"tool","id":"call_2","name":"edit",
    "arguments":{"path":"src/parser.rs","old_string":"depth > 8","new_string":"depth >= 8"},
    "ok":true,"output":"replaced 1 occurrence(s) in src/parser.rs"},
   {"index":5,"type":"assistant","text":"Fixed the off-by-one in `parser::nested`."}],
 "first":0,"total":6,"more":false,"stored":true}
```

| Field | Meaning |
|-------|---------|
| `items` | The page, oldest first; each item has its `index` in the whole list |
| `first` | The `index` of the first item returned (pass it as `before` for the page above) |
| `total` | Items in the whole conversation |
| `more` | There are items before `first` |
| `stored` | Whether atelier keeps this session's conversation: `false` with the Claude backend (below) |
| `claude_session` | Claude backend: the id of the Claude Code session, once there is one |

The items:

| `type` | Fields | |
|--------|--------|-|
| `summary` | `text` | What [compaction](sessions.md#compaction) made of the turns it replaced. Always the first item when there is one: the items after it are all that is kept word for word |
| `user` | `text`, `images` | A prompt (or a `turn/steer` message, where it was delivered). `images` is how many images were attached: the count, never the images |
| `assistant` | `text` | The model's text |
| `tool` | `id`, `name`, `arguments`, `ok`?, `output`?, `truncated`?, `pruned`? | A tool call together with its result, as [`event/tool_end`](#notifications-atelier--client) reported it: `output` capped at 16 KiB, `truncated: true` when cut. `pruned: true` when compaction replaced an old result's body with a one-line stub (then `output` is that line). A call without a result yet has neither `ok` nor `output` |
| `tool_images` | `images`, `pruned`? | Images the tool calls above returned (`read` on a picture): how many |

What is in it, and what is not:

- It is the conversation **as the model has it**, not a recording of the
  events: text arrives whole rather than in chunks, and there are no
  `info`, `notice`, `activity`, `review` or `file_change` items.
- **Left out**: the system prompt (with the instruction files and the memory
  index it carries), and the project context atelier attaches to each prompt
  (git status, diagnostics, the plan-mode note). **Reasoning** is streamed
  and not kept, so there is none.
- **Kept, inside its own markers**: what the harness added to a message's
  text — a hook's `<hook-context>…</hook-context>`, a `<peer-message>` from
  another instance, the notice of a background job that ended, a
  `[turn_end hook] …` prompt, a subdirectory's `<nested-instructions>` at
  the end of a tool result. A `user` item's `text` is therefore what the
  model was sent as the user's message, which can be more than was typed.
- `ok` is not stored with a result; it is read back from it (`false` when
  the result starts with `error:`, as every refusal and failure atelier
  writes does).
- `index` counts from the oldest item the session still has. It is stable
  while the session is idle; a compaction shifts it (the oldest items
  become one `summary`).

It needs the session idle: while a turn or a command runs it fails with
`-32001` — the running turn is being reported through its events, and the
history is complete again at `turn/ended`.

**Backends.** With an OpenAI-compatible endpoint or a ChatGPT sign-in the
conversation is atelier's and this is all of it. With the
[Claude backend](claude.md) the conversation is kept by Claude Code, in its
own store: atelier has no messages to return, so `items` is empty,
`stored` is `false`, and `claude_session` names the Claude Code session
(the one `--continue` resumes) for a client that wants to read that
transcript itself.

### `settings/get`

The settings as a client needs them to show a settings view of its own:
every setting with its value in force and **where that value comes from**,
and enough about each to draw a form without knowing atelier's schema. No
params. Like a command, it needs the session idle: `-32001` while a turn or
a command runs.

```json
{"files":{"user":{"path":"/home/me/.config/atelier/atelier.toml","exists":true},
          "project":{"path":"/home/me/project/atelier.toml","exists":true}},
 "trust":{"state":"untrusted","active":false},
 "settings":[
   {"key":"defaults.context_limit","type":"integer","label":"context limit",
    "description":"Tokens in a request past which older history is compacted into a summary",
    "settable":true,"scopes":["project"],"env":"ATELIER_CONTEXT_LIMIT",
    "value":4000,"source":"project",
    "layers":[{"source":"project","value":4000},{"source":"default","value":8000}]},
   {"key":"defaults.mode","type":"string","values":["plan","ask-edits","auto-edit","yolo"],
    "description":"The permission mode sessions start in; `yolo` is honoured in a trusted project only",
    "settable":false,"scopes":[],"env":"ATELIER_MODE",
    "value":"auto-edit","source":"default",
    "layers":[{"source":"project","value":"yolo",
               "ignored":"the project is not trusted (untrusted): accept its settings with /trust"},
              {"source":"default","value":"auto-edit"}]},
   "…"],
 "keys":[{"name":"ATELIER_API_KEY","description":"the bearer token of the `env` provider","set":true,"source":"env"},
         {"name":"ATELIER_SEARCH_KEY","description":"the `web_search` provider's API key","set":false,"source":null}]}
```

| Field | Meaning |
|-------|---------|
| `files.user`, `files.project` | The two `atelier.toml` files: `path` (`null` for the user's when there is no home directory), whether each `exists`, and `error` when it cannot be read as settings — then nothing of it is in force |
| `trust` | Where the project stands (see [Project trust](trust.md)): `state` is `not-needed`, `trusted`, `trusted-by-environment`, `untrusted` or `changed`; `active` says whether its gated settings count |
| `settings` | One entry per setting, see below |
| `keys` | The keys the environment holds: whether each is `set`, never its value |

An entry of `settings`:

| Field | Meaning |
|-------|---------|
| `key` | The path in `atelier.toml`: `table.key`, or a table's name (`mcp`, `provider`, …) |
| `type` | `boolean`, `integer`, `string`, `list` (strings), `tables` (an array of tables), `table` |
| `values` | The allowed values, for an enumeration |
| `description` | One line on what it is |
| `label` | The setting's name on the terminal interface's `/config` screen, for the ones it shows |
| `settable`, `scopes` | Whether [`settings/set`](#settingsset) can change it, and in which files: `user`, `project` |
| `env` | The environment variable that overrides it, if there is one |
| `value` | The value in force; `null` when nothing sets it and there is no default worth stating |
| `source` | Where `value` comes from: `default`, `user` (the user-global file), `project`, `env`, `flag` (the command line) or `session` (changed since the session started: `/model`, `/mode`, `/review`, …) |
| `combined` | `true` when the files' entries apply together (`permissions.allow`, `hooks`, `provider`, …): `value` is the project's entries followed by the user's, and `source` the first file that has any |
| `layers` | Everything that says something about the setting, the strongest first: `source`, `value`, `name` (the variable or the flag), `key` (when the value sits under another key: `models.main` decides `defaults.model`), and `ignored` — the reason a value that is there does not count |

A value is `ignored` when the project is not trusted and the setting is
[gated](trust.md#what-is-gated-and-why) — an untrusted project's `yolo`
default, its MCP servers, its allow rules — when it sits in a file the
setting is not read from (`[local] model` or `[sandbox] root` in a project's
file; `context_limit` in the user's), or when the variable holds something
it does not understand.

**No secret is returned.** `atelier.toml` names keys rather than holding
them (`api_key_env`, `key_env`: reported as written). Where one can sit
anyway it is left out: the headers of `[[mcp_http]]` and `[[provider]]` are
reduced to their names, `api_key_command` to `true`. `ATELIER_API_KEY` and
`ATELIER_SEARCH_KEY` appear under `keys` as set or not. Commands (`[[mcp]]`,
`[[hooks]]`, `[[diagnostics]]`, `[[lsp]]`) are reported as written, as
`/trust show` prints them.

Where the sources are approximate:

- The files are read **as they are on disk now**; the session read them when
  it started. A file edited by hand since is reported with its new values,
  which take effect at the next start.
- `session` says the running session differs from its configuration, not
  which command did it; it is worked out for the model, the mode,
  auto-approve, the context limit and review mode. `flag` is `--mode` only.
  With the [Claude backend](claude.md), claude picks its own model: the
  model settings are reported as configured.
- In an untrusted project, the allow rules saved with "always" during the
  session count for it, though `permissions.allow` shows the project's rules
  as ignored.
- A `combined` value is the plain concatenation: an entry of the user's file
  that the project replaces by name (`provider`) is still listed.
- Checkers and language servers detected from the project's files are not
  settings, and are not listed.

### `settings/set`

Change one setting in an `atelier.toml`, and put it into effect.

```json
{"jsonrpc":"2.0","id":9,"method":"settings/set","params":{"key":"defaults.model","value":"qwen3-coder:30b"}}
```

| Param | | |
|-------|-|-|
| `key` | required | The setting's key path, as `settings/get` lists it |
| `value` | required | The new value, of the setting's `type` (and among its `values`); `null` takes the key out of the file |
| `scope` | optional | `user` (the default): the user-global file. `project`: the project's `atelier.toml`. A setting takes only the scopes `settings/get` lists for it |

Result:

```json
{"key":"defaults.model","scope":"user","value":"qwen3-coder:30b",
 "file":"/home/me/.config/atelier/atelier.toml","applies":"now",
 "setting":{"key":"defaults.model","value":"qwen3-coder:30b","source":"user","…":"…"}}
```

| Field | Meaning |
|-------|---------|
| `file` | The file that was written |
| `applies` | `now`: the running session has the value. `next_start`: it is written, and the session keeps what it has — after `null`, or when a command changed the setting during the session. `overridden`: it is written, and something stronger decides, now and at the next start |
| `by` | For `overridden`, the layer that decides (as in `settings/get`) |
| `setting` | The setting's entry as `settings/get` now reports it |
| `note` | A remark, when there is one (the Claude backend picks its own model) |

What can be set is what the terminal interface's `/config` screen edits,
written the way it writes:

| Key | Scopes | |
|-----|--------|-|
| `defaults.model` | `user`, `project` | The main model. Written to the project's file it is in effect at once, as on the screen. Written to the user's, at once when that value is the one that counts: `ATELIER_MODEL`, `[models] main` and the project's `[defaults] model` win over it (`overridden`) |
| `defaults.context_limit` | `project` | In effect at once |
| `review.enabled` | `project` | In effect at once |

- The user-global file is the default, and the place for the user's own
  choices; but `context_limit` and `review.enabled` are read from the
  project's file only, so that is the one scope they take. A scope the
  setting is not read from is refused rather than written to no effect.
- `defaults.approve` (the screen's auto-approve) cannot be set: in a
  project's file it is a [gated setting](trust.md) — it switches every
  approval prompt off — and the user-global file is not read for it. Switch
  the session with `command/run` and `/mode yolo`. Nothing gated by trust,
  and nothing honoured in the user-global file only (`[local] model`,
  `[sandbox] root`), can be written to a project's file through this method.
- **The file is rewritten whole** from the parsed settings, as `/config` and
  `/mcp add` rewrite the project's: comments and the layout are not kept,
  and tables come back in atelier's order. A user-global file that cannot be
  read as settings is left alone (`-32603`).
- A change to the project's file is the user's own edit: in a trusted
  project the result stays trusted, in an untrusted one nothing is accepted
  by it (see [atelier's own edits](trust.md#ateliers-own-edits)).
- Fails with `-32602` for an unknown key, a value of the wrong type or not
  among the allowed ones, a setting that is not settable, or a scope it does
  not take; with `-32001` while a turn or a command runs — a setting does
  not change under a turn.

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
| `event/file_change` | `turn`, `tool`, `path`, `relative_path`?, `kind`, `diff`?, `added`?, `removed`?, `truncated`?, `binary`? | A tool wrote a file: what changed, as a unified diff; see [below](#eventfile_change) |
| `event/info` | `turn`?, `text` | Informational output: a command's, or the harness's during a turn |
| `event/notice` | `turn`?, `text` | An out-of-band notice: a warning, something that went wrong |
| `event/activity` | `turn`, `text` | A line of progress from a subagent |
| `event/review` | `turn`, `text` | A note from the [parallel reviewer](review.md) |
| `event/approval` | `turn`, `tool`, `arguments`, `decision`, `by`, `reason`, `risk`? | The [approval reviewer](permissions.md#the-approval-reviewer)'s verdict on a call (`"by": "reviewer"`): `decision` is `"allowed"`, `"denied"` or `"escalated"` (left to the client, or — after an `approval/request` went unanswered — refused). With `when = "always"` it precedes the `approval/request`; by default it comes only after one the client answered with an error or let time out |
| `event/delivered` | `turn`, `text` | A `turn/steer` message just reached the model |
| `event/idle` | `kind`, … | Output between turns; see below |
| `turn/ended` | see below | The turn is over |
| `request/cancelled` | `id`, `reason` | A request atelier made no longer needs an answer |

The payloads are those of the [`stream-json` events](headless.md#events-stream-json)
of the same name (`text`, `tool_start`, `tool_end`, `file_change`, …): the
two interfaces share one vocabulary.

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

### `event/file_change`

A tool call wrote, created or deleted a file. Sent once the write has
happened, between the call's `event/tool_start` and its `event/tool_end` —
so it describes what is on disk, and a call that was refused, or failed
before it wrote, sends none.

```json
{"jsonrpc":"2.0","method":"event/file_change",
 "params":{"turn":1,"tool":"edit","path":"/home/me/project/src/parser.rs","relative_path":"src/parser.rs",
           "kind":"modified","added":1,"removed":1,
           "diff":"--- a/src/parser.rs\n+++ b/src/parser.rs\n@@ -40,7 +40,7 @@\n fn nested(…\n-    if depth > 8 {\n+    if depth >= 8 {\n …"}}
```

| Field | Meaning |
|-------|---------|
| `tool` | The tool that wrote: `write`, `edit`, `multiedit`, `apply_patch` or `notebook_edit` |
| `path` | The file, absolute |
| `relative_path` | The file relative to the project root, with `/` separators. Absent for a file outside it (a subagent working in a worktree of its own) |
| `kind` | `created`, `modified` or `deleted` |
| `diff` | The change as a unified diff of the whole file, three lines of context: `--- a/<path>` and `+++ b/<path>` (`/dev/null` for the side that does not exist), then the hunks. Capped at 64 KiB, cut at the end of a line |
| `added`, `removed` | Lines added and removed — of the whole change, also when `diff` was cut |
| `truncated` | `true` when `diff` was cut — or left out altogether, with `added` and `removed`, the file being over 1 MiB before or after |
| `binary` | `true` when the file was not text (UTF-8) before the write: no `diff`, `added` or `removed` |

- One event per file: an `apply_patch` over three files sends three, in the
  patch's order. A patch that failed part-way reports the files it did
  write before its `tool_end` with `ok: false`.
- A write that leaves the file's text as it was sends nothing.
- A [subagent](agents.md)'s edits are reported in its parent's turn, between
  the `task` call's start and end — not those of an agent running in the
  background.
- With the [Claude backend](claude.md) the events come from atelier's tools,
  which is what claude is given by default; edits made by claude's own
  built-ins (`ATELIER_CLAUDE_BUILTINS=keep`) are not reported.
- **Not covered**: files changed by a `bash` command or by a
  [`node`](tools.md) script's `fs`, and atelier's own bookkeeping (`todo`,
  `memory`, spilled tool output). A client that needs those watches the
  directory.

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
| `-32602` | invalid params | A required param is missing or of the wrong type; for `settings/set`, also a setting that does not exist or cannot be set |
| `-32603` | internal error | A settings file could not be read or written (`settings/set`) |
| `-32001` | turn running | `turn/start`, `command/run`, `session/history`, `settings/get`, `settings/set` or unforced `shutdown` while a turn or command runs. `data: {"turn": n}` for a turn |
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
← {"id":1,"jsonrpc":"2.0","result":{"atelier":{"version":"0.1.1"},"capabilities":{"approval_timeout":null,"approvals":true,"commands":true,"file_changes":true,"history":true,"images":true,"interrupt":true,"questions":true,"steer":true},"protocol":1,"session":{"backend":"openai","cwd":"/home/me/project","mode":"auto-edit","model":"qwen3-coder","tools":["apply_patch","ask","bash","…","write"]}}}
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
← {"jsonrpc":"2.0","method":"event/file_change","params":{"added":1,"diff":"--- a/src/parser.rs\n+++ b/src/parser.rs\n@@ -40,7 +40,7 @@\n…","kind":"modified","path":"/home/me/project/src/parser.rs","relative_path":"src/parser.rs","removed":1,"tool":"edit","turn":1}}
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
permission modes. An edit reaches an ACP client as the `diff` content of
its tool call (old and new text, from the call's arguments) rather than as
`event/file_change`. Where they differ, `serve` is the smaller and the more
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
