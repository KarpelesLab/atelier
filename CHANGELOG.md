# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.3](https://github.com/KarpelesLab/atelier/compare/v0.1.2...v0.1.3) - 2026-11-14

### Added

- atelier is a library its program consumes
- serve --listen — the protocol on a Unix socket or loopback, a session per connection
- serve — queries that answer with data where a slash command prints
- settings over the protocol — settings/get and settings/set
- serve — session/history and edits as diffs (event/file_change)
- a model on this machine — atelier --local runs oxinfer
- pre_tool hooks see a command file's shell lines before they run

### Fixed

- a slow local model no longer times out while it reads the prompt

### Other

- the 27B at 3 bits, for machines with less memory
- roadmap — M24, a backend for other interfaces
- the local model — why the 27B, and that smaller ones do poorly
- wait for the client to read the server's status in the symbols test

## [0.1.2](https://github.com/KarpelesLab/atelier/compare/v0.1.1...v0.1.2) - 2026-11-13

### Added

- symbols — code navigation through a language server, or the text
- an approval reviewer — a model answers the prompts nobody can (M17)
- atelier acp — an Agent Client Protocol agent, for editors (M21)
- /loop — re-send a prompt at an interval while the session is idle
- advisory locks — instances say which paths they are working on
- an idle instance can be woken by its peers (opt-in)
- peer_ask — put a question to another instance's agent and get its answer
- atelier mcp — atelier's tools as an MCP server over stdio (M20)
- code mode — node scripts call the session's tools (M20)
- a GitHub Action — review pull requests, run prompts, answer @atelier
- worktree isolation for subagents — isolation: worktree (M18)
- background subagents — task with background: true (M18)
- model: in a command file's front matter, for that turn only
- MCP prompts as slash commands — /mcp:<server>:<prompt>
- [instructions] files — instruction files named in the settings
- /init — have the agent write the repository's AGENTS.md
- atelier serve — a protocol to embed atelier, JSON-RPC over stdio (M21)
- project trust — atelier.toml runs and grants nothing until accepted (M17)
- sandboxed bash on nixvm, behind an opt-in `sandbox` feature
- /goal — keep working until a stated condition holds
- atelier doctor — a checklist of the setup
- atelier debug prompt — print the first request without sending it
- atelier review — a read-only review of a change
- a finished background job announces itself (M20)
- read-only tool calls of one model step run in parallel (M20)
- OAuth sign-in for remote MCP servers — /mcp login, /mcp logout (M20)
- deferred MCP tools — mcp_search and mcp_call past a spec budget (M20)
- connect configured MCP servers concurrently (M20)
- keep MCP tool annotations and show them in /tools (M20)
- pass MCP server instructions to the model as standing context (M20)
- MCP per-server tool filters, timeouts and enabled = false (M20)
- usage log, atelier stats, /usage
- named providers, a model per role, request variants
- permission modes — plan, ask-edits, auto-edit, yolo (M17)
- subagents — the `task` tool, agents as files, the step cap (M18)
- /context — where the context window goes
- compaction in stages — prune tool output, hand-off summary, /compact, retry on overflow
- save tool output past the budget to a file, return head and tail
- ask tool, a structured question answered on the input line
- hooks — user-configured commands on lifecycle events (M19)
- skills — SKILL.md directories, the `skill` tool, `/skills` (M19)
- custom slash commands from Markdown files (M19)
- permission rules — allow / ask / deny patterns per call (M17)
- web_search tool with searxng, brave, tavily and duckduckgo providers
- diagnostics for every language, reporting only what changed
- machine-usable --print (JSON output, stream input, step limit, schema)
- load all instruction files, with imports, a byte budget and nested files on demand
- durable memory — `memory` tool, ambient index, `/memory` (M15)

### Fixed

- coordinator queues an answer under the lock its request ran under
- a subagent runs in its parent's current mode, and isn't compacted at its end
- never leave two user messages in a row in the conversation
- /trust revoke withdraws MCP instructions, prompts and deferral too
- gate a project's [sandbox] enabled behind project trust
- gate a project's [search] url — the user's search key was sent to it
- a command file's shell lines ask even while the sandbox is on
- permission rules and modes govern claude's built-in tools
- confine instruction imports to the repository; keep the memory index in every request

### Other

- roadmap — M15–M22 status after the last merges
- a newer push cancels the older run on the same branch
- bring configuration, tools, hooks and protocol in line with the code
- fix two races in the peer-message and background-job tests
- roadmap — status of M15–M22
- keep the polled background job running until it is polled
- a cache-stable prompt — constant system message, context with the user turn
- roadmap — drop M16, sandbox on nixvm
- roadmap M15–M22, from a review against Claude Code, opencode and Codex

## [0.1.1](https://github.com/KarpelesLab/atelier/compare/v0.1.0...v0.1.1) - 2026-11-13

### Other

- drive releases with release-plz
- fullrust x86_64 build, gzip-compressed assets
- atelier's own terminal layer replaces crossterm
- Build without the TUI on fullrust (libc-free static Linux)
- a daemon connecting every atelier on the machine
- Claude backend: messages typed mid-turn join the running turn
- v0.1.0 released
