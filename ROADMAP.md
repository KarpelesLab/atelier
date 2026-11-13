# atelier — Roadmap

An AI coding harness written in Rust. An agent loop that talks to
OpenAI-compatible model APIs, runs tools in a project directory, speaks MCP,
and continuously feeds itself context (git status, diagnostics, …) — driven
from a deliberately minimal terminal interface.

Status: **M0–M14 done** (§3); **M15–M22** (§3b) are next. This document is
the plan of record; it changes as we build.

---

## 1. Principles

- **Minimal surface, append-only output.** The interface is a single input
  line with a status strip (active model, cwd, token/cost counters). Assistant
  and tool output is *printed to the terminal scrollback and never redrawn*.
  No panes, no mouse, no alt-screen. What scrolls past is history.
- **Bring-your-own API.** We target OpenAI-compatible endpoints by default.
  Subscription backends are only allowed where the vendor *sanctions* it: we do
  **not** reach Claude/ChatGPT subscriptions through unofficial means (ToS), but
  OpenAI's official **Sign in with ChatGPT** (SIWC) open-source OAuth is
  supported (`/login`) — it runs on the user's own ChatGPT plan via the
  Responses API. Separately, atelier can act as a frontend for the user's own
  installed Claude Code CLI ([docs/claude.md](docs/claude.md)): the
  unmodified `claude` binary does its own auth and model calls. First-class
  target is our own server at `http://192.168.0.50:11400/v1` (currently serving
  `qwen3.8-unc:q4`, `qwen3-coder:30b`, `gpt-oss:120b`, `qwen3.8:27b`,
  `qwen3:0.6b`, …). **Default test/dev model: `qwen3.8-unc:q4`** — it is
  thinking-capable and vision-capable (reads images), so the provider layer must
  handle a reasoning channel and multimodal input from day one.
- **Project-scoped.** atelier is launched inside a project directory and treats
  it as the root. Tools, context, and permissions are anchored there.
- **The agent is fed, not just prompted.** A pipeline of "helpers" injects fresh
  context each turn (git status, recent diffs, build/lint diagnostics, …) so the
  model reasons about the *current* state, not a stale snapshot.
- **Tools first, then everything.** A solid, well-typed local tool set is the
  foundation. MCP extends it; it does not replace it.
- **Boring, legible core.** Prefer explicit state machines and plain data over
  cleverness. The agent loop should be readable end to end.

---

## 2. Architecture (target shape)

**One binary crate, organized by module.** No internal workspace — atelier is a
single application, not a library ecosystem, so we avoid the path-dep / version /
circular-dep friction of many small crates. The engine stays usable without the
TUI through module discipline (nothing in `core`/`provider` imports `tui`) plus a
`tui` feature flag for headless builds — not through crate boundaries. Any module
whose seam proves stable can be extracted into its own crate later; merging a
premature split back is the expensive direction, so we split late, not early.

```
atelier/
├─ src/
│  ├─ main.rs        # entry: arg parse, config load, wiring, run loop
│  ├─ config.rs      # atelier.toml + env/flag overlay
│  ├─ provider/      # OpenAI-compatible client: chat, streaming, tool-calls
│  ├─ agent/         # agent loop, conversation state, turn orchestration
│  ├─ tools/         # built-in tools + tool registry/trait
│  ├─ mcp/           # MCP client (stdio + HTTP/SSE transports)
│  ├─ context/       # "helpers": context providers (git, diagnostics, …)
│  └─ tui/           # minimal inline input + status strip; stream renderer  [feature = "tui"]
├─ tests/            # integration + scripted-agent regression harness
├─ ROADMAP.md
└─ Cargo.toml        # single package
```

**Core data flow (one turn):**

```
user input ─▶ context helpers gather ─▶ assemble messages ─▶ provider (stream)
      ▲                                                          │
      │                                              ┌───────────┴───────────┐
      │                                              │ text deltas   tool calls
      └────────── loop until no tool calls ◀── tool results ◀── execute (w/ perms)
```

**Proposed stack**

| Concern         | Choice (initial)                          | Notes |
|-----------------|-------------------------------------------|-------|
| HTTP            | **`rsurl`** (ours, pure-Rust curl)         | `Request::send_reader()` gives a blocking `Read`+`.status()` — ideal for SSE |
| Concurrency     | `std::thread` + channels (defer `tokio`)   | rsurl's primary API is blocking; a worker thread streams into the UI. Add an async runtime only if concurrency demands it |
| JSON            | `serde` / `serde_json`                     | provider + MCP + config |
| Terminal        | own layer (`src/tui/term.rs`)              | raw mode, keys, size via `ioctl`; no libc needed, so the TUI builds on fullrust |
| Line editing    | `reedline` (evaluate) or hand-rolled       | history, editing; must coexist with streamed output above it |
| Errors          | `thiserror` (libs) / `anyhow` (bin)        | |
| Config          | `toml` + `serde`, `figment`/env overlay    | `atelier.toml` per project + user global |
| Logging         | `tracing` → file (not stdout)              | stdout is reserved for the conversation |

Decisions marked "evaluate" are revisited at the milestone that needs them.

---

## 3. Milestones

Each milestone is shippable and independently demoable. Exit criteria are the
bar for "done".

### M0 — Skeleton & provider spike
Prove we can hold a streaming conversation with the real endpoint.

- Workspace + crates scaffolded; CI (fmt, clippy, test) green.
- `atelier-provider`: chat-completions client with **streaming** (SSE) and
  non-streaming fallback; `GET /models`; typed request/response.
- **Reasoning channel:** parse and separate "thinking" content from final
  answer (`qwen3.8-unc:q4` emits it) — kept on its own channel, collapsible in
  the UI, and **never fed back to the model as if it were assistant output**.
- **Multimodal input:** message content supports image parts (data-URL / file)
  so vision-capable models can read images; text-only path stays the default.
- Throwaway CLI: read a line from stdin, stream the reply to stdout.
- Config loading: endpoint URL, API key (env), default model
  (`qwen3.8-unc:q4`).

**Exit:** `atelier` streams a coherent multi-turn chat against
`192.168.0.50:11400` with a selectable model, with thinking rendered separately
from the answer. No tools yet.

### M1 — Agent loop & tool calling
Turn the chat into an agent.

- `atelier-core`: conversation state, turn orchestration, the **tool-call loop**
  (model → tool_calls → execute → feed results → repeat until done).
- `atelier-tools`: `Tool` trait + registry; JSON-schema tool advertisement in
  the provider request; robust parse of streamed/partial tool-call arguments.
- First tools: **Read**, **Write**, **Edit** (exact-match string replace),
  **Bash** (with timeout), **Grep**, **Glob**, **LS**.
- All filesystem/exec tools are **sandboxed to the project root** by default;
  path traversal outside root is rejected.

**Exit:** the agent can complete a real task ("read X, change Y, run the test")
end to end via tool calls against a tool-capable model (`qwen3-coder:30b`).

### M2 — Minimal TUI
The intended interface.

- `atelier-tui`: single input line + status strip (model · cwd · git branch ·
  token/turn counters). Input row redraws in place; **everything else prints to
  scrollback and is never touched again**.
- Streamed assistant text and tool activity render above the live input without
  corrupting it (line-discipline / cursor management is the hard part here).
- ✅ Interrupt (Ctrl-C) stops the in-flight turn (next streamed chunk or
  between tool calls; claude's background agents are never stopped by it);
  Ctrl-D exits.
- Slash commands: `/model`, `/help`, `/clear`, `/quit` to start.
- ✅ **Working state.** A turn runs on a worker thread while the main thread
  keeps the keyboard: the status strip shows a spinner + elapsed time
  (`⠋ working 12s`), and the input line stays live. Enter *queues* a message
  (`N queued` in the strip); queued prompts are delivered to the agent
  between steps, and whatever is left when the turn ends runs in order.

**Exit:** normal daily use happens through the TUI; resizing and long output
don't garble the input line.

### M3 — Permissions & safety ✅
Make tool execution trustworthy — approval keyed on *confinement*, not
side-effects.

- **Confined to the project dir = automatic.** All file tools are sandboxed to
  the project root and can't escape, so they run without prompting — including
  `write`/`edit`. Operating inside the project is the whole point.
- **Unconfined = ask.** Only tools that run arbitrary, unbounded code — `bash`
  and MCP tools — prompt: once / always (persisted to `atelier.toml`) / deny
  (the model is told and adapts).
- Headless mode respects a policy (`ATELIER_APPROVE=all`); no silent blocking.
- The frontier for unconfined execution is M8 (mediated scripting) and M9
  (script analysis), which replace or illuminate the coarse yes/no for `bash`.

**Exit (met):** a first-run user approves/denies each unconfined action; a power
user pre-authorizes with "always"; in-project edits never nag.

### M4 — Context helpers
The "feed the agent" pipeline.

- `atelier-context`: a `ContextProvider` trait; providers run per turn (budgeted)
  and inject compact, structured context.
- Initial providers: **git status/branch**, **recent diff**, **project layout**,
  and a pluggable **diagnostics** hook (build/lint/test output → summarized).
- Token budgeting ✅: providers are prioritized and truncated to a context
  budget (`context::render_budgeted`), and unchanged context is deduped across
  turns (`context::dedup_items`) so identical git/layout isn't re-sent — both
  wired into the agent loop.

**Exit:** the agent visibly reacts to repo state it was never explicitly told
about (e.g. notices uncommitted changes, a failing build).

### M5 — MCP support
Extend the tool surface via the ecosystem.

- MCP client — **stdio** ✅ and **Streamable HTTP** ✅ transports (shared
  `JsonRpc` trait; HTTP does POST + JSON/SSE responses + `Mcp-Session-Id`), both
  wired into `atelier.toml` (`[[mcp]]` / `[[mcp_http]]`) and `/mcp add`
  (a `http(s)://` second arg selects HTTP), connected at startup. ✅
- Server config in `atelier.toml`; discovered MCP tools are merged into the tool
  registry and namespaced (`mcp__<server>__<tool>`).
- MCP tools flow through the same permission model as local tools.
- MCP **resources** ✅: a server that advertises resources gets a
  `mcp__<server>__read_resource` tool (lists URIs, reads by `uri`), and
  **prompts** ✅ as a `mcp__<server>__get_prompt` tool.

**Exit:** a configured MCP server's tools are callable in a task, indistinguishable
from built-in tools to the agent.

### M6 — Session & context management
Make long sessions durable.

- ✅ Persist conversation to `.atelier/session.json` after each turn; resume
  with `--continue`; `/new` starts fresh. Verified: a fact stated in one run is
  recalled after `--continue`.
- ✅ Cost/usage accounting surfaced (TUI status strip + REPL line).
- ✅ **Compaction/summarization**: when the last request's total tokens exceed
  `ATELIER_CONTEXT_LIMIT` (default 8000), older history is summarized into a
  rolling summary (folded into the system message) and dropped, split at a
  user-message boundary so no tool result is orphaned.

**Exit (met):** a session survives restarts and compacts older turns as it
approaches the context limit instead of hard-failing.

Also landed alongside M6: **vision input** (`/image <path>` attaches an image to
the next message — OpenAI multimodal content parts) and a **`multiedit`** tool
(atomic multi-edit of one file).

### M7 — Hardening & polish
- Provider quirks: per-model tool-calling dialects, missing SSE fields, retries,
  backoff, timeouts.
- Config UX, better `/` commands. `--print`/`-p` pipe-friendly headless mode ✅
  (one-shot prompt from args or stdin; answer on stdout, everything else stderr).
- Docs: quickstart, config reference, tool reference, MCP setup.

**Exit:** stable enough for daily driving on real work.

### M8 — Scripting via a mediated JS runtime (`Node` tool)

Give the agent a real scripting surface that is *safer* than raw `bash`, by
running JavaScript on our own engine ([kataan](https://github.com/KarpelesLab/kataan),
pure-Rust) with **host APIs we implement and mediate**. The engine core is
sans-I/O, so we provide `fs`, `fetch`, etc. — and check every effect at runtime.

- A `Node`/`js` tool that evaluates a script in an embedded kataan runtime.
- A Node-compatible **`fs`** whose every path is resolved and checked against the
  project root at call time: in-project reads/writes are automatic; anything
  outside the root (or network via `fetch`) is gated through the same approval
  path as `bash`. This is the payoff — mediation instead of a coarse yes/no.
- Capability gating per script run (fs / network / env), surfaced to the agent.
- Depends on kataan's native-function / global-injection API (under
  investigation) and its `host`/`fetch`/`crypto` features.

**Why:** scripting lets the agent compose logic (loops, parsing, transforms)
that would otherwise be brittle shell one-liners — and because we own the host,
each capability is inspectable and revocable, unlike `bash`.

**Exit (v1 met):** the agent runs JS through the `node` tool with a synchronous,
project-confined `fs` (readFile/writeFile/readdir/exists/mkdir) + captured
`console`; out-of-project paths throw, and there is no network. Verified live
(the model wrote/read a file and summed it).

**M8.1 — progress & follow-ups:**
- ✅ Wall-clock timeout (`timeout_ms`, default 5s): runs kataan on a worker
  thread and aborts at the deadline; the harness stays responsive.
- ✅ Gated `fetch`/`httpGet`/`httpRequest` (sync, via rsurl), installed only on
  `network: true`, which is treated as unconfined and requires approval
  (per-call `Tool::requires_approval(args)`).
- ✅ **True CPU stop** (kataan 0.0.9): the worker installs kataan's cooperative
  `Interrupt` (an `Arc<AtomicBool>`); on timeout the main thread trips it, the
  interpreter aborts on the next loop back-edge, and the worker is reaped — a
  runaway `while(true){}` is actually halted, not abandoned. (A pathological
  script with no back-edge to observe the flag is still abandoned after a short
  grace rather than blocking the harness.)
- ✅ Expanded mediated `fs`: stat/appendFile/rm/rmdir/rename + binary
  (`readFileBytes`/`writeFileBytes` via a real `Uint8Array`).
- ⏳ async `fs`; out-of-project fs via approval.

### M9 — Safer command execution (script analysis)

`bash` (and any future `python`) runs opaque, unconfined code. Beyond a yes/no
prompt, analyze what a script will do before running it.

- Surface risk signals in the approval prompt (network access, `sudo`, `rm -rf`,
  writes outside the project, pipes to a shell).
- Optionally a model- or rule-based pre-flight summary of a complex script.
- Prefer steering the agent toward the mediated `Node` tool (M8) for anything
  that can be expressed as script rather than raw shell.

**Exit (v1 met):** a risky `bash` command is flagged with *why* at the prompt
(rm -rf, sudo, `curl|sh`, out-of-project paths, force-push, …), not just an
opaque string. Verified live. A model-based pre-flight summary is a later add.

### M10 — Subconscious reviewer & settings screen ✅

- ✅ **Parallel "subconscious" reviewer.** With review mode on (`/review on`,
  `[review] enabled`, or `ATELIER_REVIEW=on`), the loop spawns a read-only
  reviewer on a worker thread after each tool batch; it watches the exchange +
  edits and can do nothing but post short `💭` notes into the main dialog (via
  the `Ui::subconscious` channel), drained between steps and at turn end.
  Verified live — it flagged a missing `chmod +x` and warned about an
  `rm -rf /`.
- ✅ **`/config` settings screen.** `Dispatch::Config` opens a full-screen
  alternate-screen editor in the TUI (model, auto-approve, context limit,
  review mode; save & exit, alt screen always restored); the REPL prints a text
  summary. Backed by `[defaults]`/`[review]` in `atelier.toml`.

### M11 — Sign in with ChatGPT (SIWC) ✅ (pending live validation)

OpenAI's sanctioned open-source OAuth, so a user can run on their **own ChatGPT
plan** without an API key — a second, separately-sanctioned backend alongside
bring-your-own-API.

- ✅ **`auth`**: PKCE (S256) authorization-code flow — loopback `127.0.0.1/callback`
  listener, browser open, token exchange + rotating refresh; tokens stored
  `$XDG_CONFIG_HOME/atelier/chatgpt-auth.json` (0600), restored at startup.
  Crypto via **purecrypto**.
- ✅ **Responses API backend** (`provider::responses`): the ChatGPT-plan token is
  only valid for `/v1/responses`, so this maps our `Message`/`ToolSpec`/
  `Completion` onto the Responses request + typed `response.*` SSE events.
- ✅ **Wiring**: `/login`/`/logout`; `send()` routes to Responses when signed in,
  else chat/completions; `/model` + `/config` + status show the backend.
- ⏳ **Live validation** needs a real ChatGPT account (the OAuth round-trip and
  the available model ids can't be exercised from CI). Default model `gpt-5`,
  changeable with `/model`.

### M12 — Claude backend ✅

atelier as a frontend for the user's own installed Claude Code CLI
([docs/claude.md](docs/claude.md)): the unmodified `claude` binary keeps its
own auth, agent loop and tools; atelier supplies the interface, its MCP
servers, and per-turn context.

- ✅ **Bridge** (`src/claude`): one long-lived `claude -p` stream-json process
  per conversation; text/thinking/tool/permission events rendered through `Ui`;
  `can_use_tool` answered by atelier's approval prompt; `--mcp-config` from
  `atelier.toml`; session id kept for `--continue`; `/model` relaunches with
  `--model`. Turns tracked by message uuid (`command_lifecycle`).
- ✅ **Ctrl-C without collateral damage**: preempts the turn (`interrupt` +
  `send_now`) instead of claude's plain interrupt, which kills background
  agents. Background work is never ended unless explicitly requested (`/new
  force`, repeated quit); claude's own reports on it render while idle.
  Live-tested: `cargo test live_stop -- --ignored`.
- ✅ **atelier's tools for claude**: `atelier mcp-serve <socket>` is the MCP
  server claude launches — a token-authenticated relay over a private Unix
  socket back to the atelier process, which answers MCP and runs each call
  with the session's tools, read tracking and approval policy (claude allows
  `mcp__atelier` wholesale; atelier decides). Live-tested:
  `cargo test --test claude_live -- --ignored`.
- ✅ **atelier's tools replace claude's** by default (`--disallowedTools`;
  `ATELIER_CLAUDE_BUILTINS=keep` opts out), with parity where claude's were
  stronger: `read` does images, PDFs (pure-Rust `oxideav-pdf`), notebooks
  and (approved) outside paths;
  `bash` background jobs + `bash_output`/`bash_kill` cover Monitor;
  `notebook_edit`; `web_fetch` converts HTML and pages. Tools are marked
  `anthropic/alwaysLoad` so claude doesn't defer them.
- ✅ Messages typed mid-turn join the running claude turn (handed over
  while a tool runs; claude delivers them after the tool results).


### M13 — Releases & self-update ✅

- ✅ **Release workflow**: release-plz (`release-plz.yml`) keeps a release PR
  open and, once it is merged, tags `vX.Y.Z` and drafts the GitHub Release;
  `release-binaries.yml` then builds static binaries natively on x86_64
  (with [fullrust](https://github.com/KarpelesLab/fullrust): libc-free) and
  arm64 (musl, until fullrust targets it) runners, smoke-tests them, and
  attaches them gzip-compressed (`atelier-<target>.gz`) with `SHA256SUMS`, and
  publishes the release.
- ✅ **Self-update** (`src/update.rs`): official builds (marked by
  `ATELIER_RELEASE_TARGET` at compile time) check the latest release daily
  in the background, verify the checksum, decompress (pure-Rust `compcol`),
  test-run the new binary, and
  rename it over the old one for the next launch; `atelier update` on
  demand, `ATELIER_NO_UPDATE` to opt out. Builds from source never update.
- ✅ First release (`v0.1.0`): x86_64 and aarch64 static binaries with `SHA256SUMS`
  (musl, uncompressed — the asset names changed after it, so it doesn't
  update itself to later releases).

### M14 — Coordinator: instances connected ✅ (v1)

A daemon that connects every atelier running on the machine
([docs/coordinator.md](docs/coordinator.md), `src/coord`).

- ✅ **Self-managing daemon**: `atelier coordinator`, launched detached by
  the first instance up; a held file lock keeps it unique. Instances
  reconnect on their own — and relaunch it — whenever it ends (crash,
  kill, idle exit); it steps down when its executable is replaced or a
  newer atelier connects, so updates take over without intervention.
- ✅ **Schemes** over one JSON-RPC socket: presence (`list`/`update`),
  direct and broadcast messages, topics (subscribe/publish), and
  instance-to-instance `call` (request/response, with deadline).
- ✅ **Surfaces**: `peers`/`peer_send` tools, `/peers`, `/send`,
  `/subscribe`; peers' messages are shown and reach the model mid-turn or
  with the next prompt, marked as another agent's.
- ⏳ Continued in M18 (subagents ride the coordinator): waking an idle
  instance on a message, `call` handlers that reach the agent, advisory
  locks. Also open: a non-Unix transport.
---

## 3b. Next — the rest of what a harness does

M0–M14 built the loop, the tools and the plumbing. What follows comes from
going through what Claude Code, opencode and Codex CLI do for the agent and
the user *besides drawing a screen*, and keeping what earns its place. The
interface stays what §1 says: an input line, a status strip, and scrollback.
Every item below must work through plain printed output, slash commands,
files on disk and the headless protocol — none needs a pane, a picker
overlay or a mouse.

### Where atelier stands

| Capability | Claude Code | opencode | Codex CLI | atelier today | Planned |
|---|---|---|---|---|---|
| Instruction files | `CLAUDE.md` hierarchy, `@` imports, path-scoped rules | `AGENTS.md` up the tree, nested ones loaded lazily, globs/URLs, live reload | `AGENTS.md` root→cwd, override files, size cap | first of 4 files at the root, cut at 3000 chars | M15 |
| Memory across sessions | agent-written memory files with an index | — | background extract → consolidate, off by default | — | M15 |
| Sessions | many per project, resume by id, fork | SQLite, fork at a message, export/import | named, resume/fork/archive | one per project, `--continue` | no (M16 dropped) |
| Undo of file changes | checkpoints + rewind | snapshot per step in a private git object store, undo/redo | removed; "use git" | — | no — git does it (M16 dropped) |
| Compaction | auto + manual with instructions; old tool output cleared first | cache-aware, structured hand-off, prune tool output, background | auto + manual, custom prompt, hooks | auto summary only | M22 |
| Permission rules | allow/ask/deny patterns per tool and command; modes | one grammar for tools, MCP, skills, subagents; per-agent | Starlark prefix rules with inline tests, `check` CLI | per-tool "always", risk signals | M17 |
| OS sandbox for commands | opt-in (bubblewrap/Seatbelt), network allowlist | — | default: bwrap+seccomp/Seatbelt, proxy with domain allowlist | — (`node` tool is mediated; `bash` isn't) | M17, on nixvm |
| Approvals without the user | — | — | reviewer agent with a policy, fails closed | `ATELIER_APPROVE=all` | M17 |
| Plan / read-only mode | plan mode | `plan` agent | read-only sandbox | — | M17 |
| Subagents | task tool, custom agents (Markdown), parallel, background, worktrees | task tool, Markdown agents, step cap, background | spawn/wait/message, roles, TOML agents | reviewer only; peers via coordinator | M18 |
| Custom commands | Markdown, arguments, shell output | same, plus per-command agent/model | (skills) | — | M19 |
| Skills | `SKILL.md`, loaded on demand | same, also reads `.claude/skills` | same, `.agents/skills` | — | M19 |
| Hooks | pre/post tool, prompt, stop, session, compact | plugin events | same set + permission request, hash-pinned trust | — | M19 |
| Ask the user | structured question tool | `question` tool | `request_user_input` | — | M20 |
| Web search | yes | yes (several providers) | yes | `web_fetch` only | M20 |
| Diagnostics after edits | LSP tool, IDE diagnostics | LSP in v1, **dropped in v2** for lint/typecheck commands | — | `cargo check` only | M20 |
| Parallel tool calls | yes | yes | yes | sequential | M20 |
| MCP extras | OAuth, resources, prompts as commands, deferred tools, serve as MCP | OAuth, prompts as commands, "code mode" | OAuth, tool filters, deferred tools, serve as MCP | stdio+HTTP, resources, prompts | M20 |
| Headless / embedding | `-p`, JSON stream in/out, SDK | `run --format json`, HTTP server, ACP | `exec --json`, output schema, app-server | `--print` (text) | M21 |
| Scheduled / CI work | loops, cron, GitHub action | GitHub action | cloud tasks, GitHub review | — | M21 |
| Model per role, effort | per subagent, effort levels | per agent/command, variants, small model | per role, effort, review model | one model (+ reviewer's) | M22 |
| Prompt-cache care | automatic | summary reuses the cache, cache warming | — | fresh context rewrites the system message every turn | M22 |
| Instances that talk | agent teams | — | message board (in development) | ✅ coordinator (M14) | M18 |

Already ahead or equal, and to be kept that way: file-state tracking and
stale-edit rejection, ambient per-turn context with dedup, the mediated `node`
tool, mid-turn steering, the subconscious reviewer, three sanctioned backends,
the coordinator, a libc-free static binary.

### M15 — Instructions & memory

What the agent knows before the first prompt, and what it keeps afterwards.

- **Instruction files, properly.** Load *all* that apply instead of the first
  found: user-global (`~/.config/atelier/AGENTS.md`), then every `AGENTS.md`
  from the repository root down to the working directory, concatenated
  root→leaf (`CLAUDE.md` and `.cursorrules` stay as fallbacks per directory).
  A real byte budget (Codex: 32 KiB) replaces the 3000-char cut, and
  truncation says so.
- **Nested instructions on demand** (opencode v2): an `AGENTS.md` in a
  subdirectory is injected the first time the agent reads or edits under it,
  once. Changed instruction files are re-injected as an update before the next
  request rather than waiting for a restart.
- **`@path` imports** and extra instruction globs from `atelier.toml`.
- **Memory.** A per-project directory of small Markdown notes with a one-line
  index, written and pruned by the agent through a `memory` tool, and read
  back as ambient context (index always; notes when relevant). Typed entries —
  about the user, corrections the user gave, project facts not in the code,
  pointers to outside resources. `/memory` lists, opens the directory, forgets.
  Explicitly *not* a log of completed work.
- **Guard rails** (from Codex): nothing is memorized from sessions whose
  context held untrusted outside content (web pages, MCP output) unless the
  user asks; secrets are redacted; memory is plain files the user can read,
  edit and commit or ignore.
- `/init` writes a first `AGENTS.md` from the repository.

**Exit:** a correction given in one session changes behaviour in the next
without being repeated; a subdirectory's rules apply only when working there.

### M16 — dropped

Was: many sessions per project with fork, and checkpoints with
`/undo`/`/rewind` of file edits. Dropped: nearly every project already has
git, which does this job better, and private snapshot and session stores
grow without restraint. One saved session per project (`--continue`) stays.
What the milestone said about compaction and `/context` moved to M22.

### M17 — Permission rules and a real sandbox

Today's model is one bit per tool. Codex shows how far the other end goes.

- **Rules.** `[permissions]` grows `allow` / `ask` / `deny` lists of patterns
  over tool name and arguments: `bash(git diff *)`, `bash(cargo *)`,
  `mcp__github__*`, `read(~/.ssh/**)`. Deny wins, then ask, then allow.
  A compound shell command is split and every part must pass; one that can't
  be parsed is never auto-allowed. "Always" at the prompt proposes the
  narrowest useful pattern, not the whole tool. User-global rules sit under
  project rules; `atelier permissions check -- <command>` explains a decision.
- **Modes.** `plan` (read-only tools; the agent investigates and proposes,
  edits are refused with a reason it can act on), `default`, `auto-edit`
  (today's behaviour), `yolo` (today's `ATELIER_APPROVE=all`). `/mode`, and
  the status strip shows the current one.
- **Sandboxed `bash`, on nixvm.** Commands run inside
  [nixvm](https://github.com/KarpelesLab/nixvm), our own sandbox: a real
  Linux userland whose syscalls are served by nixvm's kernel, written in
  Rust, instead of the host's. The guest sees the project directory
  (writable, with `.git/hooks` and `atelier.toml` protected) and a userland
  image, and nothing else of the host. Being a library with no OS
  dependencies, it is the same sandbox on every platform and in the
  libc-free build — no `bwrap`, namespaces or Seatbelt to depend on. A
  command that fails *because of* the sandbox is reported as such, and the
  agent may ask to rerun it on the host with a stated reason — that request
  is what the user approves. With a sandbox, most commands stop needing
  approval at all.
- **Network by allowlist.** The guest's network is nixvm's in-VM stack, so
  which domains it may reach is decided in atelier, not by a proxy.
- **What this depends on:** nixvm running the toolchains projects actually
  use (compilers, package managers, test runners) at a usable speed, and the
  userland image carrying or reaching the project's tools. Until a command
  is known to work there, it falls back to asking as today.
- **Approvals by a reviewer.** Optionally, would-be prompts go to a reviewer
  model with a written policy (`[permissions] reviewer`), which allows, denies
  or escalates to the user, and fails closed. This is what makes unattended
  and coordinator-driven work safe without `yolo`.
- **Project trust.** A freshly cloned repository's `atelier.toml` (MCP
  servers to launch, hooks, permission rules) is not honoured until the user
  says the project is trusted.

**Exit:** a day's work prompts a handful of times instead of on every command,
and a hostile `curl | sh` in a README can neither write outside the project
nor reach the network.

### M18 — Subagents

- **A `task` tool**: run a named agent on a prompt in a fresh context and get
  its final report. Built-ins: `explore` (read-only search), `general`.
  Several calls in one step run in parallel; the transcript shows one line per
  subagent, its tool activity summarized, never interleaved streams.
- **Agents as files**: `.atelier/agents/<name>.md` (and user-global), with
  front matter for description, model, reasoning effort, tools, permission
  rules and a step cap; the body is the system prompt. Reads `.claude/agents`
  too. At the cap, tools are withdrawn and the agent is told to report what is
  done and what is left (opencode).
- **Background agents**: started with `background: true`, they report when
  finished the way peers' messages arrive today; `/agents` lists the running
  ones, and Ctrl-C never kills them (the rule the Claude backend already
  follows).
- **Isolation**: an agent may run in its own git worktree, so parallel
  writers don't trample each other; its diff comes back for the parent to
  apply.
- **Built on the coordinator.** A subagent is an atelier instance: the same
  routing carries a parent's task, the child's report and messages between
  siblings, whether the child is a thread, a process, or an instance the user
  opened in another terminal. This is where M14's `call` gets its real
  methods (`task`, `ask`), where idle instances may be woken on request, and
  where advisory locks ("I am editing this file") belong.

**Exit:** "look through the codebase for X" costs the main conversation a
paragraph instead of forty file reads; three independent changes proceed at
once without conflicts.

### M19 — Commands, skills, hooks

The three ways a project teaches the harness, all as plain files.

- **Custom slash commands**: `.atelier/commands/<name>.md` → `/name`, with
  `$ARGUMENTS`/`$1`, `` !`command` `` output injection, and optional front
  matter (agent, model, run as a subagent). MCP prompts appear as commands.
- **Skills**: `SKILL.md` directories (`.atelier/skills`, also `.claude/skills`
  and `.agents/skills`, which the other three already share). Only name and
  description are advertised, within a small budget; a `skill` tool loads the
  body and its files when the task calls for it.
- **Hooks**: commands run on events — session start/end, prompt submitted,
  before a tool (may deny or rewrite arguments), after a tool, before/after
  compaction, turn end (may send the agent back to work), subagent start/stop,
  permission request (may answer it). JSON on stdin, decision on stdout.
  Formatters after edits and a notification when a turn ends are just hooks,
  shipped as documented examples rather than features. Hooks from a project
  run only once the user has reviewed them, and again after they change
  (Codex's hash-pinned trust).

**Exit:** a project's conventions (format on save, run tests before stopping,
a `/release` routine) live in its repository and apply to whoever opens it.

### M20 — Tools the others have

- **`ask`**: a structured question to the user — a few options, optionally
  multi-select — printed as numbered lines and answered on the input line.
  Cheaper and clearer than the agent stopping to ask in prose.
- **`web_search`**, behind a provider setting (a search API key, a SearXNG
  URL, or the model provider's own search where it exists); approval like
  `web_fetch`.
- **Diagnostics for every language, the cheap way.** opencode built LSP
  support and then removed it in favour of running the project's own
  lint/typecheck commands. Follow that: `[diagnostics]` commands per file
  pattern, auto-detected for common stacks, run after edits with only the
  *change* in diagnostics reported (§4's edit→verify loop, generalized from
  today's `cargo check`). An LSP-backed `symbols` tool (definition,
  references) is worth having later for navigation, not for diagnostics.
- **Parallel tool calls** for read-only tools in one model step.
- **Tool output to a file**: results past the budget are saved under
  `.atelier/` and the agent gets the head, the tail and the path, instead of
  a truncation it can't see past.
- **MCP**: OAuth for remote servers; per-server tool allow/deny lists and
  timeouts; server instructions passed to the model; elicitation answered
  through `ask`; **deferred tools** — with many servers, advertise names and
  load schemas on demand; or route MCP through the `node` tool ("code mode"),
  which atelier is unusually well placed to do since the JS runtime is
  already mediated. And `atelier mcp-serve` as a public mode, so other
  harnesses can use atelier's tools and sessions.
- **Background commands that notify**: a finished `bash` background job wakes
  the agent rather than waiting to be polled.

### M21 — Headless, protocol, automation

- **`--print` grows up**: `--output-format json|stream-json` (events for
  text, reasoning, tool calls, approvals, usage, final result),
  `--input-format stream-json` for multi-turn driving, `--output-schema` for
  a validated structured answer, `--max-turns`, `--continue`, and exit codes
  that distinguish failure, refusal and limits.
- **A protocol to embed atelier**: the same events as JSON-RPC over stdio
  (and over the coordinator's socket, which is already there) — start, steer
  and interrupt turns, answer approvals, compact. With ACP on top,
  editors get atelier without atelier growing a UI. This is also how any
  other front end stays out of this repository.
- **Goals and loops**: `/goal` keeps the agent going until a stated,
  checkable condition holds, within a token budget; `/loop <interval>
  <prompt>` and scheduled runs through the coordinator, which is the one
  process that outlives every instance.
- **CI**: a GitHub action for review and for `@atelier` in issues and pull
  requests, built on `--print`; `atelier review` (uncommitted, a base branch,
  a commit) as a first-class read-only run using the reviewer that already
  exists.
- `atelier doctor`, and a way to print the exact prompt a turn would send.

### M22 — Models, context and the cache

- **A model per role**: main, subagents, reviewer, a small one for titles,
  summaries and memory upkeep; `/model` variants carrying reasoning effort
  and sampling settings; effort changeable mid-session.
- **Named providers** in `atelier.toml` (several endpoints, each with its
  key source, headers and quirks) instead of one base URL in the environment,
  with a key helper command for rotating credentials.
- **A cache-stable prompt.** Today the per-turn context is folded into the
  system message, so the first bytes of every request change and a server's
  prompt cache — llama.cpp's KV cache as much as a hosted provider's — is
  thrown away each turn. Keep the system message and tool list byte-identical
  for the life of a session, and deliver fresh context as late messages
  that replace their predecessors. On a local 30B model this is the largest
  latency win available.
- **Compaction in stages.** First drop the bodies of old tool results (keep
  one line saying what they were); only then summarize. The summary is a
  structured hand-off (goal, decisions, state of the work, files touched, next
  steps), the recent tail stays verbatim by token count, and `/compact
  [instructions]` does it on demand. The agent's todo list and memory live
  outside the history, so they survive untouched.
- **`/context`** prints where the window goes (system, instructions, context
  items, tools, history) so both user and agent can see what to trim.
- **Cost and usage** per session and per day (`atelier stats`), with prices
  for providers that have them, and budgets that warn before they stop.
- **Telemetry** stays local: a trace file per session that replays (§5).

### Considered and left out

- **A full-screen or paned interface**, pickers, themes, mouse support — the
  point of atelier is not having them (§7).
- **An in-process plugin API or marketplace.** Files (commands, skills,
  agents), hooks, and MCP cover it without loading other people's code into
  the harness.
- **Multiple sessions, fork, and undo of file edits** (the dropped M16):
  git already does it, and such stores only grow.
- **LSP for diagnostics** — see M20; opencode's reversal is the evidence.
- **Remote instruction URLs and organization-managed configuration.** Useful
  to enterprises; revisit if there is one.
- **Hosted "cloud tasks".** The coordinator and CI cover unattended work on
  the user's own machines.

---

## 4. Harness ergonomics — designing for the agent's experience

I run inside a harness like this one, so these are the frictions I actually hit,
turned into features. This is atelier's differentiator: not just "an agent
loop," but a loop tuned so the model wastes fewer turns and stays grounded in
truth. Each item maps to a milestone.

**Grounding & truth**
- **File-state tracking (M1).** Track a hash/mtime of every file the agent reads.
  Reject an `Edit` whose target changed since it was read; tell the agent a read
  file went stale. This kills the "edit → re-read to verify" tax — the harness
  *knows* the write applied, so the agent shouldn't spend a turn confirming it.
- **Precise edit feedback (M1).** When an exact-match `Edit` misses, return the
  closest candidate + line numbers instead of a bare "not found". Most retries
  are whitespace/indent drift; give the agent what it needs to fix in one shot.
- **Tight edit→verify loop (M4).** After an edit, auto-run the relevant checker
  (compiler/LSP/linter) and feed back only the **delta** of diagnostics. The
  agent learns immediately whether it broke something, without asking.

**Context economy**
- **Budgeted, paged tool output (M1).** Truncate large results with a
  `… N more lines (page 2 with …)` affordance instead of dumping a 5k-line log
  into context. Structured, summarizable outputs over raw firehoses.
- **Ambient state, not tool calls (M4).** git status, branch, cwd, project
  layout should arrive as injected context — the agent shouldn't burn turns
  running `git status` / `ls` / `pwd` to discover its own environment.
- **Context deduplication (M4/M6).** Don't re-send unchanged context every turn;
  send diffs. Reserve the window for what changed.
- **Durable memory across compaction (M6).** An agent-maintained scratchpad +
  task list persisted outside the message history, so summarization can't erase
  in-flight intent. What the agent decided survives a compaction.

**Flow & control**
- ✅ **Mid-turn steering (M2).** Let the user inject guidance into a running turn
  without aborting it — queued and surfaced to the agent at the next step.
  *(This very roadmap was steered that way mid-turn; the pattern works.)*
  Landed: `Ui::take_queued` is drained before each model request (i.e. right
  after a batch of tool results) and appended as a user message.
- **Concurrent independent tools (M1/M4).** Run tool calls in parallel when they
  don't depend on each other. Serial execution is pure latency.
- **Batchable permissions (M3).** Pre-authorize classes of safe actions and
  learn common ones, so approval prompts don't chop the agent's momentum into
  one-action-per-interruption.
- **Structured failure, no blind retry (M1/M3).** Every denied/failed action
  returns *why* in a machine-usable form, so the agent adapts instead of
  retrying the same call verbatim.

**Honesty & observability**
- **Verification nudge (M4).** Make "claim done → actually check" cheap and
  default, to counter the agent's bias toward declaring success unverified.
- **Cost/latency visibility (M0/M6).** Surface tokens + time so both agent and
  user can be economical; log raw provider traffic for deterministic replay.

---

## 5. Cross-cutting concerns (tracked throughout)

- **Testing:** provider tests against a recorded/mock server + smoke tests
  against the live endpoint; tool tests in temp dirs; a headless harness that
  runs scripted agent tasks for regression.
- **Model compatibility matrix:** track which served models reliably do tool
  calling / streaming (`qwen3-coder`, `gpt-oss:120b`, …) and note quirks.
- **Security:** path sandboxing, command allowlists, no secrets in logs, redact
  API keys, treat tool/MCP output as untrusted data (not instructions).
- **Observability:** `tracing` to a log file; a debug mode that dumps raw
  provider traffic.

---

## 6. Open decisions (resolve at the milestone that needs it)

1. **Line editor:** `reedline` vs. hand-rolled input. Blocker is clean coexistence
   of a live input row with streamed output above it (M2).
2. **Tool schemas:** hand-written JSON Schema per tool vs. derive from Rust types
   (e.g. `schemars`). Lean derive to avoid drift (M1).
3. **Edit semantics:** exact-string replace (like this harness) vs. diff/patch
   application. Start with exact-match; revisit for reliability (M1).
4. **Multiple concurrent tool calls:** run in parallel when independent? Start
   sequential for legibility; parallelize later if models emit batches (M1/M4).
5. **Config precedence:** project `atelier.toml` vs. user global vs. env/flags —
   nail down the overlay order (M0).

---

## 7. Non-goals (for now)

- Reaching Claude/ChatGPT subscriptions through unofficial means (ToS). SIWC
  is the sanctioned ChatGPT path; for Claude, the Claude backend drives the
  user's own `claude` CLI rather than its credentials.
- Full-screen / mouse-driven / multi-pane TUI.
- Editing or reflowing output already committed to scrollback.
- A plugin marketplace, web UI, or multi-user server (§3b, "Considered and
  left out", has the reasons and the rest of the list).
