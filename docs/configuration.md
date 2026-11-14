# Configuration

atelier has two configuration layers:

- **`Config`** (`src/config.rs`) — which backend, which endpoints and which
  model for which job: read from the environment at startup, with the model
  tables of `atelier.toml` ([providers](#provider--named-endpoints),
  [`[models]`](#models--a-model-per-role),
  [variants](#variantname--reasoning-effort-and-sampling)) laid over it.
- **`Settings`** (`src/settings.rs`) — durable, user-editable state stored in
  `atelier.toml` at the project root: configured MCP servers, the
  permission rules, `[defaults]`, `[review]`, `[[diagnostics]]`, `[[lsp]]`, `[search]`,
  `[[hooks]]`, `[goals]`, `[sandbox]`, `[instructions]` and the model
  tables. Read at startup
  and rewritten whenever it changes (`/mcp add`/`/mcp remove`, granting
  "always" on a tool approval, `/review [on|off]`, or the `/config` editor).

`atelier.toml` is read from the current project root. Some tables are also
read from your own `$XDG_CONFIG_HOME/atelier/atelier.toml`
(`~/.config/atelier/atelier.toml` when that variable is unset), with the
project's entries winning: [`[permissions]`](#permissions),
[`[[hooks]]`](#hooks), and the model tables — `[[provider]]`, `[models]`,
`[variant.*]`, `[[price]]`, and `[defaults]`' `model` and `provider`.
[`[instructions]`](#instruction-files) is read from both, and both lists
apply; so is [`[sandbox]`](#sandbox), whose `root` counts only there. The
other tables (`[[mcp]]`, `[[mcp_http]]`, `[review]`, `[[diagnostics]]`,
`[search]`, `[goals]`, the rest of `[defaults]`) are the project's alone.

## Environment variables

| Variable                  | Default                          | Read by | Meaning |
|----------------------------|-----------------------------------|---------|---------|
| `ATELIER_BACKEND`         | *(unset)*                        | `Config::from_env` | `claude` selects the [Claude backend](claude.md) (same as `--claude`); otherwise OpenAI-compatible |
| `ATELIER_BACKEND=local`, `ATELIER_LOCAL_MODEL`, `ATELIER_LOCAL_ACCEPT`, `ATELIER_OXINFER_BIN` | *(unset)* | `local` | The [model on this machine](local.md): select it, name the model, agree to its download unattended, use your own oxinfer |
| `ATELIER_CLAUDE_BIN`      | `claude`                         | `Config::from_env` | Claude backend: the Claude Code executable to launch |
| `ATELIER_CLAUDE_BUILTINS` | *(unset)*                        | `Config::from_env` | Claude backend: `keep` leaves claude's own file/shell/fetch tools on next to atelier's (default: switched off — see [claude.md](claude.md#tools)) |
| `ATELIER_BASE_URL`        | `http://192.168.0.50:11400/v1`   | `Config::from_env` | OpenAI-compatible endpoint base; every request path is appended to it. This endpoint is the provider named `env` — see [providers](#provider--named-endpoints) |
| `ATELIER_MODEL`           | `qwen3.8-unc:q4`                 | `Config::from_env` | The main model: a model id, or `provider/model#variant` (see [addressing a model](#addressing-a-model)). Wins over `[models] main` and `[defaults] model`. Claude backend: passed as `--model` when set (e.g. `opus`, `sonnet`); unset leaves the choice to claude |
| `ATELIER_API_KEY`         | *(unset)*                        | `Config::from_env` | Bearer token of the `env` provider; sent to it as `Authorization: Bearer <key>` when set to a non-empty value |
| `ATELIER_APPROVE`         | *(unset)*                        | `agent::Session::new` | `all`, `yes`, or `1` disables every approval prompt for the session: the `yolo` [mode](permissions.md#modes). Meant for headless/CI runs — `--print` asks nobody, so without it (or an allow rule) a call that needs approval is denied |
| `ATELIER_MODE`            | *(unset)*                        | `main`, `permissions::Mode::from_env` | The permission [mode](permissions.md#modes) to start in: `plan`, `ask-edits`, `auto-edit` (the default) or `yolo`. Overridden by `--mode`; overrides `ATELIER_APPROVE` and `[defaults] mode`. Any other value is an error |
| `ATELIER_CONTEXT_LIMIT`   | `8000`                           | `agent::Session::new` | Token threshold (last request's total tokens) past which older history is compacted into a summary. Must be a positive integer or the default is used — see [Sessions](sessions.md) |
| `ATELIER_PARALLEL_TOOLS`  | *(unset)*                        | `agent::batch` | `0`, `off`, `no` or `false` runs the read-only tool calls of one model step one after the other instead of [at the same time](tools.md#read-only-calls-run-in-parallel); any other value turns that on. Overrides `[defaults] parallel_tools` |
| `ATELIER_REVIEW`          | *(unset)*                        | `agent::Session::new` | `on`, `yes`, `1`, or `true` turns on the parallel "subconscious" reviewer for the session, overriding `[review].enabled` — see [Review mode](review.md) |
| `ATELIER_TRUST_PROJECT`   | *(unset)*                        | `trust::Store::from_env` | `1` accepts the [gated settings](trust.md) of the project's `atelier.toml` (MCP servers, checkers, providers, allow rules, a `yolo` default, hooks) for this run without recording anything — for headless runs (`--print`), which never ask and otherwise skip them. Only for checkouts you control |
| `ATELIER_TRUST_HOOKS`     | *(unset)*                        | `trust::Store::from_env` | `1` does the same for the project's [hooks](hooks.md) alone |
| `ATELIER_HTTP_TIMEOUT_MS` | *(unset)*                        | `provider::stream_chat`, `provider::list_models` | Overrides the HTTP connect timeout in milliseconds for both the chat-completion stream (default 60000) and `GET /models` (default 15000), and any provider's `timeout_secs`. Non-numeric or `<= 0` values are ignored |
| `ATELIER_NO_UPDATE`       | *(unset)*                        | `update::spawn_background_check` | Any non-empty value other than `0` turns off release builds' daily background update check (`atelier update` still works) |
| `ATELIER_UPDATE_API`      | `https://api.github.com`         | `update` | GitHub API base the update check queries (for a mirror, or testing) |
| `ATELIER_NO_COORDINATOR`  | *(unset)*                        | `coord::start` | Any non-empty value other than `0`: don't join (or start) the [coordinator](coordinator.md) that connects atelier instances |
| `ATELIER_COORDINATOR_DIR` | `$XDG_RUNTIME_DIR/atelier`, else `~/.local/state/atelier` | `coord::runtime_dir` | Directory of the coordinator's socket, lock and log |
| `ATELIER_COORDINATOR_WAKE` | *(unset)*                       | `agent::idle` | `never`, `questions` or `always`: whether a peer's question or message starts a turn in this instance while it is idle — see [waking](coordinator.md#waking-an-idle-instance). Overrides `[coordinator] wake`; any other value means `never` |
| `ATELIER_SEARCH_PROVIDER` | `duckduckgo` (`searxng` if a URL is set) | `web_search` tool | Search provider: `searxng`, `brave`, `tavily` or `duckduckgo`. Overrides `[search] provider` — see [`[search]`](#search) |
| `ATELIER_SEARCH_URL`      | *(unset)*                        | `web_search` tool | Base URL of the SearXNG instance (required for `searxng`); for the other providers, replaces their endpoint. Overrides `[search] url` |
| `ATELIER_SEARCH_KEY`      | *(unset)*                        | `web_search` tool | API key for `brave` or `tavily`. Overrides the variable named by `[search] key_env` |
| `ATELIER_SANDBOX`         | *(unset)*                        | `sandbox::state` | `1`/`true`/`on` runs `bash` commands in the [sandbox](sandbox.md), `0`/`false`/`off` does not, whatever `[sandbox] enabled` says. Needs a build with the `sandbox` feature |
| `ATELIER_SANDBOX_ROOT`    | `$XDG_DATA_HOME/atelier/sandbox/rootfs` | `sandbox::state` | The sandbox guest's root filesystem (a directory holding a Linux userland). Overrides the user-global `[sandbox] root` |
| `ATELIER_SANDBOX_INTERP`  | *(unset)*                        | `sandbox::state` | `1` runs the guest on nixvm's software CPU even where hardware virtualization is available |
| `ATELIER_TRACE`           | *(unset)*                        | `provider` | File path to append a JSONL trace to, one line per model request (messages, tool calls, usage), for debugging |
| `ATELIER_DEBUG`           | *(unset)*                        | `provider::stream_chat` | If set to any value, the outgoing chat-completion request body is printed to stderr before sending (the body only — never headers, so never a key) |

## `atelier.toml`

Lives at the project root (`Settings::path`, i.e. `<root>/atelier.toml`).
Missing entirely is fine — atelier falls back to empty settings. A file that
fails to parse also falls back to empty settings, with a warning printed to
stderr.

**Not all of it applies at once.** The file comes with the repository, so
the tables that run commands or grant access — `[[mcp]]`, `[[mcp_http]]`,
`[[diagnostics]]` (and auto-detected checkers), `[[lsp]]` (and auto-detected
language servers), `[[provider]]`,
`[permissions] allow`, a `yolo` default, `[[hooks]]`, `[search] key_env` and
`url`, `[sandbox] enabled` —
take effect only once you have accepted them for the project with `/trust`
(or `atelier trust accept`; `ATELIER_TRUST_PROJECT=1` for one run). See
[Project trust](trust.md). Your user-global file is never gated.

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
| `enabled` | bool       | `false` keeps the server configured but not connected (default: `true`) |
| `enabled_tools`  | string[] | If non-empty, only tools matching one of these names (`*` wildcards) are registered |
| `disabled_tools` | string[] | Tools never registered; wins over `enabled_tools` |
| `startup_timeout_secs` | integer | Seconds each handshake request may take before the server is skipped (default: `10`) |
| `tool_timeout_secs`    | integer | Seconds one tool call may take before it fails (default: `120`) |

The last five are shared with `[[mcp_http]]`; see
[MCP → Per-server options](mcp.md#per-server-options).

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
| `client_id` | string   | OAuth client id for `/mcp login`, when the authorization server has no dynamic registration (default: atelier registers itself) |
| `enabled`, `enabled_tools`, `disabled_tools`, `startup_timeout_secs`, `tool_timeout_secs` | | As for `[[mcp]]` |

Managed via `/mcp add <name> <http(s)://url> [Header: Value ...]` and `/mcp
remove <name>` — the same subcommands as stdio servers; atelier tells the two
apart by whether the target looks like a URL. See [MCP](mcp.md) for the
transport details and its known gaps.

A server that requires OAuth is signed in to with `/mcp login <name>`; its
tokens go to `mcp-auth.json` in the user config directory
(`$XDG_CONFIG_HOME/atelier/`, else `~/.config/atelier/`), mode 0600 — not
into this file. See [MCP → Signing in](mcp.md#signing-in-to-a-remote-server-oauth).

Servers of either kind are started or contacted only in a
[trusted project](trust.md).

### `[permissions]`

| Field   | Type       | Meaning |
|---------|------------|---------|
| `allow` | string[]   | Rules for calls that run without asking. "Always" at an approval prompt appends here |
| `ask`   | string[]   | Rules for calls that prompt even when they would otherwise run unasked |
| `deny`  | string[]   | Rules for calls that are refused |
| `reviewer` | table   | `[permissions.reviewer]`: a model that answers approval prompts — see below |

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
project's. `atelier permissions
check -- <command>` explains a decision. See [Permissions](permissions.md)
for the pattern syntax per tool and how compound shell commands are judged.

The project's `allow` list counts only in a [trusted project](trust.md);
its `ask` and `deny` lists always do.

#### `[permissions.reviewer]` — a model that answers approval prompts

Off unless enabled. When a call would *ask*, a model judges it against a
written policy and allows it, denies it, or leaves it to you — see
[Permissions → The approval reviewer](permissions.md#the-approval-reviewer)
for precedence, what the model is shown, and how it fails closed.

```toml
[permissions.reviewer]
enabled = true
model = "local/judge#low"
policy = ".atelier/approval-policy.md"
when = "unattended"
```

| Field | Type | Meaning |
|-------|------|---------|
| `enabled` | bool | Whether the reviewer is consulted. Default `false` |
| `model` | string | The judging model, a [model address](#addressing-a-model). Default: `[models] review`, then the main model. Required (on a configured `[[provider]]`) with the Claude backend |
| `policy` | string | A file holding the policy, in place of the built-in one (`atelier permissions reviewer-policy` prints that). Relative to the project root in a project's file; to the user config directory in the user-global file, where it may also be absolute or `~/…` |
| `when` | string | `"unattended"` (default): only where nobody can answer a prompt — `--print`, background subagents, `atelier mcp` without elicitation, a `serve` client that does not answer. `"always"`: ahead of interactive prompts too |
| `timeout_secs` | integer | Seconds the reviewer's one request may take before it counts as no verdict. Default 30 |

The table is read from the user-global file as well; a project's replaces it
whole, and counts only in a [trusted project](trust.md) (an untrusted
project can switch your reviewer off, never on). Verdicts are appended to
`$XDG_STATE_HOME/atelier/approvals.jsonl`.

## `[defaults]`

Per-project defaults in `atelier.toml`, each overridden by its environment
variable when set:

```toml
[defaults]
model = "qwen3-coder:30b"   # overridden by ATELIER_MODEL, and by [models] main
provider = "local"          # the provider a bare model name is on (default: env)
approve = false             # overridden by ATELIER_APPROVE
mode = "auto-edit"          # overridden by ATELIER_MODE and --mode
context_limit = 8000        # overridden by ATELIER_CONTEXT_LIMIT
mcp_tool_budget = 8192      # no environment variable
parallel_tools = true       # overridden by ATELIER_PARALLEL_TOOLS
```

`mode` is the permission [mode](permissions.md#modes) sessions start in:
`plan`, `ask-edits`, `auto-edit` (the default when unset) or `yolo`.
`approve = true` is the older spelling of `mode = "yolo"`; `mode` wins when
both are set. On the command line, `--mode <name>` (or `--mode=<name>`)
selects the mode for one run, interactive or `--print`; `/mode <name>`
switches it during a session. `mode = "yolo"` and `approve = true` are
honored only in a [trusted project](trust.md).

`mcp_tool_budget` is how many bytes of MCP tool specs (names, descriptions,
schemas) are advertised to the model before the tools are
[deferred](mcp.md#deferred-tools) behind `mcp_search` and `mcp_call`. The
default is 8192; `0` never defers.

`parallel_tools` (on when unset) lets the read-only tool calls of one model
step [run at the same time](tools.md#read-only-calls-run-in-parallel); `false`
runs every call one after the other.

## `[agents]`

Limits on [subagents](agents.md):

```toml
[agents]
max_background = 4   # background agents running at once (the default)
```

A `task` call with `background: true` past `max_background` running agents
is refused with an error result; `0` switches background agents off.

## `[[provider]]` — named endpoints

The environment describes one endpoint (`ATELIER_BASE_URL`,
`ATELIER_API_KEY`). It is the provider named `env`, and unless you say
otherwise it is the only one and everything goes to it. `[[provider]]` tables
add more OpenAI-compatible endpoints, each with a name:

```toml
[[provider]]
name = "local"
base_url = "http://192.168.0.50:11400/v1"

[[provider]]
name = "hosted"
base_url = "https://api.example.com/v1"
api_key_env = "HOSTED_API_KEY"        # the key is in this environment variable
headers = { "X-Org" = "abc" }         # optional, sent with every request
timeout_secs = 20                     # optional connect timeout

[[provider]]
name = "work"
base_url = "https://llm.corp.example/v1"
api_key_command = "pass show work/llm"   # or: the key is this command's output
```

| Field             | Type    | Meaning |
|-------------------|---------|---------|
| `name`            | string  | What the provider is called in a model address. One word, no `/` or `#`; `env` is taken |
| `base_url`        | string  | Endpoint base including `/v1`; `chat/completions` and `models` are appended |
| `api_key_env`     | string  | **Name** of the environment variable holding the bearer token, read at each request. Unset when a request is made is an error naming the variable |
| `api_key_command` | string  | Shell command (`sh -c`, run in the project directory) whose standard output, trimmed, is the bearer token. It runs at the first request and the result is kept for the rest of the process; when the endpoint answers 401 it is run again and the request retried once — so a helper that mints short-lived tokens just works. Its standard error is discarded |
| `headers`         | table   | Extra request headers |
| `timeout_secs`    | integer | Connect timeout (default 60 s for a completion, 15 s for `/models`); `ATELIER_HTTP_TIMEOUT_MS` wins |

Set at most one of `api_key_env` and `api_key_command`; with neither, no
`Authorization` header is sent. **The key itself never goes in
`atelier.toml`**, which gets committed — only where to find it. atelier sends
it as the `Authorization: Bearer` header of requests to that provider and
nowhere else: it is not printed by `/providers`, `ATELIER_DEBUG` or
`ATELIER_TRACE`, and if a server quotes it back in an error, it is blanked
out of the message.

A project's `[[provider]]` tables — all of them, with or without a key
source — are used only in a [trusted project](trust.md): a provider is where
the conversation is sent, `api_key_env` names a variable to send there, and
`api_key_command` is a shell command. Until then model choices that point at
one fall back to your own default. Providers in your user-global
`atelier.toml` are always in use.

Providers are read from the user-global file and from the project's; a
project entry replaces a global one of the same name. A table with a mistake
(no `base_url`, both key sources, a bad name) is skipped with a warning at
startup. `[defaults] provider` picks the default provider — the one a bare
model name is on — in place of `env`.

`/providers` lists the providers (name, base URL, where the key comes from)
and the model serving each role. `/models` lists what each provider's
`GET /models` reports, written `provider/model` when more than one provider
is configured.

### Addressing a model

Wherever a model is named — `ATELIER_MODEL`, `[models]`, `[defaults] model`,
`[review] model`, `/model` — the full form is

```
provider/model#variant        local/qwen3-coder:30b#high
```

and both ends are optional: a bare `model` is on the default provider, and
without `#variant` nothing is added to the request.

Model ids may contain `/` and `#` themselves (`meta/llama-3` on an
aggregator), so the prefix counts as a provider only if a provider by that
name is configured, and the suffix as a variant only if a variant by that
name exists. Otherwise they are part of the model id. `/model` says so when
a `#suffix` matched no variant, since that is usually a typo.

## `[models]` — a model per role

```toml
[models]
main     = "local/qwen3-coder:30b#high"
small    = "local/qwen3:0.6b"
subagent = "local/qwen3-coder:30b"
review   = "hosted/some-critic"
summary  = "local/qwen3.8:27b"
```

| Role       | Used for | When unset |
|------------|----------|------------|
| `main`     | The conversation. `ATELIER_MODEL` wins over it, and `/model` changes it for the session | `[defaults] model`, else the built-in default |
| `small`    | Cheap upkeep work. Today that is summaries (below); titles and memory upkeep will use it | `main` |
| `subagent` | Subagents started by the `task` tool | `main` |
| `review`   | The [reviewer](review.md) | `[review] model`, else `main` |
| `summary`  | The hand-off summary of a [compaction](sessions.md#compaction) | `small`, else `main` |

"`main`" in the last column means the main model *as it is at that moment*:
a role without a model of its own follows `/model`. Each entry is a [model
address](#addressing-a-model), so roles can live on different providers and
carry their own variant.

A summary written by a much weaker model than the one doing the work loses
detail the work depended on; `summary` exists so that `small` can be tiny
without that cost.

While [signed in with ChatGPT](chatgpt.md) the main model is the ChatGPT
one, and so are the roles that follow it. A role with a model of its own
keeps using its provider.

## `[variant.<name>]` — reasoning effort and sampling

A variant is a named set of request options. A model addressed as
`model#name` has them added to the body of every request:

```toml
[variant.careful]
reasoning_effort = "high"
temperature = 0.2
top_p = 0.9
max_tokens = 8192
extra = { top_k = 40, seed = 7 }     # any other body fields, as they are
```

| Field              | Type    | Request field |
|--------------------|---------|---------------|
| `reasoning_effort` | string  | `reasoning_effort` |
| `temperature`      | number  | `temperature` |
| `top_p`            | number  | `top_p` |
| `max_tokens`       | integer | `max_tokens` |
| `extra`            | table   | Each key becomes a top-level body field with its value, untouched — for what a particular server understands. Wins over the named fields above |

Three variants exist without being configured: **`low`**, **`medium`** and
**`high`** set `reasoning_effort` to that word. Defining `[variant.high]`
yourself replaces the built-in one. Whether an option does anything is up to
the server: one that doesn't know `reasoning_effort` typically ignores it,
and some reject unknown fields.

No option can replace the fields atelier sets itself (`model`, `messages`,
`stream`, `stream_options`, `tools`).

`/model name#variant` switches model and variant together. **`/effort
low|medium|high|off`** (or any variant's name) changes only the variant of
the current main model, from the next request on; `/effort` alone shows it.
Options travel as body fields, never in the messages, so changing them
mid-session rewrites nothing that was already sent (see [the shape of a
request](sessions.md#the-shape-of-a-request)).

With the ChatGPT backend (the Responses API) `reasoning_effort` is sent as
`reasoning.effort` and `max_tokens` as `max_output_tokens`; the other
options are sent as written. Variants and providers don't apply to the
[Claude backend](claude.md), where claude calls its own models.

## Usage and cost: `[[price]]`, `atelier stats`, `/usage`

Every model request atelier makes — the conversation's, the reviewer's, a
compaction summary — appends one line to a log under
`$XDG_STATE_HOME/atelier/usage/` (`~/.local/state/atelier/usage/` when that
variable is unset), one file per month (`2026-11.jsonl`):

```json
{"ts":1794528000,"provider":"local","model":"qwen3-coder:30b","project":"/home/me/src/app","prompt_tokens":12000,"completion_tokens":800,"cached_tokens":9000,"duration_ms":4120}
```

| Field               | Meaning |
|---------------------|---------|
| `ts`                | When the request finished, seconds since the Unix epoch |
| `provider`, `model` | Where it went: the provider's name (`env` for the endpoint from the environment, `chatgpt` when signed in with ChatGPT) and the model id as sent |
| `project`           | The directory atelier was run in |
| `prompt_tokens`, `completion_tokens` | As the endpoint reported them; 0 when it reports no usage |
| `cached_tokens`     | The part of `prompt_tokens` served from the provider's prompt cache, where it says (`prompt_tokens_details.cached_tokens`) |
| `duration_ms`       | From sending the request to the end of the response |

Nothing else is logged — no prompts, no answers. Writing is best-effort: a
log that can't be written never fails a turn. The files are yours to delete.

```
atelier stats [--days N] [--project] [--json]
```

prints totals for the last `N` days (default 30; days are UTC, today
included) — per day, per model, and per project. `--project` counts only
requests made from the current directory; `--json` prints one JSON document
(`total`, `by_day`, `by_model`, `by_project`, each with `requests`,
`prompt_tokens`, `cached_tokens`, `completion_tokens`, `duration_ms`, `cost`
and `priced_requests`). `/usage` prints the same per-model table for the
requests of the current session.

Cost appears where you have given a price:

```toml
[[price]]
model = "hosted/big"     # provider/model, or a bare model id for any provider
input = 3.0              # per million prompt tokens
output = 15.0            # per million completion tokens
cached = 0.3             # per million cached prompt tokens (default: the input price)
```

Prices are per million tokens in whatever currency you write them in, and
are applied when the totals are printed — from the `atelier.toml` of the
directory you run `atelier stats` in, then your user-global one — so a
changed price re-prices the past. A model without a price has no cost (`-`,
or `null` in JSON); a total that includes such requests is marked `+` and
`priced_requests` says how many requests the cost covers. The first
matching entry wins, a `provider/model` one before a bare one.

There are no budgets or limits: this only counts.

With the [Claude backend](claude.md), claude makes and bills the requests,
so nothing is logged; `/usage` shows the token counts claude reports
(context at its latest request, output over the session).

## `[review]`

The parallel "subconscious" reviewer — see [Review mode](review.md) for what
it does and how its notes appear.

```toml
[review]
enabled = false              # overridden by ATELIER_REVIEW
model = "qwen3-coder:30b"    # optional; [models] review wins; defaults to the main model
```

| Field     | Type   | Meaning |
|-----------|--------|---------|
| `enabled` | bool   | Whether the reviewer runs (default `false`); also toggled with `/review [on\|off]`, which persists it here |
| `model`   | string | Model for the reviewer (a [model address](#addressing-a-model)). `[models] review` is the newer spelling and wins; with neither, the reviewer uses the main model |

## `[goals]`

Bounds on the pursuit of a [`/goal`](goals.md) (or `--goal`): how far one
prompt may go before the turn ends with the goal unmet.

```toml
[goals]
max_rounds = 10      # times a prompt is sent back to work (default 10)
max_tokens = 50000   # output tokens a prompt may generate (default: no limit)
```

## `[loops]`

How long a [`/loop`](loops.md) may go on.

```toml
[loops]
max_runs = 50   # runs one loop gets before it stops by itself (default 50)
```

## `[coordinator]`

Whether your other atelier instances may set this one to work while it sits
idle at its prompt — see [Waking an idle
instance](coordinator.md#waking-an-idle-instance) for what that does and
what bounds it.

```toml
[coordinator]
wake = "never"   # never | questions | always (overridden by ATELIER_COORDINATOR_WAKE)
wake_max = 5     # turns it may start by itself before you type again
```

| Field      | Type    | Meaning |
|------------|---------|---------|
| `wake`     | string  | `never` (default): what peers send waits for your next prompt. `questions`: a `peer_ask` starts a turn. `always`: any peer message does |
| `wake_max` | integer | The most turns the instance starts by itself between two inputs of yours (default 5) |

The table is read from the project's `atelier.toml` only; to make it your
choice for every project, export `ATELIER_COORDINATOR_WAKE` instead.

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
when that variable is unset). Your own
hooks always run. A project's run only in a [trusted project](trust.md)
(`/trust`), and need accepting again whenever they change; decisions are kept
in `$XDG_STATE_HOME/atelier/trust.json` (`~/.local/state/atelier/trust.json`).
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

**More files: `[instructions]`.** `atelier.toml` can name instruction files
that are not found by name:

```toml
[instructions]
files = ["docs/conventions.md", "team/*.md"]
```

Each entry is a path or a glob (`*` within one directory, `**` across
directories — at most eight levels deep and 64 files per pattern), relative to
the directory atelier was launched in. They are loaded after the `AGENTS.md`
chain above, in the order written, a glob's matches sorted by path — each
under a header with its path, with imports, and within the same budget as the
other instruction files. A file already loaded is not repeated. A path that
does not exist leaves a `[skipped: not found]` note; a glob that matches
nothing is silent.

A project's `atelier.toml` can only name files of its own repository: an
entry that resolves outside it — `..` past the repository root, an absolute
path, `~/`, a symlink leading out — is not read, and a
`[skipped: outside the project]` note takes its place. The same table in your
user-global `~/.config/atelier/atelier.toml` has no such limit: its entries
may be absolute or start with `~/` (relative ones are taken from that file's
directory), apply to every project, and are loaded right after your
user-global `AGENTS.md`.

**Size.** The loaded files share a budget of 32 KiB. Past it the text is cut,
and a closing note names the file that was cut and any that were left out —
the more specific ones, since they come last. Instructions are sent with
every request, so they count against the model's context window each time;
keep them short.

**Edits take effect on your next message**; no restart is needed. (They
change the system message, so the inference server's prompt cache starts over
from there — see [the shape of a
request](sessions.md#the-shape-of-a-request).)

**Starting one.** `/init` has the agent write the repository's `AGENTS.md`:
it reads the README, the manifests, the CI and lint setup and any instruction
file already there, then writes a short file (about 60 lines at most) holding
only what it could not cheaply rediscover — the exact build, test and lint
commands, conventions that differ from the ecosystem's defaults, the
architecture in a few sentences, and what is easy to get wrong. An existing
`AGENTS.md` is improved rather than replaced, and the agent says what it
changed; a `CLAUDE.md` is left alone. Text after the command is passed on as
guidance: `/init the web/ app matters most`. The file is written with the
ordinary `write` tool, so the usual approval applies.

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

Checkers — configured or auto-detected — run only in a
[trusted project](trust.md): until you `/trust` it there are no diagnostics.

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

## `[[lsp]]`

Language servers for the [`symbols`](tools.md#symbols) tool, by file
extension. Without any, `symbols` uses the server it
[detects](tools.md#two-tiers) from the project's files, when that server is
installed; an entry replaces the detected server for the extensions it
lists, and adds languages atelier knows no server for.

```toml
[[lsp]]
name = "rust"
command = "rust-analyzer"
extensions = ["rs"]

[[lsp]]
name = "zig"
command = "zls"
extensions = ["zig"]
timeout_secs = 30
```

| Field          | Type     | Meaning |
|----------------|----------|---------|
| `name`         | string   | What the server is called in results |
| `command`      | string   | The command line, split like a shell would (quotes honored) and run directly in the project root — no shell. It must speak LSP on stdin/stdout (`--stdio` for the servers that need telling) |
| `extensions`   | string[] | The file extensions it serves, with or without the dot |
| `timeout_secs` | integer  | How long one `symbols` call waits for it, loading included, before answering from the text instead (default 20) |

The server is started the first time `symbols` needs it, not at startup, and
only in a [trusted project](trust.md) — it runs the project's code. The list
is read on each call, so an edit applies to servers not yet started. A
command that cannot be started, or that does not answer `initialize`, is
given up on for the session and `symbols` says why in its results;
`atelier doctor` lists the servers a project has.

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
| `url`      | string | The SearXNG instance's base URL. For the other providers it replaces the endpoint (a proxy). The key is sent there, so it is honored only in a [trusted project](trust.md) (`ATELIER_SEARCH_URL` always works) |
| `key_env`  | string | **Name** of the environment variable that holds the API key — the key itself never goes in `atelier.toml`. Honored only in a [trusted project](trust.md) (`ATELIER_SEARCH_KEY` always works) |

| Provider     | Needs | Request |
|--------------|-------|---------|
| `duckduckgo` | nothing | `GET https://html.duckduckgo.com/html/?q=…`, the HTML results page, parsed. No key, but no guarantee either: DuckDuckGo may answer an automated client with a challenge, and `web_search` then fails asking for one of the providers below |
| `searxng`    | `url` | `GET {url}/search?q=…&format=json`. The instance must enable the `json` format (`search.formats` in its `settings.yml`), or it answers 403 |
| `brave`      | key   | `GET https://api.search.brave.com/res/v1/web/search?q=…&count=N`, key in `X-Subscription-Token` |
| `tavily`     | key   | `POST https://api.tavily.com/search`, key as `Authorization: Bearer` |

The key is sent only as that request header. It is not written to
`atelier.toml`, tool results, errors or logs.

## `[sandbox]`

Runs the `bash` tool's commands inside nixvm instead of on the host —
experimental, and only in a build with the `sandbox` feature. See
[Sandboxed `bash`](sandbox.md) for what that means, what works in the guest
and how to make its root filesystem.

```toml
[sandbox]
enabled = true
memory_mb = 4096
```

| Field       | Type    | Meaning |
|-------------|---------|---------|
| `enabled`   | bool    | Route foreground `bash` commands through the sandbox. The project's file wins over the user-global one — once the project is [trusted](trust.md), not before; `ATELIER_SANDBOX` wins over both |
| `root`      | string  | The guest's root filesystem. **User-global file only** — in a project's `atelier.toml` it is ignored, since it decides what of the host the guest can read. Default `$XDG_DATA_HOME/atelier/sandbox/rootfs`; `ATELIER_SANDBOX_ROOT` wins |
| `memory_mb` | integer | Guest RAM ceiling in MiB (default 4096) |
| `network`   | array   | Reserved for an allowlist of hosts. Not implemented: the guest has no network, and `atelier sandbox status` says a list here is ignored |

## `/config`

`/config` opens a settings view covering the model, auto-approve, the context
limit, and review mode. In the inline TUI it's a full-screen editor
(alternate screen) — edit the values and save & exit to write them back to
`atelier.toml`; in the plain REPL, which has no alternate screen, it instead
prints a read-only text summary of the same settings. Either way it shows
the active **backend** — the configured chat/completions endpoint, or
`chatgpt:<model>` while [signed in with ChatGPT](chatgpt.md) via `/login`.

A program driving atelier through [`atelier serve`](protocol.md) has no such
screen: it reads the settings, with the file, variable or flag each value
comes from, with [`settings/get`](protocol.md#settingsget), and changes the
ones this screen edits with [`settings/set`](protocol.md#settingsset).

## `/login` and `/logout`

`/login` runs [Sign in with ChatGPT](chatgpt.md) — OpenAI's OAuth flow for
using your own ChatGPT plan, no API key needed — and switches the session to
the Responses API backend. `/logout` signs out and reverts to the configured
endpoint. See [Sign in with ChatGPT](chatgpt.md) for the full picture,
including where tokens are stored and how login is restored on launch.
