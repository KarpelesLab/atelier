# The embedding protocol: `atelier serve`

atelier's own interface is deliberately small: an input line, a status
strip, scrollback. Anything richer — an editor integration, a web page,
another program driving an agent — lives *outside* atelier and talks to it
through this protocol.

```sh
atelier serve [--claude] [--continue] [--mode MODE] [--approval-timeout SECS]
atelier serve --listen WHERE [--allow-origin ORIGIN]... [--max-sessions N] [the flags above]
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

A client that cannot launch atelier and hold its stdin and stdout — a web
page, a program started separately — connects to `atelier serve --listen`
instead: the same protocol on a Unix socket or on a loopback port, a session
per connection. See [Listening on a socket](#listening-on-a-socket).

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
  session idle (`turn/start`, `command/run`, `session/history`,
  `settings/get`, `settings/set`, the [queries](#queries), `shutdown`) are
  refused while it is busy rather than queued; the others are answered at
  once, even in the middle of a turn.
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
  │ models/list, mcp/list, … ───────────────▶│
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
                 "approval_timeout":null,
                 "queries":["models/list","mcp/list","peers/list","usage/get",
                            "memory/list","commands/list","jobs/list"]}}
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
- `capabilities.queries` names the [queries](#queries) this build answers.
  A server from before they existed has no such field, and answers them
  with `-32601`.

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

## Queries

Methods that answer with data where a slash command prints lines: what an
interface needs to build a model picker, a list of MCP servers, a panel of
background jobs — without running `/models` or `/mcp` through
[`command/run`](#commandrun) and reading `event/info` text. Each returns
what the slash command of the same subject shows, from the same source.

- **They need the session idle**, like `command/run`: while a turn or a
  command runs they fail with `-32001`. (They read the session the turn is
  using, the disk, and for `models/list` the providers — unlike
  [`session/status`](#sessionstatus), which answers from a snapshot.) Ask
  before a turn, or after its `turn/ended`.
- They change nothing: no event is sent, nothing is written.
- **No result carries a credential.** A provider is described by where its
  key comes from, an MCP server without the headers it is sent.
- `initialize` lists them in `capabilities.queries`; a server that predates
  one answers `-32601`.
- Fields are only ever added. Unless a table says otherwise, a field is
  always present, `null` when there is nothing to say.

### `models/list`

The model in use, the providers and the models each offers, the model per
role and the variants — what `/model`, `/models`, `/providers` and `/effort`
show (see [Addressing a model](configuration.md#addressing-a-model)).

| Param | | |
|-------|-|-|
| `fetch` | optional | `false`: do not ask the providers which models they offer (`providers[].models` is then `null`). Default `true`: one `GET /models` per provider, each waiting for as long as an unreachable endpoint takes to fail |

```json
{"backend":"openai",
 "current":{"label":"local/qwen3-coder#high","model":"local/qwen3-coder#high",
            "provider":"local","id":"qwen3-coder","variant":"high",
            "options":{"reasoning_effort":"high"}},
 "providers":[
   {"name":"env","base_url":"http://192.168.0.50:11400/v1","key":"ATELIER_API_KEY",
    "default":false,"models":null,"error":"sending request: connection refused"},
   {"name":"local","base_url":"http://127.0.0.1:8080/v1","key":"none",
    "default":true,"models":["qwen3-coder","qwen3-small"],"error":null}],
 "roles":[
   {"role":"main","model":"local/qwen3-coder#high","provider":"local","id":"qwen3-coder",
    "options":{"reasoning_effort":"high"},"own":true},
   {"role":"small","model":"qwen3-small","provider":"local","id":"qwen3-small",
    "options":{},"own":true},
   {"role":"subagent","model":"local/qwen3-coder#high","provider":"local","id":"qwen3-coder",
    "options":{"reasoning_effort":"high"},"own":false},
   "…"],
 "variants":[{"name":"high","options":{"reasoning_effort":"high"}},
             {"name":"low","options":{"reasoning_effort":"low"}},
             {"name":"medium","options":{"reasoning_effort":"medium"}}]}
```

| Field | Meaning |
|-------|---------|
| `backend` | As in `initialize`: `openai`, `chatgpt` or `claude` |
| `current.model` | The main model as addressed (`provider/model#variant`, or any part of it): what `/model` takes |
| `current.label` | The same as `session/status`'s `model` (`chatgpt:<model>` when signed in with ChatGPT) |
| `current.provider`, `current.id` | The provider that serves it and the id it is sent as. `provider` is `null` when signed in with ChatGPT, which is its own endpoint |
| `current.variant`, `current.options` | The variant it is addressed with, or `null`, and the request fields that variant adds |
| `providers[]` | The configured endpoints, the environment's (`env`) first |
| `providers[].key` | Where the key comes from, never the key: `none`, `ATELIER_API_KEY`, `$VAR (set)` / `$VAR (not set)`, `from a command` |
| `providers[].default` | A model addressed without a provider is on this one |
| `providers[].models` | The ids it advertises; write one as `<name>/<id>` for `/model`. `null` when it was not asked, or could not be — then `error` says why |
| `roles[]` | `main`, `small`, `subagent`, `review`, `summary`: the model serving each, as addressed and resolved |
| `roles[].own` | The role has a model of its own, rather than falling back towards the main one |
| `variants[]` | The known variants (`low`, `medium`, `high` and the `[variant.*]` tables) with the request fields each stands for |

With the Claude backend, which picks and calls its own models, `providers`,
`roles` and `variants` are empty and `current` has only `label` and `model`
(the others `null`).

### `mcp/list`

The MCP servers and where each stands, with the tools of the connected
ones — `/mcp`, plus what connecting them reported.

```json
{"servers":[
  {"name":"github","transport":"stdio","command":"github-mcp","args":["--read-only"],
   "enabled":true,"configured":true,"status":"connected","error":null,
   "tools":[{"name":"mcp__github__search_issues","tool":"search_issues",
             "description":"Search issues and pull requests","title":"Search issues",
             "read_only":true,"destructive":null,"deferred":false}]},
  {"name":"docs","transport":"http","url":"https://mcp.example.com/mcp",
   "enabled":true,"configured":true,"status":"needs_login","error":null,"tools":[]},
  {"name":"db","transport":"stdio","command":"db-mcp","args":[],
   "enabled":true,"configured":true,"status":"failed",
   "error":"starting MCP server 'db': No such file or directory (os error 2)","tools":[]}]}
```

| Field | Meaning |
|-------|---------|
| `transport` | `stdio` (then `command`, `args`) or `http` (then `url`); `null` for a server the client attached. An HTTP server's headers are never returned |
| `enabled` | `false` for a server kept in `atelier.toml` with `enabled = false` |
| `configured` | In `atelier.toml`, rather than attached for this session by the program driving it |
| `status` | See below |
| `error` | Why connecting failed, for `failed` |
| `tools[].name` | The tool as the model calls it, `mcp__<server>__<tool>`; `tool` is the server's own name for it |
| `tools[].title`, `read_only`, `destructive` | What the server says about the tool (its annotations); `null` where it says nothing |
| `tools[].deferred` | The tool is reached through `mcp_search` / `mcp_call` rather than advertised to the model one by one |

| `status` | Meaning |
|----------|---------|
| `connected` | Its tools are available |
| `disabled` | `enabled = false` |
| `needs_login` | An HTTP server that wants a sign-in: `/mcp login <name>` |
| `failed` | Connecting it failed; `error` says how |
| `untrusted` | The [project is not trusted](trust.md), so the server was not started |
| `delegated` | The Claude backend: the server is handed to claude, which connects it itself |
| `disconnected` | Not connected, for none of these reasons |

### `peers/list`

The other atelier instances on this machine and the advisory locks, as
`/peers` shows them (see [Coordinator](coordinator.md)).

```json
{"enabled":true,"name":"atelier-2","link":"connected to coordinator 0.1.2 as 'atelier-2'","error":null,
 "instances":[
   {"name":"atelier","you":false,"status":"working","model":"qwen3-coder","cwd":"/home/me/project",
    "pid":4021,"version":"0.1.2","since":1767225600,"topics":["build"]},
   {"name":"atelier-2","you":true,"status":"idle","model":"qwen3-coder","cwd":"/home/me/project",
    "pid":4188,"version":"0.1.2","since":1767225720,"topics":[]}],
 "locks":[{"path":"/home/me/project/src/parser.rs","holder":"atelier","yours":false,
           "note":"rewriting the tokenizer","since":1767225650}]}
```

| Field | Meaning |
|-------|---------|
| `enabled` | This session has a coordinator link. `false` when it is switched off (`ATELIER_NO_COORDINATOR`) or on a platform without one (Windows): the lists are then empty, the rest `null` |
| `name` | This instance's name among the others |
| `link` | One line on the state of the link, as `/peers` prints it |
| `error` | Why the coordinator could not be asked (the link is down); `instances` is then empty |
| `instances[]` | Every connected instance, this one included (`you`): what it says of itself — `status` (`idle` / `working`), `model`, `cwd`, `pid`, `version` — when it connected (`since`, Unix seconds) and the `topics` it listens to |
| `locks[]` | The paths instances say they are working on: absolute `path`, the `holder`'s name, whether it is this instance (`yours`), its `note`, and `since` (Unix seconds) |

### `usage/get`

This session's model requests, tokens and cost — `/usage`.

```json
{"backend":"openai","context_tokens":5210,"output_tokens":1873,
 "by_model":[{"model":"env/qwen3-coder","requests":7,"prompt_tokens":31020,"completion_tokens":1873,
              "cached_tokens":24100,"duration_ms":41250,"cost":0.0123,"priced_requests":7}],
 "total":{"requests":7,"prompt_tokens":31020,"completion_tokens":1873,"cached_tokens":24100,
          "duration_ms":41250,"cost":0.0123,"priced_requests":7}}
```

| Field | Meaning |
|-------|---------|
| `context_tokens`, `output_tokens` | As in `session/status`'s `usage` |
| `by_model[]` | One row per `provider/model` this process sent requests to — subagents', the reviewer's and summaries' included |
| `requests`, `prompt_tokens`, `completion_tokens` | Summed over the row. `prompt_tokens` includes `cached_tokens`, the part the provider served from its cache |
| `duration_ms` | Time spent waiting on the model |
| `cost` | From the [`[[price]]`](configuration.md#usage-and-cost-price-atelier-stats-usage) tables; `null` when no request of the row has a price. With `priced_requests` < `requests` it is a lower bound |
| `total` | The same sums over all rows |

With the Claude backend, claude makes and bills the requests: `by_model` is
empty and `total` all zeros; `context_tokens` and `output_tokens` are what
claude reported.

### `memory/list`

What the agent remembers and where it is kept — `/memory` — and the
instruction files the session loads up front (see [Memory](memory.md)).

```json
{"scopes":[
   {"scope":"project","dir":"/home/me/project/.atelier/memory",
    "memories":[{"name":"build","type":"reference","description":"how to build and test",
                 "path":"/home/me/project/.atelier/memory/build.md"}]},
   {"scope":"global","dir":"/home/me/.local/share/atelier/memory","memories":[]}],
 "instruction_files":[{"path":"/home/me/.config/atelier/AGENTS.md"},
                      {"path":"/home/me/project/AGENTS.md"}]}
```

| Field | Meaning |
|-------|---------|
| `scopes[]` | `project`, then `global` (left out when no home directory is known), each with its directory — which need not exist yet |
| `memories[]` | The notes of the scope, by name: `type` is `user`, `feedback`, `project` or `reference`; `path` is the Markdown file, there to be read or edited |
| `instruction_files[]` | The instruction files in the model's standing context, least specific first: the user's, then the project's, then those `[instructions] files` names |

### `commands/list`

What can be typed after a slash — `/help` — and the skills the agent can
load — `/skills`.

```json
{"builtin":[{"name":"help","usage":"/help","description":"show this help"},
            {"name":"model","usage":"/model [provider/model#variant]",
             "description":"show or switch the active model"},
            {"name":"mcp","usage":"/mcp","description":"list configured MCP servers"},
            {"name":"mcp","usage":"/mcp remove <name>","description":"remove an MCP server"},
            "…"],
 "custom":[{"name":"fix","description":"fix an issue","argument_hint":"<issue>","model":null,
            "path":"/home/me/project/.atelier/commands/fix.md"}],
 "prompts":[{"name":"mcp:github:review","server":"github","prompt":"review",
             "description":"Review a pull request","argument_hint":"<number>"}],
 "skills":[{"name":"deploy","description":"ship a release",
            "path":"/home/me/project/.atelier/skills/deploy","global":false}],
 "skipped_skills":[".atelier/skills/broken/SKILL.md: no `name` in the front matter"]}
```

| Field | Meaning |
|-------|---------|
| `builtin[]` | The built-in commands, one entry per form (`/mcp`, `/mcp add …` are two): `name` without the slash, `usage` as typed, a one-line `description`. Some belong to the terminal (`/config`, `/quit`: see [`command/run`](#commandrun)) |
| `custom[]` | The [command files](commands.md) of the project and the user, less those a built-in command shadows: `name` without the slash, `argument_hint` and `model` from the front matter, `path` of the file |
| `prompts[]` | The prompts of the connected MCP servers, run as `/<name>`: `server`, the server's name for it (`prompt`), and what to type after it (`argument_hint`). One a command file shadows is left out |
| `skills[]` | The [skills](skills.md) found: `path` is the skill's directory, `global` whether it is the user's rather than the project's |
| `skipped_skills[]` | One line per `SKILL.md` that was skipped, saying why |

Run any of the first three with `command/run`.

### `jobs/list`

The work running in the background: the `bash` jobs the model started with
`background: true`, and the subagents `/agents` lists as running.

```json
{"jobs":[{"id":"job-1","command":"cargo watch -x test","state":"running","status":"running",
          "exit_code":null,"runtime_ms":184220},
         {"id":"job-2","command":"cargo build --release","state":"exited",
          "status":"exit status: 0","exit_code":0,"runtime_ms":42010}],
 "agents":[{"id":"agent-1","agent":"explore","description":"find the callers of parse()",
            "state":"running","activity":"grep parse( src/","steps":null,
            "runtime_ms":9120,"worktree":null}],
 "claude_background":0}
```

| Field | Meaning |
|-------|---------|
| `jobs[]` | Every background job of the process, in the order `bash_output` lists them; one that ended stays until the model removes it (`bash_kill`) |
| `jobs[].state` | `running` or `exited`. `status` is the same in words (`exit status: 0`, or the signal that ended it); `exit_code` the code, `null` while it runs or when a signal ended it |
| `jobs[].runtime_ms` | How long it ran, or has been running |
| `agents[]` | The background subagents that are running, or have ended and not yet reported to the model (their report goes with the next prompt) |
| `agents[].state` | `running`, or how it ended: `done`, `wrapped-up` (it ran out of steps), `failed`, `stopped` |
| `agents[].activity` | The last thing it did |
| `agents[].steps` | Model requests it made, once it has ended |
| `agents[].worktree` | The git worktree it works in, when it has one of its own |
| `claude_background` | As `session/status`'s `background`: tasks running in claude |

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
| `-32001` | turn running | `turn/start`, `command/run`, `session/history`, `settings/get`, `settings/set`, a [query](#queries) or unforced `shutdown` while a turn or command runs. `data: {"turn": n}` for a turn |
| `-32002` | not initialized | A request other than `initialize` came first |
| `-32003` | no turn running | `turn/steer` or `turn/interrupt` with nothing to act on |
| `-32004` | background running | Unforced `shutdown` with background tasks running. `data: {"background": n}` |
| `-32005` | already initialized | A second `initialize` |
| `-32020` | unauthorized | [`--listen`](#listening-on-a-socket) on TCP: the connection's first line was not the token. `"id": null`; the connection is closed |
| `-32021` | too many sessions | [`--listen`](#listening-on-a-socket): `--max-sessions` connections are being served already. `"id": null`; the connection is closed |

A turn that fails is not a JSON-RPC error — `turn/start` had already
succeeded: it ends with `turn/ended` and `status: "error"`.

## A session, on the wire

`→` is the client writing, `←` is atelier. Auto-approve is off, so the shell
command asks.

```
→ {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"client":{"name":"demo","version":"0.1"}}}
← {"id":1,"jsonrpc":"2.0","result":{"atelier":{"version":"0.1.1"},"capabilities":{"approval_timeout":null,"approvals":true,"commands":true,"file_changes":true,"history":true,"images":true,"interrupt":true,"queries":["models/list","mcp/list","…"],"questions":true,"steer":true},"protocol":1,"session":{"backend":"openai","cwd":"/home/me/project","mode":"auto-edit","model":"qwen3-coder","tools":["apply_patch","ask","bash","…","write"]}}}
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
"lines in, lines out": the messages carry no transport state. `atelier serve
--listen`, below, is that pair of ends pointed at a socket, plus what stdio
gets for free: knowing who is on the other end, and deciding what a
disconnect means.

## Listening on a socket

```sh
atelier serve --listen unix:/run/user/1000/atelier/ui.sock
atelier serve --listen 127.0.0.1:8123 --allow-origin http://localhost:5173
```

`atelier serve --listen WHERE` runs no session of its own. It waits for
connections and gives each one **a session of its own, in a process of its
own**: for every connection it lets in it starts `atelier serve` — the same
executable, in the listener's working directory, with the listener's other
flags (`--mode`, `--claude`, `--approval-timeout`, …) — and carries the
messages between the two. So everything above holds unchanged on a
connection: `initialize` first, the same methods, events and error codes.
Sessions share nothing but the project directory and what any two atelier
processes there share (with `--continue`, every connection resumes — and
saves over — the directory's one saved session).

| Flag | Meaning |
|------|---------|
| `--listen unix:PATH` | A Unix domain socket (Unix-like systems). A value with a `/` in it is taken as a path without the prefix |
| `--listen 127.0.0.1:PORT`, `[::1]:PORT`, `localhost:PORT` | A loopback TCP port; `0` lets the system pick one. `localhost` is `127.0.0.1`. **Any other address is refused**: whoever connects drives an agent with your files and your shell |
| `--allow-origin ORIGIN` | A web origin (`scheme://host[:port]`) whose pages may connect over WebSocket. Repeatable; none by default |
| `--max-sessions N` | How many connections are served at once (default 8) |

On start the listener prints **one line on stdout** and nothing after it,
for a program that launched it to read; what a person needs goes to stderr,
as do the logs of every session.

```json
{"listening":{"transport":"tcp","address":"127.0.0.1:8123","ws":"ws://127.0.0.1:8123/","token_file":"/run/user/1000/atelier/serve-8123.token"}}
{"listening":{"transport":"unix","path":"/run/user/1000/atelier/ui.sock"}}
```

### Who may connect

**A Unix socket** is protected by the file system. It is created with mode
`0600` in a directory no other user can enter: a directory that does not
exist is created with mode `0700`, and one that is open to others (`/tmp`,
say) is refused. There is no token. If the path is already a socket that a
live listener answers on, the new one refuses to start; a socket nobody
answers on is replaced; anything else at that path is left alone and
refused.

**A TCP port** on loopback can be reached by every user and every program
on the machine, and by any web page your browser shows. So:

- **A token comes first.** The listener makes one — 256 random bits,
  base64url, 43 characters — and writes it to a file only you can read
  (`serve-PORT.token` in `$XDG_RUNTIME_DIR/atelier`, else
  `~/.local/state/atelier`; the path is `token_file` in the line above). Or
  choose it yourself in the environment variable `ATELIER_SERVE_TOKEN` (16
  to 512 characters of `A-Z a-z 0-9 - . _ ~`; `token_file` is then `null`).
  The token is never taken from the command line, which other users can
  read, and it is not passed on to the sessions' environment. It is compared
  in constant time, and a connection has ten seconds to present it.
- **A page's origin must be allowed.** A browser states where a page comes
  from in the `Origin` header of a WebSocket handshake, and the page cannot
  change it. A handshake with an `Origin` is refused (`403`) unless that
  origin was given with `--allow-origin` — compared exactly, scheme and port
  included — and this is checked before the token is looked at. A handshake
  without an `Origin` is a program's, not a page's, and needs the token
  only. `--allow-origin null` admits pages opened from files *and* every
  sandboxed frame on any site; use it for development at most.

### Framing

The listener tells the two framings apart by a TCP connection's first line.

**JSON-RPC lines** — for programs, and the only framing on a Unix socket.
Exactly the stdio protocol: one message per line each way. On TCP the first
line is the token, as `{"token": "…"}`; nothing answers it, and the
protocol's lines follow. A wrong or missing token is answered with error
`-32020` and the connection is closed.

```sh
# Unix socket
socat - UNIX-CONNECT:/run/user/1000/atelier/ui.sock
{"jsonrpc":"2.0","id":1,"method":"initialize"}

# TCP: the token line first
(printf '{"token":"%s"}\n' "$(cat "$XDG_RUNTIME_DIR/atelier/serve-8123.token")"; cat) | nc 127.0.0.1 8123
{"jsonrpc":"2.0","id":1,"method":"initialize"}
```

**WebSocket** (RFC 6455, version 13) — for browsers, on the same TCP port.
The first line is an HTTP `GET` with `Upgrade: websocket`; any path will do.
Each JSON-RPC message is one **text** message, each way. The token travels
in the handshake, in one of two places a browser can put it:

- a subprotocol: offer `atelier` and `token.<TOKEN>` (the `Sec-WebSocket-Protocol`
  header). The listener answers with `atelier`, never with the token. Prefer
  this: the token stays out of URLs;
- the query: `ws://127.0.0.1:8123/?token=<TOKEN>`.

```js
const ws = new WebSocket("ws://127.0.0.1:8123/", ["atelier", "token." + token]);
ws.onopen = () =>
  ws.send(JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize",
                           params: { client: { name: "my-ui", version: "0.1" } } }));
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  if (m.method && m.id !== undefined) { /* a request to answer: approval/request, question/request */ }
  else if (m.method) { /* a notification: event/*, turn/ended */ }
  else { /* the response to the request with this id */ }
};
```

with the listener started as `atelier serve --listen 127.0.0.1:8123
--allow-origin http://localhost:5173` (the page's own origin). How the page
learns the token is the front end's business — its own server reads the
token file, or it starts the listener with `ATELIER_SERVE_TOKEN` set.

The details: no extensions and no compression; fragmented messages are put
back together; a ping is answered with a pong; binary messages are refused
(close code `1003`), as are frames that are not masked (`1002`), text that
is not UTF-8 (`1007`) and messages over 32 MiB (`1009`, the limit on a line
too). A handshake that fails gets an HTTP error, which a page cannot see
beyond the fact of it: `403` (the origin), `401` (the token), `426` (not a
WebSocket upgrade, or not version 13 — this port serves no pages), `400`.

### Connections and sessions

- **A connection is a session**; a second one is another session. The
  session's process starts with the connection's first message.
- **A disconnect ends the session**, as the end of stdin ends a `serve`
  launched directly: a running turn is interrupted, background work is
  stopped, and the process exits (it is killed if it has not after ten
  seconds). Nothing waits for the client to come back; a client that wants
  the conversation again starts the listener with `--continue`.
- **`shutdown` ends the connection**: after its answer the listener closes
  the connection (a WebSocket with close code `1000`). If the session's
  process ends any other way, the client is sent an error with `"id": null`
  and code `-32603` first.
- **`--max-sessions`** connections are served at once. One more is let in
  as far as the token, answered with error `-32021` (`"id": null`) and
  closed (a WebSocket with close code `1013`); it may try again later.
- **The listener runs until it is killed.** Its sessions then end as on a
  disconnect. It handles no signals, so the socket file and the token file
  stay behind; both are harmless — the token opens nothing any more — and
  the next listener on the same socket or port replaces them.
