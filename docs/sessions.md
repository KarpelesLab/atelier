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

`summary` and `messages` both `#[serde(default)]`, so a partial or
older file still loads. A pre-compaction file — a bare JSON array of
messages with no wrapping object — also still loads (`summary` comes back
`None`); atelier has always stored some form of this file, so old sessions
aren't invalidated by newer versions.

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

## Compaction

As a conversation grows, the request sent to the model on every turn (system
prompt + summary + fresh context + full history) grows with it. atelier
compacts automatically instead of asking you to manage this by hand.

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

**How it's used afterward:** every request's single leading system message
carries, in order: the fixed system prompt, `Summary of earlier
conversation:` + the rolling summary (if any), then fresh per-turn context
(git status, diff, layout, diagnostics). The model always sees the summary,
never the raw messages it was built from.

## See also

- [Configuration](configuration.md) — the full environment-variable reference
- [Quickstart](quickstart.md) — `--continue`, `--print`, and `/image` in the
  bigger picture of running atelier
