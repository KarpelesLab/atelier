# Configuration

atelier has two configuration layers:

- **`Config`** (`src/config.rs`) — ephemeral connection info read from the
  environment at startup. Not persisted.
- **`Settings`** (`src/settings.rs`) — durable, user-editable state stored in
  `atelier.toml` at the project root: configured MCP servers and the
  always-allow permission list. Read at startup and rewritten whenever it
  changes (`/mcp add`/`/mcp remove`, granting "always" on a tool approval).

There is currently no project-vs-user-global precedence: `atelier.toml` is
read only from the current project root, and `Config`'s environment variables
have no `atelier.toml` equivalent.

## Environment variables

| Variable                  | Default                          | Read by | Meaning |
|----------------------------|-----------------------------------|---------|---------|
| `ATELIER_BASE_URL`        | `http://192.168.0.50:11400/v1`   | `Config::from_env` | OpenAI-compatible endpoint base; every request path is appended to it |
| `ATELIER_MODEL`           | `qwen3.8-unc:q4`                 | `Config::from_env` | Model id sent in every request |
| `ATELIER_API_KEY`         | *(unset)*                        | `Config::from_env` | Bearer token; sent as `Authorization: Bearer <key>` when set to a non-empty value |
| `ATELIER_APPROVE`         | *(unset)*                        | `agent::Session::new` | `all`, `yes`, or `1` disables every approval prompt for the session (headless/CI) |
| `ATELIER_HTTP_TIMEOUT_MS` | *(unset)*                        | `provider::stream_chat`, `provider::list_models` | Overrides the HTTP connect timeout in milliseconds for both the chat-completion stream (default 60000) and `GET /models` (default 15000). Non-numeric or `<= 0` values are ignored and the default is used |
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

[permissions]
allow = ["bash"]
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

Only the stdio transport is configurable this way today. An HTTP
(Streamable) MCP transport exists in `src/mcp/http.rs` (`connect_http`,
`HttpServer`) but has no `atelier.toml` shape or `/mcp` subcommand wired up
yet — see [MCP](mcp.md) for details.

### `[permissions] allow`

| Field   | Type       | Meaning |
|---------|------------|---------|
| `allow` | string[]   | Tool names the user has permanently approved via "always" at an approval prompt |

Populated automatically — you don't hand-author this list, though you may
pre-seed it. See [Permissions](permissions.md) for how entries land here and
how they're used.
