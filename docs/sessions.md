# Sessions: persistence, `--continue`, and compaction

The conversation — the rolling summary of older turns plus the recent message
history — lives in `agent::Session` in memory and is mirrored to disk so a run
can be resumed later.

## On-disk format

Saved to `.atelier/session.json` under the project root
(`session::SESSION_FILE`), written after **every completed turn**
(`Session::persist`, called once a model response arrives with no further
tool calls):

```json
{
  "summary": "…or null if nothing has been compacted yet",
  "messages": [ /* the recent Message history, verbatim */ ]
}
```

A user message may carry a `context` string next to its `content`: the
project context that was gathered for that turn (see [the shape of a
request](#the-shape-of-a-request)), stored apart from what you typed.

`summary` and `messages` both `#[serde(default)]`, so a partial or
older file still loads; so does one whose messages have no `context`. A
pre-compaction file — a bare JSON array of messages with no wrapping object —
also still loads (`summary` comes back `None`); atelier has always stored
some form of this file, so old sessions aren't invalidated by newer versions.

Add `.atelier/` to your project's `.gitignore`.

## Resuming: `--continue` / `-c`

```sh
atelier --continue          # or -c
atelier --continue --print "keep going"   # resume, then run one prompt headlessly
```

`main` checks for `--continue`/`-c` anywhere in `argv` and, if present, calls
`Session::resume()` before handing off to the TUI/REPL/`--print`. This loads
`.atelier/session.json` into `summary`/`history` and prints `resumed session
(N message(s))` to stderr. Without the flag, every run starts with empty
history (even if a session file exists — it is only loaded on request).

A program driving the session (`atelier serve --continue`) has seen none of
the resumed conversation's events; it asks for it with
[`session/history`](protocol.md#sessionhistory).

## One session, one owner

A project has one saved session, so one atelier at a time works on it. The
first to start **owns** it for as long as it runs; one started in the same
directory while it does is told so, and chooses.

```text
A session is already running in this directory:
  pid 4021 · the terminal interface on /dev/pts/3 · started 37 min ago
  now: a turn in progress · 1 background job · 2 subagents
? What do you want to do?
  1. Remote control — this terminal becomes a second front end of that session; it keeps running there
  2. Take over — stop that atelier and everything it is running, and continue its session here
  3. Cancel — leave it alone and exit
  (answer with a number)
```

The question is answered on the input line, like one the
[`ask` tool](tools.md#ask) puts: a number, or the option's name. Ctrl-C and
Ctrl-D are "cancel". If the other atelier ends while the question is up,
there is nothing left to ask, and this one starts as on any other day
(`the other atelier (pid 4021) has ended: starting normally`).

### Remote control

This terminal becomes a second front end of the session that is running.
Nothing is started here — no session, no instance among the
[coordinator](coordinator.md)'s (the session keeps its one name in
`/peers`), no hook — and nothing is stopped there.

```text
atelier — remote control of the session of pid 4021 on /dev/pts/3
/help for commands · Ctrl-D leaves this remote, and the session goes on
── the conversation so far ──
› now fix the off-by-one
⚙ edit {"path":"src/parser.rs","old_string":"depth > 8","new_string":"depth >= 8"}
✓ replaced 1 occurrence(s) in src/parser.rs
Fixed the off-by-one in `parser::nested`.
── now ──
```

- **It shows the conversation so far** (the last 200 items), then
  everything as it happens: the turn in progress from its start, and from
  there what the model says and does, whoever asked.
- **Both terminals work, on one session.** A prompt typed at either is a
  turn of the same conversation, and each shows what the other typed,
  marked: `› run the tests  ← remote 2` at the owner's,
  `› run the tests  ← the owner's terminal` at the remote.
- **During a turn** a line typed at the remote joins it, as at the owner's
  (`(queued)`, then delivered at the turn's next step); a slash command
  waits for the turn's end, with what is typed behind it. Ctrl-C on an
  empty line drops what is waiting, or else stops the turn — from either
  terminal.
- **An approval or a question** (`⚠ allow tool 'bash'?`, the
  [`ask` tool](tools.md#ask)) is put to both. Whoever answers first has
  answered; the other terminal takes its prompt down with a line saying so
  (`↳ allowed once at remote 2`, `↳ denied at the owner's terminal`).
- **A command that waits for input** says so at both, and Ctrl-T hands it
  the keyboard of the terminal it is pressed at (see [A command you can
  type to](tools.md#a-command-you-can-type-to)). What is typed goes to the
  command and nowhere else: not to the model, as always, and not to the
  other terminal — which sees what the command's terminal prints only if it
  took the keyboard too (the echo of the keys with it, where the program
  leaves echo on; a password is read without).
- **Slash commands run in the owner**, where the session and its settings
  live: `/model`, `/mode`, `/compact`, `/new`, `/mcp add`, `/agents`, a
  custom command — each changes the one session, for both terminals. Three
  are the terminal's own: `/quit` (also `/exit`, `/detach`, Ctrl-D) leaves
  the remote and nothing else, and `/config` says that the settings screen
  is the owner terminal's.
- **Leaving ends nothing**: quit, Ctrl-D, a closed terminal, a killed
  remote — the session, the turn in progress and everything it runs go on,
  and the owner's transcript notes `· remote 2 detached`.
- **When the owner ends, the remote says how and exits**: it quit, it was
  taken over (by whom), or it is gone without a word (killed). It does not
  carry the session on by itself; `atelier --continue` in the same
  directory does, from what was saved.

Several remotes may be attached at once; each is numbered, and they see
one another's lines as they see the owner's.

Remote control is offered when the owner is a **terminal interface**. A
session an editor or a program drives (`serve`, `acp`), a `--print` run and
the line REPL take no remote: the question then offers the other two
choices and says why. The remote is the same user on the same machine —
the owner's socket is the only gate (see [How it is
recorded](#how-it-is-recorded)) — so it may do what the owner's terminal
may.

A program attaches the same way, through `atelier serve --attach`: see
[A session shared with its terminal](protocol.md#a-session-shared-with-its-terminal).

### Take over

**Take over** stops the other process and everything it has going — the
turn in progress, the command it is running, its background jobs, its
subagents (those on threads and those that are processes of their own, with
what they run), its language and MCP servers — and then continues its
session here, from what was saved, as `--continue` does. In order:

1. The owner is **asked** to stop, save and exit, over its own socket. It
   interrupts its turn, saves the session and leaves as on a quit, with a
   last line on its terminal:
   `another atelier (pid 4188 on /dev/pts/5) took this session over; this
   one has stopped.` An owner that cannot stop within two seconds — a model
   that has not answered, a command that will not end — gives its terminal
   back, says the same and ends where it is; the session file is then the
   one of its last finished turn (it is always replaced whole, never
   written in place).
2. If the session is not free two seconds after that, the owner is
   **signalled**: TERM, then KILL. Never before checking that the process
   with that pid is still the one that wrote the record (by its start
   time), and that the session is still that owner's: a recycled pid is not
   signalled, and a session a third atelier took meanwhile is left to it.
3. What the owner **left running** is stopped: every process it had started,
   directly or not, noted before it was asked and stopped now if still
   there (TERM, then KILL). The [coordinator](coordinator.md) and the
   [local model](local.md)'s server serve other instances too and are not
   touched; the model's server ends on its own once nobody uses it.

The new owner then says what was done (`took over the session of pid 4021
(it stopped when asked); stopped 3 process(es) it left running.`), and
takes the old one's place among the instances of the
[coordinator](coordinator.md): same name, no trace of the old one or of the
locks it held. A subagent's worktree is left as a stop leaves it when the
owner could stop by itself in time. An owner that was cut short — it ended
where it was, or was signalled — leaves the worktree behind, locked for a
process that no longer exists; `/agents clean` removes it as it does any
other.

If the owner is an `atelier serve` or `atelier acp` — a session an editor
or another program drives — the question says so: taking it over ends that
program's session with it (a `serve` client is sent
[`session/closed`](protocol.md#sessionclosed) first; an ACP editor sees its
agent exit). A `--print --continue` run that is taken over ends with status
1, its last line on stderr saying why.

A remote attached to the session that is taken is told, and exits.

### Cancel

**Cancel** exits with status 8. Nothing was read, written or started: no
session opened, no instance joined, no hook run.

### Starts that cannot ask

A start with nobody at a terminal — `--print --continue`, `serve`, `acp`,
input from a pipe — does not guess: it prints who has the session and
exits with **status 7**, having changed nothing. Say beforehand what to do:

| Flag | When a session is already running here |
|------|------------------------------------------|
| `--attach` | Join it as a remote, as above: for the terminal interface, and for `serve` (whose client then speaks to the owner). With an owner that takes no remote, exit with status 7 |
| `--take-over` | Take it over, as above, and continue it |
| `--cancel` | Leave it alone and exit (status 8) |

The flags also answer the question beforehand at a terminal: `atelier
--attach` joins without asking. With no session running they change
nothing: the run starts as it would without them (`--take-over` does not by
itself resume a saved session; `--continue` does). `atelier serve` puts the
question to its client instead of exiting ([A session that is in
use](protocol.md#a-session-that-is-in-use)); `atelier acp` refuses the
editor's `session/new` with the same message unless it was started with
`--take-over`.

### Which runs own the session

Those that work on the project's saved session:

| Run | Owns the session |
|-----|------------------|
| `atelier` (the terminal interface, or the line REPL when input is not a terminal) | yes |
| `atelier --print --continue` | yes, for as long as it runs |
| `atelier serve`, and a connection of `serve --listen --continue` | yes |
| `atelier acp` | yes, from the editor's `session/new`, in the directory it names |
| `atelier --print` without `--continue` | no |
| A connection of `serve --listen` without `--continue` | no: the listener gives every connection a conversation of its own, several at once |
| `atelier review`, `debug prompt`, `doctor`, `mcp`, `stats`, `trust`, … and atelier's own helper processes (subagents, the coordinator) | no |

A run that does not own the session is never asked and never refused. Next
to an atelier that does, it leaves the saved session alone: a one-shot
`--print` there is answered as always and its conversation is not saved
(without an owner it is, as before).

### How it is recorded

Ownership is an exclusive lock (`flock`) on `.atelier/session.lock`, held
from the start of the session to the end of the process. The kernel drops
it however the process ends, so there is nothing to clean up after a crash:
the next start finds the lock free and is the owner, without a word. Of two
that start together, one gets the lock and the other is told about it.

The file holds the owner's record — a line of JSON — for whoever finds the
lock held:

```json
{"pid":4021,"started":"8812345","since":1767225600,"kind":"terminal",
 "tty":"/dev/pts/3","socket":"/run/user/1000/atelier/session-4021.sock","version":"0.1.2"}
```

`started` is when that process started as the system counts it, which tells
it from a later process with the same pid; `kind` is `terminal`, `repl`,
`print`, `serve` or `acp`. The record is never believed on its own: without
the lock held it is a leftover, and with it the pid is checked against
`started` before anything is done to it.

`socket` is where the owner answers: what it is doing (for the question),
the request to yield, and — when the owner is the terminal interface — the
[session itself, shared](protocol.md#a-session-shared-with-its-terminal)
with the front ends that attach to it. It is a Unix socket in the directory
the coordinator's is in (`$ATELIER_COORDINATOR_DIR`, else
`$XDG_RUNTIME_DIR/atelier`, else `~/.local/state/atelier`), protected as
[`serve --listen`'s](protocol.md#who-may-connect): a directory only you can
enter, mode `0600`, no TCP. Whoever can open it is you, on this machine;
that is the only gate, and it is the one your files have. Without such a
directory the session is owned all the same, and can be taken over by
signal only.

Not on Windows, where atelier has neither the socket nor the process table:
there every start proceeds as it always did.

## Starting over: `/new`

The `/new` command (or a fresh run without `--continue`) discards the
in-memory conversation: history, the rolling summary, any images staged with
`/image` but not yet sent, and the per-turn context-dedup cache — and deletes
`.atelier/session.json` (`Session::new_conversation`).

## The shape of a request

An inference server keeps the prompt it has already evaluated — llama.cpp's
KV cache, a hosted provider's prompt cache — and reuses it up to the first
byte that differs in the next request. On a local model that is most of the
latency of a turn, so atelier's own loop builds every request as the previous
one plus new messages at the end (`agent::prompt`):

```text
system     system prompt
           standing context: instruction files, memory index
           summary of earlier conversation (after a compaction)
user       <context> git status, diff, layout, … </context>   your 1st message
assistant  (tool calls)
tool       (results)
assistant  answer
user       <context> only what changed since </context>       your 2nd message
…
```

- **The system message is constant for the session.** It holds the system
  prompt and the *standing* context — the
  [instruction files](configuration.md#instruction-files) and the
  [memory index](memory.md). It changes only when one of those does (you edit
  `AGENTS.md`, the agent saves a memory), from the next message on, and the
  cache is rebuilt from there.
- **Everything volatile travels with the turn it was gathered for**: git
  status, the diff, diagnostics, the layout, TODOs, the environment go in a
  `<context>` block in front of your message, in the same user message
  (strict chat templates accept neither a second system message nor two user
  messages in a row). A block is never rewritten afterwards. The next turn's
  block holds only the items that changed, then names the rest — `(unchanged
  since last shown: layout, environment)`, `(no longer applies:
  diagnostics)` — so the model knows which older blocks still hold; a turn
  where nothing changed has no block at all. Each block is cut to about 2,000
  tokens, most important items first.
- **The tool list** is sent in registration order and only changes when an
  MCP server is added or removed.

What does rewrite the request, once: a [compaction](#compaction) (the summary
sits at the end of the system message, so the prompt and instructions before
it stay cached), `/new`, and a change to the standing context.

The [Claude backend](claude.md) is not concerned: claude assembles and caches
its own prompt, and atelier only prefixes an `<atelier-context>` block to
your message.

### What the cache is worth

Measured on 2026-11-17 against the development server: `qwen3.8-unc:q4` (a
27B at 4 bits) behind an Ollama-compatible endpoint on the local network. It
evaluates a prompt at roughly 3,000–4,000 tokens/s and reports what it
reused (`usage.prompt_tokens_details.cached_tokens`); its hardware was not
recorded. One scripted session, run through the `atelier` binary of a debug
build: five prompts that make the model read a 360-line file, edit it,
search it, write a note and read the note back. That is ten or eleven
requests, and the prompt grows from 7,450 tokens (the system message and the
22 built-in tools) to 16,000. Three sessions per arm, the arms taking turns;
the figures are for the requests after each session's first (27 to 29 per
arm), which meets whatever the server evaluated last.

| Requests | Prompt tokens from the cache | First token after: median (mean) | Waiting for first tokens, per session |
|---|---|---|---|
| as atelier sends them, each a prefix of the next | 93.7% | 0.71 s (1.05 s) | 10 s |
| the system message changes with every request | 49.1% | 2.65 s (2.81 s) | 25 s |
| the tool list changes as well: nothing to reuse | 0% | 4.51 s (4.57 s) | 44 s |

- **The first row** is atelier. What the server still evaluates there is
  what is new: after the model read the file, 5,469 of 12,912 tokens (first
  token after 1.9 s); on the next request, 109 of 13,017 (1.05 s).
- **The second row** is what a prompt that carries per-turn context in its
  system message costs — atelier's before requests were laid out this way.
  Here it loses half rather than everything, because the Qwen chat template
  renders the tool list *before* the system message: the tools stay cached
  (6,947 to 7,286 tokens, as the server reported), and all that follows
  them, the whole conversation, is evaluated again on every request. A template that puts the system message
  first loses the tools too, which is the third row.
- **"First token"** is the first streamed piece of any kind, reasoning
  included, counted from the request being sent. This server delivers a
  tool call in one piece, so on a step that only calls a tool the figure
  includes generating the call — in every row alike.
- **The saving is the prompt not evaluated**: 13,000 to 16,000 tokens a
  request here, about 4 s of every request on this server, and 34 s of a
  44 s session's waiting. It grows with the conversation and with a slower
  machine. At a tenth of this speed — a 27B on a CPU or a small GPU — the
  same arithmetic gives some 40 s a request; that was not measured.
- **The same session before the threshold followed the window** (a default
  of 8,000 tokens, of which the fixed part took 7,450; on a six-line file):
  two compactions in five prompts, each a summary request of its own and a
  rewritten prompt. With the present default there was none.
- **The cache is the server's**, and one prompt deep on this one: a session
  whose predecessor on the server was something else starts cold (2.5 s for
  the 7,450 tokens), and so would a request made in between by anything
  else that uses the same model — the [reviewer](review.md), another
  session.

To repeat it, against the endpoint and model of `ATELIER_BASE_URL` and
`ATELIER_MODEL`:

```sh
cargo test --test cache_live -- --ignored --nocapture
```

`tests/cache_live.rs` puts a recorder between atelier and the endpoint. It
times each response, reads the endpoint's usage figures, and in the two
control arms writes a different tag at the start of the system message (and
of the first tool's description) of every request. It also checks on the
real traffic that each request starts with the bytes of the one before and
carries the same tool list, as `tests/e2e.rs` does against the mock; that
held in every session.

## Compaction

As a conversation grows, the request sent to the model on every turn (system
message + full history) grows with it. atelier compacts automatically instead
of asking you to manage this by hand.

**When it runs** (`agent::compact`):

- **after a turn**, when the last request's total token usage (the
  provider's reported `usage.total_tokens`) exceeded the threshold;
- **when you ask**: `/compact [instructions]`;
- **when the endpoint refuses a request as too long** (HTTP 413, or a 400
  whose body speaks of the context length): the conversation is compacted and
  the request sent again, once per turn, instead of the turn failing. This is
  what happens when the threshold is above the model's real window, or the
  endpoint reports no usage.

**Threshold.** What you set is the threshold, as it is:
`ATELIER_CONTEXT_LIMIT` (a positive integer; anything else is ignored),
`[defaults] context_limit` in `atelier.toml`, or `/config`. Set nothing and
it follows the model's context window (`agent::limit`):

- **80% of the window**, where atelier knows it. The threshold is compared
  with the last request's prompt and answer together; the rest is room for
  what the next request adds to that — the results of the tools just called,
  and its own answer.
- **24,000 tokens** while it does not: inside the 32k window that is the
  least a model fit for this work is served with.
- **Never less than twice the fixed part of a request** — the system message
  and the tool list, which every request carries before the conversation
  starts. With the built-in tools alone that is about 7,500 tokens (7,485 as
  the server counted them for `qwen3.8`, 2026-11-17: 22 tools, 28 kB of
  JSON), and each MCP server adds to it; with 20,000 tokens of tools the
  threshold is 40,000. A threshold close to the fixed part compacts after
  nearly every turn — which is what the former fixed default of 8,000 did
  ([measured](#what-the-cache-is-worth)) — and each compaction is a model
  request and a prompt the server evaluates again. A known window caps the
  floor: a window too small for twice the
  fixed part is too small for the work.

atelier keeps no table of models and their windows. It learns one from, the
most telling first:

1. **a refusal.** An endpoint that refuses a request as too long often says
   how large its window is (`maximum context length is 8192 tokens`, `the
   available context size (8192 tokens)`). That figure — or, when it gives
   none, the estimated size of the request it refused — is the window from
   then on, so the next compaction comes before the next refusal.
2. **the provider's table**: `context_window` on a
   [`[[provider]]`](configuration.md#provider--named-endpoints), for a
   server you started with a size of your choosing.
3. **the endpoint**: the model's entry in `GET /models`, where it has one of
   `context_length`, `context_window`, `max_model_len`, `max_context_length`
   or `max_input_tokens` (OpenRouter, Groq, vLLM and others; not OpenAI or
   Ollama, whose lists hold only names, and not llama.cpp's `n_ctx_train`,
   which is what the model was trained with rather than what the server was
   started with). It is asked once per model, and only when it matters: the
   first time a request passes the 24,000 tokens, before compacting there.

What was learned is about one model, and is forgotten when `/model` picks
another. The last line of [`/context`](#where-the-window-goes-context) says
which of these the threshold comes from.

**The recent tail stays verbatim.** The most recent part of the conversation
is never touched: about 40% of the threshold, measured in estimated tokens
rather than in messages (a message may be one line or a whole file) — less
when the system message and tool list are large, so that a compaction leaves
room for a few turns before the next. Half of that when the endpoint refused
the request.

**Stage 1 — prune tool output.** Most of a coding session's bulk is file
contents and command output the model has already acted on. In everything
older than the tail, the body of each tool result is replaced with one line
saying what it was:

```text
[output of read src/agent/mod.rs — 412 lines, pruned]
[output of bash cargo test — 96 lines, pruned]
```

The calls, their arguments, and the pairing of each call with its result
stay, so the model still knows what it did and can run a call again. Images
that tools returned there are dropped too. If this brings the conversation
back under the threshold, compaction stops here — nothing was summarized.

**Stage 2 — a hand-off summary.** Otherwise (and always for `/compact` and a
refused request), everything before the tail is summarized and dropped. The
boundary only ever falls on one of your messages, so a tool result is never
cut from its call; if the tail holds none of your messages — one long turn —
the boundary is the start of that turn, and the turn in progress is never
summarized. The summary is written by the model in a dedicated, tools-free
request — to the main model unless `[models]` names another for `summary`
or `small` (see [a model per role](configuration.md#models--a-model-per-role))
— that asks for fixed sections:

- **Goal** — what you want, with the constraints you set
- **Decisions** — each one, and why
- **Current state** — done, half done, broken
- **Files touched** — and what was done to each
- **Tried and failed** — so it isn't tried again
- **Next steps**
- **From the user** — what you said that must not be forgotten, verbatim
  where short

The transcript it is given includes the previous summary, which it carries
forward; long tool results are clipped in it. `/compact`'s instructions are
added to the request (`/compact keep the list of failing tests`). `/compact`
summarizes all but the last turn even when the conversation is small.

**What you see:** one line with the estimated size before and after —

```text
compacted: pruned 7 old tool result(s), summarized 31 earlier message(s) — ~9,412 → ~3,180 tokens (estimated)
```

If the summary request fails, a `compaction: summary failed: …` notice is
shown, the pruning stands, the messages stay, and it is tried again the next
time the threshold is passed. After a compaction the next request's real
usage decides whether another is needed. Token counts are estimates (about
four characters per token), calibrated against the endpoint's reported usage
when deciding whether pruning was enough.

**What is not lost:** the agent's [memory](memory.md) and its todo list live
outside the conversation; instruction files stay in the system message; a
subdirectory's `AGENTS.md` is delivered again the next time the agent works
there.

**The cache:** either stage rewrites the conversation, so the inference
server evaluates the prompt again from the first changed message — once.
That is why pruning is done in one batch when the threshold is passed rather
than a little every turn.

The [Claude backend](claude.md) compacts its own conversation; `/compact`
does nothing there.

**How it's used afterward:** the summary closes the system message, under
`Summary of earlier conversation:` (see [the shape of a
request](#the-shape-of-a-request)). The model always sees the summary, never
the raw messages it was built from. The context blocks of the summarized
turns went with them, so the next turn's block carries every item again.

## Where the window goes: `/context`

`/context` prints what the next request is made of, one part per line, with
an estimated token count and its share of the compaction threshold:

```text
context window — estimated tokens, and their share of the compaction threshold (24,000)
  system prompt                               80    0.3%
  standing: project instructions             412    1.7%
  standing: memory                            96    0.4%
  summary of earlier turns                   211    0.9%
  tool list (22 tool(s))                   6,430   26.8%
  turn context (3 block(s))                  640    2.7%
    git status (×3)                          118    0.5%
    project layout                           402    1.7%
    environment                                9    0.0%
  history: user (4)                           61    0.3%
  history: assistant (9)                     350    1.5%
  history: tool results (6)                1,204    5.0%
  total                                    9,484   39.5%
last request: 10,655 tokens as counted by the endpoint (prompt + completion)
compaction threshold: 24,000 — the default while the model's context window is not known (set context_limit, or context_window on its provider)
```

- **standing** rows are the items of the system message besides the prompt;
  **summary** appears after a compaction.
- **turn context** is what the `<context>` blocks attached to your messages
  cost together, then by item (`×3`: three blocks carry it — it changed
  twice).
- **history** is the conversation by role; tool calls count with the
  assistant, their output under tool results — usually the first thing worth
  pruning (`/compact`).
- The counts are estimates of about four characters per token; the last line
  is the endpoint's own count for the last request, when it reports usage (it
  is cleared by a compaction until the next request). If the two differ a
  lot, trust the endpoint — it is what triggers compaction.
- The **compaction threshold** line says where the threshold comes from:
  your setting, the model's context window and how atelier knows it, or the
  default (see [Compaction](#compaction)).

A large tool list is a fixed cost on every request; removing an MCP server
you don't use (`/mcp remove`) is how to shrink it. Before the first message
of a session the standing and turn context rows are missing: project context
is gathered when you send one.

## See also

- [Configuration](configuration.md) — the full environment-variable reference
- [Quickstart](quickstart.md) — `--continue`, `--print`, and `/image` in the
  bigger picture of running atelier
