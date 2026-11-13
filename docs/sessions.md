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

**When it runs:** after a turn finishes (`Session::maybe_compact`, called
right before persisting), if **both**:

- the *previous* request's total token usage (`usage_ctx`, from the
  provider's reported `usage.total_tokens`) exceeds the compaction
  threshold, **and**
- the history holds at least 10 messages (`COMPACT_MIN_MESSAGES`) — so a
  short conversation is never touched.

**Threshold:** 8,000 tokens by default, overridable with
`ATELIER_CONTEXT_LIMIT` (a positive integer; anything else falls back to the
default). Read once at `Session::new` — set it before launching, not
mid-session.

**What gets summarized:** everything before a split point is compacted; the
split point keeps at least the 6 most recent messages
(`COMPACT_KEEP_RECENT`), then advances forward to the next `user`-role
message so the kept suffix never starts mid-tool-call (an orphaned tool
result would make the next request malformed). If no such split point exists
(e.g. the tail has no user message), compaction is skipped for that turn.

**How it summarizes:** the dropped messages (plus any prior summary) are
rendered to a plain-text excerpt and sent to the model in a dedicated,
tools-free request with a system prompt asking for a concise, bullet-point
summary — concrete facts, decisions, file paths touched, open tasks. The
result replaces `summary`; the summarized messages are drained from
`history`. On success, the UI prints `compacted N earlier message(s) into a
summary`; if the summarization call fails, the turn's output isn't affected
but a `compaction failed: …` notice is shown and history is left untouched
(compaction is retried on a later turn once usage crosses the threshold
again — `usage_ctx` is reset to 0 immediately after a successful compaction
so the *next* turn's real usage decides whether to compact again, not the
now-stale pre-compaction number).

**How it's used afterward:** the summary closes the system message, under
`Summary of earlier conversation:` (see [the shape of a
request](#the-shape-of-a-request)). The model always sees the summary, never
the raw messages it was built from. The context blocks of the summarized
turns went with them, so the next turn's block carries every item again.

## See also

- [Configuration](configuration.md) — the full environment-variable reference
- [Quickstart](quickstart.md) — `--continue`, `--print`, and `/image` in the
  bigger picture of running atelier
