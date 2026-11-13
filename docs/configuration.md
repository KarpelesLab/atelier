# Configuration

atelier has two configuration layers:

- **`Config`** (`src/config.rs`) — ephemeral connection info read from the
  environment at startup. Not persisted.
- **`Settings`** (`src/settings.rs`) — durable, user-editable state stored in
  `atelier.toml` at the project root: configured MCP servers, the
  permission rules, `[defaults]`, `[review]`,
  `[[diagnostics]]`, and `[search]`. Read at startup
  and rewritten whenever it changes (`/mcp add`/`/mcp remove`, granting
  "always" on a tool approval, `/review [on|off]`, or the `/config` editor).

`atelier.toml` is read from the current project root. The one user-global
piece is `[permissions]`, also read from
`$XDG_CONFIG_HOME/atelier/atelier.toml` (see [`[permissions]`](#permissions)).
`Config`'s environment variables have no `atelier.toml` equivalent.

  always-allow permission list, `[defaults]`, `[review]`, and `[[hooks]]`.
  Read at startup and rewritten whenever it changes (`/mcp add`/`/mcp
  remove`, granting "always" on a tool approval, `/review [on|off]`, or the
  `/config` editor).

There is currently no project-vs-user-global precedence: `atelier.toml` is
read only from the current project root, and `Config`'s environment variables
have no `atelier.toml` equivalent. The one exception is [`[[hooks]]`](#hooks),
which is also read from a user-global `atelier.toml`.

## Environment variables

| Variable                  | Default                          | Read by | Meaning |
|----------------------------|-----------------------------------|---------|---------|
| `ATELIER_BACKEND`         | *(unset)*                        | `Config::from_env` | `claude` selects the [Claude backend](claude.md) (same as `--claude`); otherwise OpenAI-compatible |
| `ATELIER_CLAUDE_BIN`      | `claude`                         | `Config::from_env` | Claude backend: the Claude Code executable to launch |
| `ATELIER_CLAUDE_BUILTINS` | *(unset)*                        | `Config::from_env` | Claude backend: `keep` leaves claude's own file/shell/fetch tools on next to atelier's (default: switched off — see [claude.md](claude.md#tools)) |
| `ATELIER_BASE_URL`        | `http://192.168.0.50:11400/v1`   | `Config::from_env` | OpenAI-compatible endpoint base; every request path is appended to it |
| `ATELIER_MODEL`           | `qwen3.8-unc:q4`                 | `Config::from_env` | Model id sent in every request. Claude backend: passed as `--model` when set (e.g. `opus`, `sonnet`); unset leaves the choice to claude |
| `ATELIER_API_KEY`         | *(unset)*                        | `Config::from_env` | Bearer token; sent as `Authorization: Bearer <key>` when set to a non-empty value |
| `ATELIER_APPROVE`         | *(unset)*                        | `agent::Session::new` | `all`, `yes`, or `1` disables every approval prompt for the session — the `yolo` [mode](permissions.md#modes) — for headless/CI runs (see [Permissions](permissions.md)) |
| `ATELIER_MODE`            | *(unset)*                        | `main`, `permissions::Mode::from_env` | The permission [mode](permissions.md#modes) to start in: `plan`, `ask-edits`, `auto-edit` (the default) or `yolo`. Overridden by `--mode`; overrides `ATELIER_APPROVE` and `[defaults] mode`. Any other value is an error |
| `ATELIER_CONTEXT_LIMIT`   | `8000`                           | `agent::Session::new` | Token threshold (last request's total tokens) past which older history is compacted into a summary. Must be a positive integer or the default is used — see [Sessions](sessions.md) |
| `ATELIER_REVIEW`          | *(unset)*                        | `agent::Session::new` | `on`, `yes`, `1`, or `true` turns on the parallel "subconscious" reviewer for the session, overriding `[review].enabled` — see [Review mode](review.md) |
| `ATELIER_TRUST_HOOKS`     | *(unset)*                        | `hooks::Paths::from_env` | `1` runs the project's [hooks](hooks.md) without your stored approval — for headless runs (`--print`), which never ask and otherwise skip unapproved project hooks. Only for checkouts you control |
| `ATELIER_HTTP_TIMEOUT_MS` | *(unset)*                        | `provider::stream_chat`, `provider::list_models` | Overrides the HTTP connect timeout in milliseconds for both the chat-completion stream (default 60000) and `GET /models` (default 15000). Non-numeric or `<= 0` values are ignored and the default is used |
| `ATELIER_NO_UPDATE`       | *(unset)*                        | `update::spawn_background_check` | Any non-empty value other than `0` turns off release builds' daily background update check (`atelier update` still works) |
| `ATELIER_UPDATE_API`      | `https://api.github.com`         | `update` | GitHub API base the update check queries (for a mirror, or testing) |
| `ATELIER_NO_COORDINATOR`  | *(unset)*                        | `coord::start` | Any non-empty value other than `0`: don't join (or start) the [coordinator](coordinator.md) that connects atelier instances |
| `ATELIER_COORDINATOR_DIR` | `$XDG_RUNTIME_DIR/atelier`, else `~/.local/state/atelier` | `coord::runtime_dir` | Directory of the coordinator's socket, lock and log |
| `ATELIER_SEARCH_PROVIDER` | `duckduckgo` (`searxng` if a URL is set) | `web_search` tool | Search provider: `searxng`, `brave`, `tavily` or `duckduckgo`. Overrides `[search] provider` — see [`[search]`](#search) |
| `ATELIER_SEARCH_URL`      | *(unset)*                        | `web_search` tool | Base URL of the SearXNG instance (required for `searxng`); for the other providers, replaces their endpoint. Overrides `[search] url` |
| `ATELIER_SEARCH_KEY`      | *(unset)*                        | `web_search` tool | API key for `brave` or `tavily`. Overrides the variable named by `[search] key_env` |
| `ATELIER_DEBUG`           | *(unset)*                        | `provider::stream_chat` | If set to any value, the outgoing chat-completion request body is printed to stderr before sending |

## `atelier.toml`

Lives at the project root (`Settings::path`, i.e. `<root>/atelier.toml`).
Missing entirely is fine — atelier falls back to empty settings. A file that
fails to parse also falls back to empty settings, with a warning printed to
stderr.

```toml
[[mcp]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]

[[mcp_http]]
name = "search"
url = "https://mcp.example.com/mcp"
headers = ["Authorization: Bearer sk-xyz"]

[permissions]
allow = ["bash(cargo *)", "bash(git diff *)"]
deny = ["bash(rm -rf *)"]
```

### `[[mcp]]` — stdio MCP servers

An array of tables, one per server, launched over stdio and connected at
startup (`Session::connect_configured_mcp`):

| Field     | Type       | Meaning |
|-----------|------------|---------|
| `name`    | string     | Logical server name; tools are namespaced `mcp__<name>__<tool>` |
| `command` | string     | Executable to spawn |
| `args`    | string[]   | Arguments to the command (default: empty) |

Managed via `/mcp add <name> <command> [args...]` and `/mcp remove <name>`
(see [MCP](mcp.md)) — both read-modify-write this file, so hand edits are
also picked up on the next launch.

### `[[mcp_http]]` — Streamable HTTP MCP servers

An array of tables, one per server, connected over MCP's Streamable HTTP
transport at startup, same as `[[mcp]]`:

| Field     | Type       | Meaning |
|-----------|------------|---------|
| `name`    | string     | Logical server name; tools are namespaced `mcp__<name>__<tool>` |
| `url`     | string     | The server's MCP endpoint URL |
| `headers` | string[]   | `"Name: Value"` strings sent on every request (default: empty) |

Managed via `/mcp add <name> <http(s)://url> [Header: Value ...]` and `/mcp
remove <name>` — the same subcommands as stdio servers; atelier tells the two
apart by whether the target looks like a URL. See [MCP](mcp.md) for the
transport details and its known gaps.

### `[permissions]`

| Field   | Type       | Meaning |
|---------|------------|---------|
| `allow` | string[]   | Rules for calls that run without asking. "Always" at an approval prompt appends here |
| `ask`   | string[]   | Rules for calls that prompt even when they would otherwise run unasked |
| `deny`  | string[]   | Rules for calls that are refused |

A rule is `tool` or `tool(pattern)`; deny wins, then ask, then allow, then
the tool's default:

```toml
[permissions]
allow = ["bash(git diff *)", "bash(cargo *)", "mcp__github__*"]
ask   = ["read(.env*)"]
deny  = ["read(~/.ssh/**)", "bash(rm -rf *)"]
```

The same table in `$XDG_CONFIG_HOME/atelier/atelier.toml` (else
`~/.config/atelier/atelier.toml`) holds user-global rules, merged with the
project's — that file's other tables are not read. `atelier permissions
check -- <command>` explains a decision. See [Permissions](permissions.md)
for the pattern syntax per tool and how compound shell commands are judged.

- `ATELIER_TRACE` — file path to append a JSONL trace (one line per model request: messages, tool calls, usage) for debugging.

## `[defaults]`

Per-project defaults in `atelier.toml`, each overridden by its environment
variable when set:

```toml
[defaults]
model = "qwen3-coder:30b"   # overridden by ATELIER_MODEL
approve = false             # overridden by ATELIER_APPROVE
mode = "auto-edit"          # overridden by ATELIER_MODE and --mode
context_limit = 8000        # overridden by ATELIER_CONTEXT_LIMIT
```

`mode` is the permission [mode](permissions.md#modes) sessions start in:
`plan`, `ask-edits`, `auto-edit` (the default when unset) or `yolo`.
`approve = true` is the older spelling of `mode = "yolo"`; `mode` wins when
both are set. On the command line, `--mode <name>` (or `--mode=<name>`)
selects the mode for one run, interactive or `--print`; `/mode <name>`
switches it during a session.

## `[review]`

The parallel "subconscious" reviewer — see [Review mode](review.md) for what
it does and how its notes appear.

```toml
[review]
enabled = false              # overridden by ATELIER_REVIEW
model = "qwen3-coder:30b"    # optional; defaults to the main model
```

| Field     | Type   | Meaning |
|-----------|--------|---------|
| `enabled` | bool   | Whether the reviewer runs (default `false`); also toggled with `/review [on\|off]`, which persists it here |
| `model`   | string | Model override for the reviewer; unset reuses the active model |

## `[[hooks]]`

Commands run on lifecycle events — before and after tool calls, on a prompt,
when a turn ends. See [Hooks](hooks.md) for the events, what a hook receives
and how it answers, and worked examples.

```toml
[[hooks]]
event = "post_tool"            # session_start, session_end, user_prompt, pre_tool,
                               # post_tool, permission_request, pre_compact,
                               # post_compact, turn_end
match = "edit|write|multiedit" # optional: regex on the whole tool name
command = "rustfmt --edition 2024 \"$ATELIER_FILE\""
timeout_secs = 30              # optional, default 60
```

| Field          | Type    | Meaning |
|----------------|---------|---------|
| `event`        | string  | The event the hook runs on |
| `match`        | string  | Regex the whole tool name must match; tool events only (`pre_tool`, `post_tool`, `permission_request`) |
| `command`      | string  | Shell command (`sh -c`), run in the project root with the event as JSON on stdin |
| `timeout_secs` | integer | Seconds before the command is killed (default 60) |

`[[hooks]]` is read from two files: the project's `atelier.toml`, and your
own `$XDG_CONFIG_HOME/atelier/atelier.toml` (`~/.config/atelier/atelier.toml`
when that variable is unset), of which only this table is read. Your own
hooks always run. A project's run only after you approve them with `/hooks
trust`, and need approving again whenever they change; approvals are kept in
`$XDG_STATE_HOME/atelier/trust.json` (`~/.local/state/atelier/trust.json`).
`/hooks` lists what is configured and whether it runs.

## Instruction files

Standing instructions for the agent — conventions, commands, things not to
touch — live in plain Markdown files. atelier loads every one that applies,
broadest first, so a closer file can refine a broader one:

1. your own, for every project: `$XDG_CONFIG_HOME/atelier/AGENTS.md`
   (`~/.config/atelier/AGENTS.md` when that variable is unset);
2. one per directory from the repository root (the nearest parent containing
   `.git`) down to the directory atelier was launched in. Outside a
   repository, only the launch directory counts.

In each directory the first of `AGENTS.md`, `.atelier/instructions.md`,
`CLAUDE.md`, `.cursorrules` is used and the others are ignored. Each file is
shown to the model under a header with its path.

**Subdirectories.** An instruction file *below* the launch directory (say
`web/AGENTS.md`) is not loaded up front. It is handed to the model with the
result of the first `read`, `write` or edit of a file under that directory,
once per conversation.

**Imports.** A line containing only `@path` is replaced by that file's
content. The path is relative to the file doing the importing, absolute, or
starts with `~/`. Imports may nest five levels deep. One that can't be
followed — missing, a cycle, not text, or over 32 KiB — leaves a one-line
`[import @… skipped: …]` note in its place. Lines inside a fenced code block
are never imports. A project's instruction files can only import files of
their own repository; your user-global file can import from anywhere.

**Size.** The loaded files share a budget of 32 KiB. Past it the text is cut,
and a closing note names the file that was cut and any that were left out —
the more specific ones, since they come last. Instructions are sent with
every request, so they count against the model's context window each time;
keep them short.

**Edits take effect on your next message**; no restart is needed. (They
change the system message, so the inference server's prompt cache starts over
from there — see [the shape of a
request](sessions.md#the-shape-of-a-request).)

## `[[diagnostics]]`

Each turn atelier runs the project's lint/typecheck commands and tells the
model what they found — and, after an edit, what *changed*: the diagnostics
that are new, a count of those still there, and how many were fixed. A clean
project adds nothing to the context.

```toml
[[diagnostics]]
name = "tsc"
command = "npx tsc --noEmit --pretty false"
when = ["**/*.ts", "**/*.tsx"]
timeout_secs = 60
```

| Field          | Type     | Meaning |
|----------------|----------|---------|
| `name`         | string   | Label the diagnostics are reported under |
| `command`      | string   | Command line, run from the project root. It is split on spaces with quotes honored and run directly, without a shell — for pipes or `&&`, use `sh -c '…'` |
| `when`         | string[] | Globs relative to the project root. The checker re-runs only when a matching file (not ignored by `.gitignore`, not hidden) was added, removed or modified since its last run; otherwise its last result stands. Omitted: it runs every turn |
| `timeout_secs` | integer  | Seconds before the command and what it started are killed (default 60) |

A diagnostic is an output line that starts with a location —
`path:line[:col]: message` or `path(line,col): message` — which is what
compilers and most linters print (pick the linter's "unix", "compact" or
"concise" format if it has several). A message starting with `warning` counts
as a warning, `note`/`help`/`info` lines are skipped, and anything else is an
error. If a command fails without printing any such line, the first lines of
its output are reported as they are. At most 15 lines are shown per turn,
errors first, shared between the checkers.

Use commands that only read the tree: they run unprompted, every time a
matching file changes. A command that can't be started is reported once and
skipped for the rest of the session; one that times out is reported once,
tried again after the next change, and skipped after a second timeout in a
row. The list is read at startup.

**With no `[[diagnostics]]` at all**, atelier picks checkers from the
project's files, each only if its tool is already installed (nothing is ever
downloaded):

| Project has | Tool required | Command |
|-------------|---------------|---------|
| `Cargo.toml` | `cargo` on `PATH` | `cargo check --message-format short` |
| `tsconfig.json` | `node_modules/.bin/tsc`, and `npx` on `PATH` | `npx --no-install tsc --noEmit --pretty false` |
| `go.mod` | `go` on `PATH` | `go vet ./...` |
| `pyproject.toml` or `setup.py` | `ruff` on `PATH` | `ruff check --no-cache --output-format concise` |
| 〃 (no `ruff`) | a `python3`/`python` that can `import pyflakes` | `python3 -B -m pyflakes .` |

Configuring any checker turns auto-detection off, so list every one you want
— or a single always-clean one (`command = "true"`) to have none.

## `[search]`

Which provider the [`web_search`](tools.md#web_search) tool queries. Each
field is overridden by its environment variable, and both are read on every
call, so a change needs no restart.

```toml
[search]
provider = "searxng"                 # overridden by ATELIER_SEARCH_PROVIDER
url = "https://searx.example.org"    # overridden by ATELIER_SEARCH_URL
key_env = "BRAVE_API_KEY"            # the variable holding the key; ATELIER_SEARCH_KEY wins
```

| Field      | Type   | Meaning |
|------------|--------|---------|
| `provider` | string | `searxng`, `brave`, `tavily` or `duckduckgo`. Unset: `searxng` if a URL is set, else `duckduckgo` |
| `url`      | string | The SearXNG instance's base URL. For the other providers it replaces the endpoint (a proxy) |
| `key_env`  | string | **Name** of the environment variable that holds the API key — the key itself never goes in `atelier.toml` |

| Provider     | Needs | Request |
|--------------|-------|---------|
| `duckduckgo` | nothing | `GET https://html.duckduckgo.com/html/?q=…`, the HTML results page, parsed. No key, but no guarantee either: DuckDuckGo may answer an automated client with a challenge, and `web_search` then fails asking for one of the providers below |
| `searxng`    | `url` | `GET {url}/search?q=…&format=json`. The instance must enable the `json` format (`search.formats` in its `settings.yml`), or it answers 403 |
| `brave`      | key   | `GET https://api.search.brave.com/res/v1/web/search?q=…&count=N`, key in `X-Subscription-Token` |
| `tavily`     | key   | `POST https://api.tavily.com/search`, key as `Authorization: Bearer` |

The key is sent only as that request header. It is not written to
`atelier.toml`, tool results, errors or logs.

## `/config`

`/config` opens a settings view covering the model, auto-approve, the context
limit, and review mode. In the inline TUI it's a full-screen editor
(alternate screen) — edit the values and save & exit to write them back to
`atelier.toml`; in the plain REPL, which has no alternate screen, it instead
prints a read-only text summary of the same settings. Either way it shows
the active **backend** — the configured chat/completions endpoint, or
`chatgpt:<model>` while [signed in with ChatGPT](chatgpt.md) via `/login`.

## `/login` and `/logout`

`/login` runs [Sign in with ChatGPT](chatgpt.md) — OpenAI's OAuth flow for
using your own ChatGPT plan, no API key needed — and switches the session to
the Responses API backend. `/logout` signs out and reverts to the configured
endpoint. See [Sign in with ChatGPT](chatgpt.md) for the full picture,
including where tokens are stored and how login is restored on launch.
