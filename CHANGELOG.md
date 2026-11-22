# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.3](https://github.com/KarpelesLab/atelier/compare/v0.1.2...v0.1.3) - 2026-11-22

### Added

- the coordinator runs the local model's server, each atelier loads its own model, and oxinfer updates itself
- the sandbox fetches its root image, keeps installed packages, and gives a guest a stdin and an environment
- the user-global atelier.toml lies under the project's, table by table
- acp runs a session in another directory beside the first, and claims it
- the local model is chosen by what the machine can run, and oxinfer keeps itself up to date
- node's fs answers to Node's Sync names and callbacks
- the pre-flight says it is asking before the prompt waits on it
- tool calls a model writes as text are run, not shown
- claude's subagents show what they do, and /models lists what claude offers
- the sandbox shows the guest what is kept for the project outside it, read-only
- atelier writes nothing in the project — its state moves to a per-user directory
- node gets fs.promises, top-level await, and outside paths by approval
- a complex bash command's prompt carries the model's reading of it
- with the sandbox on, diagnostics checkers run in the guest
- with the sandbox on, the project's hooks run in the guest
- a command in the sandbox's guest can be given an environment and an input
- the coordinator says who owns a session — no lock file, nothing in the project
- remote control — a second terminal on the session that is running
- the terminal interface shares its live session with remotes on its socket
- one owner per project session — a second atelier is told, and may take it over
- bash — a command the user can type to, on a terminal of its own
- bash — tail, head and grep as parameters, and kept output to look at again
- the compaction threshold follows the model's context window
- subagents as coordinator instances — task's process: true
- the sandbox reaches the hosts it is allowed — [sandbox] network, by name
- git works in a linked worktree and in a submodule in the sandbox
- background jobs and command-file shell lines run in the sandbox
- the sandbox on nixvm 0.0.7 — git commit, cargo and make -j work in the guest
- atelier is a library its program consumes
- serve --listen — the protocol on a Unix socket or loopback, a session per connection
- serve — queries that answer with data where a slash command prints
- settings over the protocol — settings/get and settings/set
- serve — session/history and edits as diffs (event/file_change)
- a model on this machine — atelier --local runs oxinfer
- pre_tool hooks see a command file's shell lines before they run

### Fixed

- a coordinator whose socket is gone stops, instead of waiting for a connection that cannot come
- /agents stop reaches a process subagent at once, not at its parent's next look
- bash — the status line after the rebase, and a test that raced a job's two streams
- symbols — search with rust-analyzer finds functions, not types alone
- /mcp lists the servers the client attached to the session
- symbols — search on a server just started, the project's symbols only, references less the declaration
- symbols — a server's error without its stack trace, and time to settle after a file is opened
- a slow local model no longer times out while it reads the prompt

### Other

- the roadmap records the coordinator running the local model's server
- the local model's server is the coordinator's, with a model per session
- the roadmap records the sandbox in the release binary, on nixvm 0.0.9
- nixvm 0.0.9 from crates.io, in place of its master by revision
- the sandbox in the release binary, its fetched root image, and what nixvm's master settled
- the sandbox compiles for fullrust, and the x86-64 release binary is built with it
- the sandbox's guest is nixvm's Sandbox::execute, and its read-only paths nixvm's
- TEMPORARY — nixvm from its master by git revision, until 0.0.9 is released
- open decision 5 settled — the user-global file under the project's
- M14 acp claims, take-over tests, claim cost
- a take-over across a coordinator restart, and what the claim costs a start
- M23 model choice and oxinfer updates
- open decision 4 was settled by M20's parallel reads
- M7 text tool-call dialects; M7 done
- Claude backend subagent activity and /models
- the roadmap follows the state directory and what now runs in the guest
- where a project's state is kept, now that it is not in the project
- M8 async fs and approved outside paths
- M9 pre-flight summary
- a closure handed to map_err as itself, as clippy 1.99 asks
- what runs in the sandbox and what stays on the host, and why
- mark built milestones, record settled decisions
- the roadmap records one session per directory, held by the coordinator
- roadmap — what landed on 2026-11-17, and what is next
- the sandbox on nixvm 0.0.8
- web_search — SearXNG as run against a real instance
- claude — the live tests as run on 2026-11-17, and what they do not reach
- acp — what was tried against the protocol's own client library
- symbols against real language servers, opt-in, and what they showed
- the prompt cache's benefit, measured against a live endpoint
- roadmap — the sandbox after nixvm 0.0.7, and what is next
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
