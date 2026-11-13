# Editors: `atelier acp` (Agent Client Protocol)

The [Agent Client Protocol](https://agentclientprotocol.com) (ACP) is how
editors — Zed, the JetBrains IDEs, Neovim plugins — drive coding agents: the
editor launches the agent as a subprocess and speaks JSON-RPC 2.0 with it
over stdio. `atelier acp` is that agent. With it an editor gets atelier —
its loop, tools, permission rules, MCP servers, hooks, either backend —
without atelier growing a UI.

```sh
atelier acp [--claude] [--mode MODE] [--max-turns N]
```

| Flag | Meaning |
|------|---------|
| `--claude` | Use the [Claude backend](claude.md) |
| `--mode MODE` | The [permission mode](permissions.md) sessions start in: `plan`, `ask-edits`, `auto-edit` (default), `yolo`. The editor can switch it |
| `--max-turns N` | Stop a prompt after N model requests (it ends with the stop reason `max_turn_requests`). Without it there is no limit |

Everything else — the model, the endpoint, `atelier.toml`, instruction
files, hooks — is configured as for any other run (see
[Configuration](configuration.md)); the environment is the one the editor
starts the process with.

> **Status.** This implements **ACP protocol version 1**, written against
> the specification and JSON schema as published on 2026-11-13 (schema
> release 1.31.0). Every message atelier sends is checked against that
> schema by the test suite (`tests/acp.rs`), which plays the editor's part.
> **It has not been run against a real editor yet**: how Zed or another
> client lays out what it is sent, and anything a client does that the
> specification does not say, is untested. Reports are welcome.

## Setting it up

### Zed

Add an agent server to Zed's `settings.json` (`zed: open settings`):

```json
{
  "agent_servers": {
    "atelier": {
      "command": "atelier",
      "args": ["acp"],
      "env": {
        "ATELIER_BASE_URL": "http://localhost:8080/v1",
        "ATELIER_MODEL": "your-model"
      }
    }
  }
}
```

`command` must be on the `PATH` Zed has, or an absolute path. `env` is where
the [environment variables](configuration.md) go when Zed is not started
from a shell that has them; leave it out otherwise. Newer Zed versions also
take `"type": "custom"` in the entry. Then open the agent panel and start a
new thread with "atelier".

### Other clients

Any ACP client that launches an agent over stdio takes the same two things:
the command `atelier` and the argument `acp`. atelier logs to stderr
(clients usually keep it in an "ACP logs" view) and writes nothing but
protocol messages to stdout.

## What is supported

| ACP | atelier |
|-----|---------|
| `initialize` | Version negotiation (always answers version 1) and capabilities — below. No authentication is asked for (`authMethods` is empty): the model backends authenticate out of band (`ATELIER_API_KEY`, `/login`, claude's own sign-in) |
| `session/new` | Opens the session in `cwd` and connects the MCP servers: the project's configured ones (if the project is [trusted](trust.md)) and the ones the client sends. Answers with the session id and the modes |
| `session/prompt` | A turn of the conversation, or a slash command. The response is the stop reason: `end_turn`, `cancelled`, `max_turn_requests` |
| `session/cancel` | Interrupts the running turn (and prompts still queued behind it); they end as `cancelled` |
| `$/cancel_request` | For a `session/prompt` request: cancels that prompt alone |
| `session/set_mode` | Switches the permission mode |
| `session/update` | `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`, `plan`, `available_commands_update`, `current_mode_update`, `usage_update`, `notice` |
| `session/request_permission` | atelier's approval prompt |
| `fs/read_text_file`, `fs/write_text_file` | Used by `read`, `write`, `edit`, `multiedit` when the client offers them |
| `elicitation/create` (form) | The `ask` tool's question, when the client offers form elicitation |

Advertised agent capabilities: `promptCapabilities.image` and
`.embeddedContext` (not `audio`), `mcpCapabilities.http` (not `sse`),
`loadSession: false`.

### One session per process

ACP lets one agent process hold several sessions, each with its own working
directory. atelier's session is the process's — its tools and the sandbox go
by the process's current directory — so `atelier acp` changes to the `cwd`
of `session/new`, opens the session there, and refuses a second
`session/new` with error `1001` and a message that says why. A client that
wants another session starts another `atelier acp`.

### Prompts

| Content block | Becomes |
|---------------|---------|
| `text` | The prompt's text |
| `resource_link` | A Markdown link in its place — `[name](/absolute/path)` for a `file://` URI, so the agent can `read` it; `[name](uri)` otherwise |
| `resource` (text) | The same link in its place, and the text at the end of the prompt as `<file path="…">…</file>` — it may be a buffer that is not saved, which no `read` would find |
| `image`, `resource` (an image blob) | An image attached to the prompt ([vision](quickstart.md)) |
| `audio` | Refused (`-32602`): not advertised |

A prompt whose text starts with `/` is a slash command, as at the terminal.
The built-in ones that make sense in an editor are advertised
(`available_commands_update`): `/compact`, `/context`, `/usage`, `/model`,
`/mode`, `/goal`, `/init`, `/memory`, `/skills`, `/agents`, `/tools`, `/mcp`,
`/permissions`, `/trust`, `/new`, `/help` — together with your
[command files](commands.md) and the
[prompts of MCP servers](mcp.md#prompts-as-slash-commands). A command's
output is the answer to the prompt (several lines come as one code block);
a command that stands for a prompt (`/init`, a command file, an MCP prompt)
then runs its turn. The others still run if typed, except that `/quit` and
`/config` say they are the terminal's.

Prompts sent while one runs are queued and run in order. There is no
mid-turn steering in ACP; to redirect a turn, cancel it and send another
prompt.

### Tool calls

Each tool call is reported as a `tool_call` and finished by a
`tool_call_update`:

- **Status.** A call that runs without asking starts as `in_progress`. One
  that needs approval starts as `pending`, is the subject of a
  `session/request_permission`, and goes to `in_progress` once allowed.
  Either ends as `completed` or `failed`. A call refused before it starts
  (a deny rule, plan mode, a hook) is reported once, as `failed`, with the
  reason.
- **Kind.** `read` (`read`, `skill`, `bash_output`), `edit` (`write`, `edit`,
  `multiedit`, `apply_patch`, `notebook_edit`), `search` (`grep`, `glob`,
  `ls`, `tree`, `web_search`, `mcp_search`), `execute` (`bash`, `bash_kill`,
  `node`), `fetch` (`web_fetch`), `think` (`todo`, `task`), `other`
  (everything else, MCP tools included). `delete` and `move` are never
  used: atelier has no tool for either (`bash` does both).
- **Title, name, input.** `title` is one line (`Read src/main.rs`, the
  command of a `bash` call); `name` is the tool's name; `rawInput` its
  arguments.
- **Locations.** The file a `read`, `write`, `edit`, `multiedit` or
  `notebook_edit` touches (absolute), with the line where `read` starts or
  where the text an `edit` replaces is; every file of an `apply_patch`.
- **Content.** The tool's output as text (cut at 16 KB; the model has all
  of it). For the editing tools also the change as `diff` entries, from the
  start of the call: `edit` and `multiedit` with the text replaced and its
  replacement, `apply_patch` hunk by hunk, `write` with the whole file
  against the one on disk (`oldText` is `null` for a new file). Note that
  for `edit`, `multiedit` and `apply_patch` the diff's `oldText`/`newText`
  are the lines concerned, not the whole file. A change that failed leaves
  no diff. A subagent's progress lines are the content of its `task` call
  while it runs.

The `todo` tool's list is the agent's plan: after each call that changes it
the whole list is sent as a `plan` (entries are `pending` or `completed`,
priority `medium` — atelier's list has neither "in progress" nor
priorities).

### Permissions and modes

An approval prompt is a `session/request_permission` for the pending tool
call, with what is known about the risk as content, and these options:

| Option | Kind | Effect |
|--------|------|--------|
| `allow-once` | `allow_once` | Run this call |
| `allow-always` | `allow_always` | Run it and save the narrowest [permission rule](permissions.md) that allows calls like it to `atelier.toml`, as "always" does at the terminal. Not offered when a rule of yours asks every time (the rule is named instead) |
| `reject-once` | `reject_once` | Refuse; the model is told |

Anything else — the `cancelled` outcome, an error, an option that was not
offered — is a refusal. `session/cancel` answers a pending request itself;
atelier does not wait for the client's `cancelled`.

The four permission modes are ACP session modes with the same ids: `plan`,
`ask-edits`, `auto-edit`, `yolo`. `session/set_mode` switches; a switch
made with `/mode` is reported as `current_mode_update`. A `session/set_mode`
sent while a turn runs takes effect, and is answered, when that turn ends.

### Files through the editor

When the client advertises `fs.readTextFile` / `fs.writeTextFile`, the text
of files goes through it:

- `read` asks the client for the file, so the agent sees a buffer's unsaved
  text. If the client has none to give (an error: the file is outside its
  workspace, is not text, does not exist), the disk is read instead.
- `write`, `edit` and `multiedit` write through the client, so the change
  appears in the editor. A write the client refuses fails; it is not made
  on the disk behind the editor's back.
- `edit` and `multiedit` still require the file to have been read, and
  still refuse to edit one that changed since — compared against what the
  client returns now.

The rest reads and writes the disk directly: `apply_patch`,
`notebook_edit`, `grep`, `bash` and the other tools, and images and PDFs
read by `read`. A buffer that is not saved is invisible to them.

Paths are confined to the session's directory exactly as without a client
(see [Tools](tools.md)); the client's own checks come on top.

ACP's `terminal/*` methods are not used: atelier's `bash` tool runs commands
itself, with its own sandbox, background jobs and permission rules.

### Questions

The `ask` tool's question is sent as a form `elicitation/create` when the
client advertises `elicitation.form`: one field `choice` with the options (a
string to pick, or an array when several may be chosen) and, where the
question allows an answer of the user's own, a text field `other`. Without
that capability there is no one to ask, and the model is told to decide for
itself, as in a [headless run](headless.md).

### MCP servers from the client

The servers in `session/new`'s `mcpServers` — stdio ones with their `env`,
and `http` ones with their `headers` — are connected for the session, next
to the configured ones. They are not written to `atelier.toml`, and they
are connected whether or not the project is [trusted](trust.md): trust is
about what a *repository* may start, and these are your editor's. A server
whose name a configured one already has is skipped. With the Claude backend
they are not passed on (claude takes its servers from the settings).

### Notices

Lines atelier would print beside the conversation — how connecting an MCP
server went, project settings waiting to be trusted, a background job that
ended — are sent as `notice` updates to a client that advertises
`session.notices`, and go to stderr for one that does not. A warning during
a turn (a hook's, a retry after compaction) is a `notice` too, or, without
the capability, a paragraph in the answer.

## What is not supported

| | |
|---|---|
| `session/load`, `session/resume`, `session/list`, `session/close`, `session/delete` | Not advertised. atelier keeps one saved conversation per directory (`--continue`), with no id a client could ask for, and cannot replay it as updates |
| Several sessions in one process | See above |
| `terminal/*` | Never used |
| `authenticate`, `logout` | No authentication methods |
| `audio` content, the `sse` MCP transport | Not advertised; refused |
| `session/set_config_option` (config options) | Modes are offered the older way only (`modes`, `session/set_mode`) |
| `additionalDirectories`, `systemPrompt` on `session/new` | Not advertised; ignored |
| Stop reasons `max_tokens`, `refusal` | Never sent. A prompt a `user_prompt` [hook](hooks.md) blocks, like any other failure of a turn, is the request's error (`-32603`) with the reason as its message |
| Compaction updates, message ids | Not sent. `/compact` reports as text |

## Errors

| Code | When |
|------|------|
| `-32700` | A line that is not JSON (answered with a `null` id) |
| `-32600` | Not a JSON-RPC message; a request before `initialize`; a second `initialize` |
| `-32601` | A method atelier does not implement |
| `-32602` | Malformed or unsupported parameters: a relative `cwd`, an unknown mode, an `audio` block, an `sse` server |
| `-32603` | The turn failed (the model's endpoint, a hook that blocked the prompt): the message says how |
| `-32002` | A `sessionId` this connection did not create (`data.sessionId` repeats it) |
| `1001` | A second `session/new` |

## See also

- [The embedding protocol](protocol.md): `atelier serve`, atelier's own
  JSON-RPC protocol — smaller, with mid-turn steering, for a front end
  written for atelier rather than for ACP.
- [Permissions](permissions.md), [Trust](trust.md), [MCP](mcp.md).
