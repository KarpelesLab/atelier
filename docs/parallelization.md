# Parallel work plan

atelier is one crate, but the work is split so several agents build at once
without colliding. The rule that makes it safe: **each work stream owns exactly
one directory and touches nothing else.** The shared spine (`provider`, `agent`,
`main.rs`, `Cargo.toml`) is already in place, so no stream needs to edit shared
files.

## Ownership

| Stream    | Owns            | Milestone | Depends on (contracts only) |
|-----------|-----------------|-----------|-----------------------------|
| **spine** | `provider/`, `agent/`, `main.rs`, `config.rs`, `Cargo.toml` | M0–M1 | — (done, in base) |
| **tools** | `src/tools/`    | M1        | `provider::ToolSpec`, `tools::{Tool, ToolCtx, FileState}` |
| **context** | `src/context/` | M4       | `context::{ContextProvider, ContextItem}` |
| **tui**   | `src/tui/`      | M2        | `agent::{Session, Ui}` |
| **mcp**   | `src/mcp/`      | M5        | `tools::Tool`, `mcp::StdioServer` |

## Rules for every stream

1. Edit **only** files inside your directory. Do **not** touch `main.rs`,
   `Cargo.toml`, `Cargo.lock`, or another stream's directory.
2. All dependencies you need are already in `Cargo.toml` (`regex`, `ignore`,
   `globset`, `crossterm`, `serde_json`, `anyhow`). If you think you need
   another, stop and report it — don't add it.
3. Program against the **contracts** documented at the top of your module file.
   Their public signatures are frozen; if one is wrong, report it rather than
   changing it (a change there ripples into other streams).
4. Keep it green **within your scope**: `cargo build`, `cargo clippy -- -D
   warnings`, `cargo fmt --check`, and `cargo test` for your module must pass.
5. A module-level `#![allow(dead_code)]` is present in the stub modules while the
   contract is only partly wired; leave it unless everything is used.
6. Commit your work to a branch named `feat/<stream>` (e.g. `feat/tools`).

## Integration

The spine owner merges each `feat/<stream>` branch, wires the module into `main`
(register built-in + MCP tools, install the TUI, install context providers),
and verifies the whole tree builds green. Because directories are disjoint,
merges are expected to be conflict-free.

## The contracts, in one place

- **tools** — `Tool` trait (`name`, `spec`, `call`), `ToolRegistry`, `ToolCtx`
  (`project_root`, `fstate`, `resolve`), `FileState`, and `builtin_registry()`.
  Every fs/exec tool sandboxes via `ToolCtx::resolve` and records reads via
  `FileState` so `Edit` can reject stale writes.
- **context** — `ContextProvider` (`name`, `gather(root) -> Option<ContextItem>`)
  and `default_providers()`.
- **tui** — `run(Session) -> Result<()>`, plus a `TuiUi` implementing
  `agent::Ui` (`reasoning`, `content`, `tool_start`, `tool_end`, `turn_end`,
  `notice`).
- **mcp** — `connect_stdio(&StdioServer) -> Result<Vec<Box<dyn Tool>>>`.
