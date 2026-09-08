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
first prompt. A server that fails to connect is reported (e.g. `mcp: failed
to connect '<name>': ...`) but doesn't stop the others from connecting or
abort startup. Names are shared across both kinds — you can't reuse a name
already taken by a stdio *or* HTTP server.

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
model as a tool error rather than a successful result.

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
