# atelier — Roadmap

An AI coding harness written in Rust. An agent loop that talks to
OpenAI-compatible model APIs, runs tools in a project directory, speaks MCP,
and continuously feeds itself context (git status, diagnostics, …) — driven
from a deliberately minimal terminal interface.

Status: **pre-M0** (empty repo). This document is the plan of record; it will
change as we build.

---

## 1. Principles

- **Minimal surface, append-only output.** The interface is a single input
  line with a status strip (active model, cwd, token/cost counters). Assistant
  and tool output is *printed to the terminal scrollback and never redrawn*.
  No panes, no mouse, no alt-screen. What scrolls past is history.
- **Bring-your-own API.** We target OpenAI-compatible endpoints only. No
  Claude/ChatGPT subscription backends — their ToS forbids it. First-class
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
| Terminal        | `crossterm`                                | raw mode, inline redraw of input row only |
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
- Interrupt (Ctrl-C) cancels the in-flight turn cleanly; Ctrl-D exits.
- Slash commands: `/model`, `/help`, `/clear`, `/quit` to start.

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
- Token budgeting: providers are prioritized and truncated to a context budget;
  stale/unchanged context is deduped so we don't re-send it every turn.

**Exit:** the agent visibly reacts to repo state it was never explicitly told
about (e.g. notices uncommitted changes, a failing build).

### M5 — MCP support
Extend the tool surface via the ecosystem.

- MCP client — **stdio** transport ✅ and **Streamable HTTP** transport ✅
  (shared `JsonRpc` trait; HTTP does POST + JSON/SSE responses + `Mcp-Session-Id`).
  HTTP servers are not yet wired into `atelier.toml`/`/mcp` config (follow-up).
- Server config in `atelier.toml`; discovered MCP tools are merged into the tool
  registry and namespaced (`mcp__<server>__<tool>`).
- MCP tools flow through the same permission model as local tools.
- (Later) MCP resources/prompts surfaced to the agent.

**Exit:** a configured MCP server's tools are callable in a task, indistinguishable
from built-in tools to the agent.

### M6 — Session & context management
Make long sessions durable.

- Persist conversation/session to disk; resume a session.
- Context-window management: track token usage; **compaction/summarization** of
  older turns when approaching the model's limit.
- Cost/usage accounting surfaced in the status strip.

**Exit:** a long, multi-hour session survives restarts and doesn't hard-fail at
the context limit.

### M7 — Hardening & polish
- Provider quirks: per-model tool-calling dialects, missing SSE fields, retries,
  backoff, timeouts.
- Config UX, better `/` commands, `--print`/pipe-friendly headless mode.
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
- ⏳ async `fs`; out-of-project fs via approval; `Uint8Array`/binary support.

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
- **Mid-turn steering (M2).** Let the user inject guidance into a running turn
  without aborting it — queued and surfaced to the agent at the next step.
  *(This very roadmap was steered that way mid-turn; the pattern works.)*
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

- Claude/ChatGPT subscription backends (ToS).
- Full-screen / mouse-driven / multi-pane TUI.
- Editing or reflowing output already committed to scrollback.
- A plugin marketplace, web UI, or multi-user server.
