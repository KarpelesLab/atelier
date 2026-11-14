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
  what happens when the threshold is set above the model's real window, or
  the endpoint reports no usage.

**Threshold:** 8,000 tokens by default; set it with `ATELIER_CONTEXT_LIMIT`
(a positive integer; anything else falls back to the default),
`[defaults] context_limit` in `atelier.toml`, or `/config`. Set it somewhat
below your model's context window.

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
context window — estimated tokens, and their share of the compaction threshold (8,000)
  system prompt                               80    1.0%
  standing: project instructions             412    5.2%
  standing: memory                            96    1.2%
  summary of earlier turns                   211    2.6%
  tool list (17 tool(s))                   4,281   53.5%
  turn context (3 block(s))                  640    8.0%
    git status (×3)                          118    1.5%
    project layout                           402    5.0%
    environment                                9    0.1%
  history: user (4)                           61    0.8%
  history: assistant (9)                     350    4.4%
  history: tool results (6)                1,204   15.1%
  total                                    7,259   90.7%
last request: 7,804 tokens as counted by the endpoint (prompt + completion)
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

A large tool list is a fixed cost on every request; removing an MCP server
you don't use (`/mcp remove`) is how to shrink it. Before the first message
of a session the standing and turn context rows are missing: project context
is gathered when you send one.

## See also

- [Configuration](configuration.md) — the full environment-variable reference
- [Quickstart](quickstart.md) — `--continue`, `--print`, and `/image` in the
  bigger picture of running atelier
