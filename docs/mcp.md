# MCP (Model Context Protocol)

atelier can connect to MCP servers over **stdio** or **Streamable HTTP** and
merges their tools into the same registry as the built-ins, so the model
calls them exactly like any other tool. All MCP tools flow through the same
[permission model](permissions.md) as `bash` — every MCP tool call requires
approval, regardless of server, transport, or tool.

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

## Managing servers

- `/mcp` (no arguments) — list every configured server, stdio and HTTP alike
  (HTTP entries are marked `(http)`).
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
