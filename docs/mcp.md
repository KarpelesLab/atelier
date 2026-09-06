# MCP (Model Context Protocol)

atelier can connect to MCP servers and merge their tools into the same
registry as the built-ins, so the model calls them exactly like any other
tool. All MCP tools flow through the same [permission model](permissions.md)
as `bash` — every MCP tool call requires approval, regardless of server or
tool.

## Configuring a stdio server

Today, only the **stdio** transport is wired up end to end. There are two
equivalent ways to configure one:

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

Other subcommands:

- `/mcp` (no arguments) — list configured servers.
- `/mcp remove <name>` (or `/mcp rm <name>`) — drop the server from
  `atelier.toml` and remove its tools (everything named `mcp__<name>__*`)
  from the live registry.

### `atelier.toml`

The same configuration, written by hand:

```toml
[[mcp]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
```

Every server listed here is connected automatically at startup
(`Session::connect_configured_mcp`), before the first prompt. A server that
fails to connect is reported (in the REPL: a `mcp: failed to connect '<name>':
...` line) but doesn't stop the others from connecting or abort startup.

## Tool namespacing

Each tool a server advertises is exposed to the model as
`mcp__<server>__<tool>` (e.g. `mcp__filesystem__read_file`) — the prefix
disambiguates same-named tools from different servers. The server itself is
never told about the prefix; atelier strips it back off before issuing
`tools/call`. The tool's description and JSON-Schema `inputSchema` are passed
through unchanged as the tool's spec.

A tool result's `content` array is flattened to a single string (its `text`
parts, newline-joined); a result with `isError: true` is surfaced to the
model as a tool error rather than a successful result.

## HTTP (Streamable) transport: implemented, not yet configurable

`src/mcp/http.rs` implements MCP's **Streamable HTTP** transport
(`connect_http`, `HttpServer`): it POSTs each JSON-RPC message and accepts
either a plain `application/json` response or a `text/event-stream` response
(scanning its `data:` lines for the matching `id`), and it tracks the
`Mcp-Session-Id` header the server may assign on `initialize`, echoing it
back on every subsequent request. It shares all its handshake and
tool-wrapping logic with the stdio transport via the `JsonRpc` trait, and is
covered by its own end-to-end test against a hand-rolled HTTP server.

**However, it is not yet reachable from configuration or `/mcp`**: there is
no `atelier.toml` shape for an HTTP server (`Settings`/`McpServerConfig` only
has `name`/`command`/`args`, i.e. stdio) and no `/mcp add` variant that takes
a URL. `connect_http` is fully implemented and tested but currently
unreachable from `main` — wiring it up is tracked as a follow-up in the
project roadmap. Known gaps even once wired: no server-initiated
requests/notifications over a long-lived GET SSE stream, no resumable-stream
replay (`Last-Event-ID`), and no batched JSON-RPC requests.
