# MCP (Model Context Protocol)

atelier can connect to MCP servers over **stdio** or **Streamable HTTP** and
merges their tools into the same registry as the built-ins, so the model
calls them exactly like any other tool. All MCP tools flow through the same
[permission model](permissions.md) as `bash` — every MCP tool call requires
approval, regardless of server, transport, or tool.

A server configured in a project's `atelier.toml` is started (or contacted)
only in a project you have [trusted](trust.md): a stdio server is a command
of the file's choosing. In an untrusted project the startup notice lists the
servers and `/trust` starts them. Servers you add yourself with `/mcp add`
are connected at once either way.

The other direction — atelier's own tools, offered to another program's
agent over MCP — is [`atelier mcp`](#atelier-as-an-mcp-server), at the end
of this page.

## Configuring a stdio server

### `/mcp add`

```
/mcp add filesystem npx -y @modelcontextprotocol/server-filesystem .
```

`/mcp add <name> <command> [args...]` spawns `command args...`, performs the
MCP `initialize` → `notifications/initialized` → `tools/list` handshake, and
registers each advertised tool. If the connection succeeds, the server
configuration is appended to `atelier.toml` (`[[mcp]]`) so it reconnects
automatically on the next launch; nothing is saved if the connection fails.
The command line supports quoting for arguments containing spaces (e.g.
`/mcp add srv sh -c "server --root /a b"`).

### `atelier.toml`

The same configuration, written by hand:

```toml
[[mcp]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
```

## Configuring an HTTP (Streamable) server

### `/mcp add`

Pass a `http://` or `https://` URL as the second argument instead of a
command, followed by any number of `Header: Value` pairs to send on every
request (e.g. an API token):

```
/mcp add search https://mcp.example.com/mcp Authorization: Bearer sk-xyz
```

`dispatch_mcp` distinguishes the two forms by whether the target starts with
`http://`/`https://`. As with stdio, a successful connection is persisted
(this time under `[[mcp_http]]`); a failure isn't saved.

### `atelier.toml`

```toml
[[mcp_http]]
name = "search"
url = "https://mcp.example.com/mcp"
headers = ["Authorization: Bearer sk-xyz"]
```

| Field     | Type       | Meaning |
|-----------|------------|---------|
| `name`    | string     | Logical server name; tools are namespaced `mcp__<name>__<tool>` |
| `url`     | string     | The server's MCP endpoint URL |
| `headers` | string[]   | `"Name: Value"` strings sent on every request (default: empty) |

Both `[[mcp]]` (stdio) and `[[mcp_http]]` (HTTP) servers are connected
automatically at startup (`Session::connect_configured_mcp`), before the
first prompt — all at once, each on its own thread (`mcp::connect_all`), so
startup waits for the slowest server rather than for the sum of them. Their
tools are nevertheless registered in the order of the file (the `[[mcp]]`
tables, then the `[[mcp_http]]` ones, each server's tools as it listed them),
whichever server answered first: that order is the order tools are advertised
to the model in, and it has to be the same on every run for the provider's
prompt cache to be reused. A server that fails to connect is reported (e.g.
`mcp: failed to connect '<name>': ...`) but doesn't stop the others from
connecting or abort startup. Names are shared across both kinds — you can't reuse a name
already taken by a stdio *or* HTTP server.

## Per-server options

Both kinds of table take the same optional keys, all left out of the file at
their defaults:

```toml
[[mcp]]
name = "github"
command = "github-mcp"
enabled_tools = ["list_*", "get_*", "create_issue"]
disabled_tools = ["get_secret*"]
startup_timeout_secs = 20
tool_timeout_secs = 300

[[mcp_http]]
name = "search"
url = "https://mcp.example.com/mcp"
enabled = false            # kept in the file, not connected
```

| Field                  | Default | Meaning |
|------------------------|---------|---------|
| `enabled`              | `true`  | `false` keeps the server configured without connecting it; `/mcp` lists it as `(disabled)` |
| `enabled_tools`        | all     | When non-empty, only the tools matching one of these names are registered |
| `disabled_tools`       | none    | Tools never registered, even when `enabled_tools` matches them |
| `startup_timeout_secs` | `10`    | How long each request of the connection handshake may take |
| `tool_timeout_secs`    | `120`   | How long one tool call may take |

**Tool filters** are matched against the server's own tool names — `list_issues`,
not `mcp__github__list_issues` — in full, with `*` standing for any run of
characters. The two tools atelier adds itself go by `read_resource` and
`get_prompt` here, so `disabled_tools = ["get_prompt"]` drops that one, and
an `enabled_tools` list keeps them only if it names them. A filtered tool is
never shown to the model and cannot be called; this is a different thing from
a [permission rule](permissions.md), which decides whether a tool the model
*can* see runs unasked.

**Timeouts.** A server that doesn't answer `initialize` (or `tools/list`)
within `startup_timeout_secs` is reported — `mcp: failed to connect '<name>':
… did not answer `initialize` within 10s` — and skipped; a stdio server's
process is killed. Without this a server that hung at startup held the session
forever (stdio) or for as long as the HTTP client's own read timeout (HTTP).
The limit is per request, so a server that answers each step slowly can take
a few times as long in total. A tool call that runs past `tool_timeout_secs`
returns an error to the model and the connection stays usable: over stdio the
server is sent `notifications/cancelled` and whatever it still answers for
that call is discarded; over HTTP each call is its own exchange.

With the [Claude backend](claude.md) the servers are handed to `claude`, which
connects them itself: `enabled = false` is honoured (the server is left out),
tool filters and timeouts are not.

## Signing in to a remote server (OAuth)

An `[[mcp_http]]` server may require OAuth. Nothing needs configuring for the
common case:

```
/mcp add tracker https://mcp.example.com/mcp
added MCP server 'tracker' — it requires sign-in: run /mcp login tracker
/mcp login tracker
opening your browser to sign in to 'tracker'…
signed in to 'tracker' — 12 tool(s) now available
```

- **At startup** a server that answers `401` and has no usable stored token
  is reported — `mcp: 'tracker' needs sign-in — run /mcp login tracker` — and
  skipped. atelier never opens a browser on its own.
- **`/mcp login <name>`** signs in and connects the server. The browser is
  opened on the authorization page (the URL is also printed); atelier waits up
  to five minutes for the redirect back to a listener on `127.0.0.1`.
- **`/mcp logout <name>`** forgets the tokens and drops the server's tools;
  the server stays configured.

What `/mcp login` does, in order (`src/mcp/oauth.rs`):

1. Sends `initialize` without credentials and reads the `WWW-Authenticate`
   header of the `401`.
2. Fetches the **protected-resource metadata** (RFC 9728) from the header's
   `resource_metadata` URL, falling back to
   `/.well-known/oauth-protected-resource` on the server's origin, to learn
   the authorization server. A server that publishes none is taken to be its
   own authorization server.
3. Fetches the **authorization-server metadata** (RFC 8414
   `/.well-known/oauth-authorization-server`, else OpenID Connect discovery).
4. **Registers a client dynamically** (RFC 7591) as a public client for the
   loopback redirect it is about to use — unless the server's table sets
   `client_id`, for authorization servers without dynamic registration:

   ```toml
   [[mcp_http]]
   name = "tracker"
   url = "https://mcp.example.com/mcp"
   client_id = "my-registered-client"
   ```

   A pre-registered client must allow `http://127.0.0.1:<any port>/callback`
   as a redirect URI; the port is chosen at each sign-in.
5. Runs the **authorization-code grant with PKCE** (S256), with the server's
   URL as the `resource` (RFC 8707) and the scopes the `401` or the resource
   metadata asked for, checks `state` on the way back, and exchanges the code.

Tokens are stored per server URL in `$XDG_CONFIG_HOME/atelier/mcp-auth.json`
(else `~/.config/atelier/mcp-auth.json`), readable by the owner only (0600),
never in `atelier.toml`. The access token is sent as `Authorization: Bearer`
to that server URL and nowhere else; an expired one is refreshed first, and
one the server rejects early is refreshed once and the request retried. When
it cannot be refreshed the call fails and the server asks for `/mcp login`
again.

Safeguards: every endpoint named by the discovery documents must be `https`
(or `http` on loopback) or the sign-in is refused; an authorization server
that lists its PKCE methods without `S256` is refused. A server whose
`headers` already include an `Authorization` is left to that header — no
stored token is added and a `401` is an ordinary error.

Not covered: client secrets (confidential clients), a fixed redirect port,
and re-running discovery when an authorization server moves its token
endpoint (sign in again). With the [Claude backend](claude.md) `claude`
signs in to MCP servers itself.

## Managing servers

`/mcp add` and `/mcp remove` rewrite `atelier.toml`. In a project that is
trusted, or that had nothing needing trust, the change is recorded as
trusted — you are not asked about your own action. In an untrusted project
the server works for the session and is listed with the rest of the file's
gated settings until you `/trust`.

- `/mcp` (no arguments) — list every configured server, stdio and HTTP alike
  (HTTP entries are marked `(http)`).
- `/mcp login <name>` / `/mcp logout <name>` — sign in to, or out of, an HTTP
  server that requires OAuth (see above).
- `/mcp remove <name>` (or `/mcp rm <name>`) — drop the server (whichever
  kind it is) from `atelier.toml` and remove its tools (everything named
  `mcp__<name>__*`) from the live registry.

## Tool namespacing

Each tool a server advertises is exposed to the model as
`mcp__<server>__<tool>` (e.g. `mcp__filesystem__read_file`) — the prefix
disambiguates same-named tools from different servers, stdio or HTTP alike.
The server itself is never told about the prefix; atelier strips it back off
before issuing `tools/call`. The tool's description and JSON-Schema
`inputSchema` are passed through unchanged as the tool's spec.

A tool result's `content` array is flattened to a single string (its `text`
parts, newline-joined); a result with `isError: true` is surfaced to the
model as a tool error rather than a successful result. A result over 30,000
bytes comes back as its start and its end, with all of it saved to a file the
agent can `read` or `grep` ([spilled output](tools.md#long-output-goes-to-a-file));
the same goes for `read_resource`.

## Deferred tools

Every advertised tool costs its name, description and JSON Schema on every
request, and a few MCP servers can bring hundreds. When the specs of all
connected MCP tools together exceed a budget, atelier stops advertising them
one by one and gives the model two tools instead:

- `mcp_search {"query": "create issue", "limit": 5}` — the best-matching
  tools, each with its description and full input schema. The query is split
  into words; a tool matches if any word occurs in its name or description
  (case-insensitive). A word found in the name counts for more than one found
  only in the description, more matching words rank higher, and ties keep
  registration order. The server's name is part of a tool's name, so it finds
  that server's tools. `limit` defaults to 5 and is capped at 20.
- `mcp_call {"tool": "mcp__github__create_issue", "arguments": {…}}` — runs
  one of them.

The model is told, in the standing `MCP servers` context, which servers are
deferred and how many tools each has (`github (31 tools), db (12 tools)`), so
it knows to search. `/tools` lists the deferred tools after the advertised
ones, marked `(deferred)`; the startup line `mcp: N tools (… bytes of schemas)
are deferred` says when it happened.

The budget is `[defaults] mcp_tool_budget` in `atelier.toml`, in bytes of
name + description + schema summed over all MCP tools; the default is 8192
(about 2k tokens) and `0` never defers. Deferral is all or nothing across
servers, and is re-decided whenever the set of tools changes (`/mcp add`,
`/mcp remove`).

### Approval, permission rules and hooks see the real tool

`mcp_call` is not a tool that runs other tools. Before anything looks at a
call, the agent loop resolves `mcp_call {tool: T, arguments: A}` into the call
`T(A)` (`mcp::deferred::resolve`, applied in `ToolRunner::resolved`); from
there on it is a call to `T` in every respect:

- `pre_tool` and `post_tool` [hooks](hooks.md) match on and receive `T` and
  `A` — a hook with `match = "mcp__db__.*"` fires, one with `match =
  "mcp_call"` does not;
- [permission rules](permissions.md) are matched against `T` and `A`, so
  `deny = ["mcp__github__delete_*"]` and `allow = ["mcp__db__query(…)"]` mean
  what they mean without deferral, and a rule naming `mcp_call` matches
  nothing;
- the approval prompt names `T` and shows `A`, and "always" saves a rule for
  `T`.

Only a currently deferred tool is resolved. An `mcp_call` naming anything else
(an unknown tool, a built-in, `mcp_call` itself) is not rewritten and returns
an error telling the model to use the name `mcp_search` gave. `mcp_search`
reads an in-memory index and needs no approval.

Deferral applies to atelier's own loop. With the [Claude backend](claude.md)
the servers are connected by `claude`, which has its own handling of large
tool sets.

## Code mode: MCP tools from a script

The other way to keep MCP results out of the conversation is not to pass
them through the model at all. A [`node`](scripting.md#tools--calling-other-tools-code-mode)
script run with `tools: true` calls tools itself and prints what matters:

```js
var open = tools.call("mcp__tracker__search", { query: "is:open label:bug" }).json.items;
var stale = open.filter(function (i) { return i.updated < "2025-01-01"; });
console.log(stale.map(function (i) { return i.id + " " + i.title; }).join("\n"));
```

MCP tools are called by their real names (`mcp__<server>__<tool>`), whether
or not they are deferred; `tools.list()` and `tools.schema(name)` give a
script what `mcp_search` gives the model. A result that is a JSON object or
array comes parsed, as `json`. Every call is decided as a direct call of
that tool would be — the same permission rules, hooks and approval prompt,
under the tool's real name — so a script is not a way around a `deny` rule
or a prompt.

Which to use:

| | Deferred tools (`mcp_search` / `mcp_call`) | Code mode (`node`, `tools: true`) |
|---|---|---|
| Saves | the *schemas* of tools that are never called | the *results* the model does not need to read |
| Each call | one model step; the whole result enters the context | no model step; the script keeps the result |
| Good for | one or two calls whose result the model must read and reason about | many calls, loops over items, filtering/joining/counting large results |
| Costs | a search step before the first call of a tool | the model has to write a correct script without having seen a result's shape |

They combine: with deferral on, the model can look a tool up with
`mcp_search` (to learn its name and schema), try it once with `mcp_call` to
see what it returns, and then do the bulk of the work in a script.

## Tool annotations

A server may annotate a tool in `tools/list`. atelier keeps the title and the
`readOnlyHint` / `destructiveHint` hints (`mcp::Annotations`, reachable from
the tool through `McpTool::annotations()` and by tool name through
`mcp::Catalog`), and `/tools` shows them after the tool's name:

```
mcp__tracker__list_issues [read-only] List issues
mcp__tracker__close_issue [destructive]
mcp__tracker__comment
```

`[read-only]` is shown for `readOnlyHint: true`; `[destructive]` for
`destructiveHint: true` on a tool not also said to be read-only. A tool whose
server says nothing gets no marker — the protocol's defaults (not read-only,
possibly destructive) describe every unannotated tool and would mark them
all.

These are the server's claims about itself. They inform the listing; they do
not change approval — an MCP tool asks before running whatever its server
says, unless a [permission rule](permissions.md) allows it.

## Server instructions

A server's answer to `initialize` may carry `instructions`: how its tools are
meant to be used. atelier passes them to the model as standing context — in
the system message of every request, not only on the turn the server
connected — in an `MCP servers` section, each server's text under its own
name and introduced as the server's guidance for its tools, not as the user's
word. Each server's text is capped at 2 KiB (cut at a character boundary,
with a note saying so); a server that sends none adds nothing. `/mcp remove`
takes a server's instructions away with its tools.

The path they take: `connect_stdio`/`connect_http` return a `Connection`
(tools plus instructions); the session records it in an `mcp::Catalog`, a
shared handle that the `mcp` context provider (`src/context/mcp.rs`) reads on
every turn. With the Claude backend `claude` connects the servers itself and
handles their instructions its own way.

## The Streamable HTTP transport in detail

`src/mcp/http.rs` (`connect_http`, `HttpServer`) POSTs each JSON-RPC message
to the server's URL with `Accept: application/json, text/event-stream`, and
accepts either response shape:

- `Content-Type: application/json` — a single JSON-RPC object.
- `Content-Type: text/event-stream` — the (fully-buffered) SSE body is
  scanned for `data: <json>` lines and the one whose `id` matches the
  request is picked out; other events (notifications, unrelated ids) are
  ignored.

If the server returns an `Mcp-Session-Id` header on `initialize`, it's
captured and echoed back as a request header on every subsequent call, as
the spec requires. It shares all its handshake and tool-wrapping logic with
the stdio transport via the `JsonRpc` trait, and is covered by its own
end-to-end test against a hand-rolled HTTP server.

**Known gaps:** no server-initiated requests/notifications delivered over a
long-lived GET SSE stream (only the SSE embedded in the direct response to
our own POST is read), no resumable-stream replay (`Last-Event-ID`), and no
batched JSON-RPC requests (an array of messages in one POST). These cover
the common path most current MCP servers implement.

## Resources

If a connected MCP server advertises **resources** (`resources/list`), atelier
adds one extra tool per server, `mcp__<server>__read_resource`, whose
description lists the available resource URIs. The model calls it with a
`uri` argument to read a resource's text content. Servers without resources
get no such tool (and the connection still succeeds).

## Prompts

If a server advertises **prompts** (`prompts/list`), atelier adds a
`mcp__<server>__get_prompt` tool whose description lists the prompt names and
arguments. The model calls it with `{ name, arguments }` to expand a prompt
template into text. Servers without prompts get no such tool.

### Prompts as slash commands

Each prompt is also a command of yours: `/mcp:<server>:<prompt> [arguments]`.
`/help` lists them after the custom commands, with the prompt's description
and its arguments (`<required>`, `[optional]`).

```text
› /mcp:kb:review src/parser.rs style=terse
/mcp:kb:review → expanded from prompt 'review' of MCP server 'kb' (412 chars)
```

- **Arguments.** What you type is split into words like a shell would
  (quotes group). A word `name=value` naming one of the prompt's arguments
  sets it; the other words fill the arguments still unset, in the order the
  server declared them, and the last of those takes whatever words remain —
  so a prompt with one argument can be given a sentence without quotes. If a
  required argument has no value, or words are left over that no argument
  takes, atelier prints the usage line and does not call the server.
- **What is sent.** The server is asked with `prompts/get`; the text of the
  messages it returns, in order, becomes your prompt for the turn. Content
  that is not text is noted in place (`[image omitted]`); roles are dropped;
  more than 64 KiB is cut.
- **It is the server's text, not yours.** atelier prints the line above so
  you can see that a prompt was sent on a server's word, and sends the text
  exactly as received. Nothing in it is expanded or executed: the
  `` !`command` `` and `$1` of [command files](commands.md) are those files'
  syntax only, and reach the model here as plain characters.

The tool filters (`enabled_tools` / `disabled_tools`) decide what the *model*
is offered; they do not remove these commands. A [command
file](commands.md) with the same name (`mcp/kb/review.md`) wins. With the
Claude backend the servers are claude's, and there are no such commands.

## atelier as an MCP server

`atelier mcp` turns this around: it is an MCP **server** over stdio that any
MCP client — another coding harness, an editor, a desktop assistant — can
launch in a project directory to give its agent atelier's tools there. The
client's configuration is the usual one for a stdio server:

```json
{
  "mcpServers": {
    "atelier": { "command": "atelier", "args": ["mcp"] }
  }
}
```

The server works in the directory it is started in; a client that has a
setting for the working directory (`cwd`) should point it at the project.

```
atelier mcp [--mode MODE] [--allow-all] [--tools a,b,…] [--no-tools x,y,…]
```

It speaks newline-delimited JSON-RPC 2.0 (one message per line), answers
`initialize` with the protocol version the client asked for, and serves
`ping`, `tools/list` and `tools/call`. Nothing but protocol is written to
stdout; warnings go to stderr.

### The tools

`read`, `write`, `edit`, `multiedit`, `apply_patch`, `notebook_edit`, `grep`,
`glob`, `ls`, `tree`, `bash` (with `bash_output` and `bash_kill`),
`web_fetch`, `web_search`, `node`, `todo`, `memory` and `skill` — the same
tools, with the same schemas, a session has ([Built-in tools](tools.md)):

- **Confined to the project root**, like in a session: a path outside it is
  not written, and reading one needs an approval (below).
- **Read tracking lasts as long as the server process**: an `edit` of a file
  that changed since the server last read or wrote it is refused until it is
  read again. Start one server per project and keep it running.
- **Images** a tool returns (`read` on a picture) are MCP `image` content.
- Each tool carries **annotations**: `readOnlyHint` from whether the tool
  can change anything, and `destructiveHint: true` on `bash` and `write`.

Not served: `ask` (there is no user interface to put a question on), `task`
(subagents need a model — the client brings its own), the coordinator's
`peers`/`peer_send`, and the tools of MCP servers configured in
`atelier.toml` (the server does not connect them; give them to the client
directly). `--tools read,grep,glob` serves only the tools named;
`--no-tools bash,node` serves all but those. A name that is not a tool is a
usage error.

No model is involved and no conversation is kept: no system prompt, no
session file, no coordinator, and the `session_start`/`session_end`
[hooks](hooks.md) do not run. `pre_tool` and `post_tool` hooks do, for every
call, when the project is trusted.

### Permissions without a prompt

Every call goes through the same decision as in a session — the
[permission rules](permissions.md) of the project and the user, and a mode:

| | |
|---|---|
| `--mode MODE` | `plan`, `ask-edits`, `auto-edit` or `yolo`. Default **`auto-edit`**: edits inside the project run, `bash`, the web tools and reads outside the project need approval |
| `ATELIER_MODE` | the same, from the environment (the flag wins) |
| `--allow-all` | the same as `--mode yolo` |

The project's `[defaults] mode` and `ATELIER_APPROVE` are **not** consulted:
a server another program drives does not become `yolo` because a setting
meant for a terminal session says so.

What differs from a session is the prompt. There is no terminal, so a call
that would *ask*:

- is **refused**, with a result that says so and gives the rule that would
  allow it —

  ```
  error: bash needs approval, and `atelier mcp` has nobody to ask (this MCP
  client does not support elicitation). The call was not run. To allow it,
  the person running this server can add a rule to atelier.toml:

  [permissions]
  allow = ["bash(cargo test *)"]
  ```

  The rule is the narrow one "always" would have saved at a prompt. The
  client's agent can relay it; a person decides.

- or, if the client declared the MCP **elicitation** capability in
  `initialize`, is **put to the client's user**: the server sends
  `elicitation/create` with a three-way choice (`yes` / `always` / `no`) and
  waits for the answer on the same connection. `always` saves the narrow
  rule to `atelier.toml`, as at the prompt; a declined or cancelled form, or
  a client that goes away, is a no. While the question is open the server
  answers `ping`, and other requests wait until the call is done.

Deny rules refuse in every mode, `--allow-all` included, and the refusal
names the rule. Plan mode refuses the tools that change things.

`--allow-all` is **dangerous**: it hands the client's agent a shell and the
network with no check but your deny rules. Use it only where that agent is
already trusted with as much (a container, a throwaway checkout).

[Project trust](trust.md) applies as for a headless run: an untrusted
project's `allow` rules and hooks do not count — a cloned repository must
not approve its own commands — unless you have run `atelier trust accept`
there or start the server with `ATELIER_TRUST_PROJECT=1`. Rules in your own
`~/.config/atelier/atelier.toml` always count, which makes it the place for
what you want every project's server to allow. `atelier permissions check`
shows what the rules decide for a command.

### Limits

- One call at a time, in arrival order; there is no cancellation (a tool
  runs to its own timeout).
- A `node` script with `tools: true` works, each of its tool calls decided
  as above; one that would ask is asked through elicitation, or refused.
- `atelier mcp-serve <socket>` is something else: the internal relay the
  [Claude backend](claude.md) uses to reach a running session. It is not
  meant to be started by hand.
