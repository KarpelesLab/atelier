# Review mode (the "subconscious")

Review mode runs a second, parallel model pass alongside the main agent loop.
It watches the exchange — the user's ask, the tool calls the agent just made,
and their results — and can post a short note back into the dialog. It has no
tools and makes no edits: it can only observe and comment.

## What it does

After each batch of tool calls, the session spawns a background reviewer on
its own thread (`Session::spawn_review` in `src/agent/mod.rs`), handing it a
text excerpt of the exchange since the last user message (the request, the
tool calls, and their arguments — see `Session::review_excerpt`). The
reviewer calls the model with a read-only reviewer prompt and no tool
definitions, and returns either a short note or nothing.

Because the reviewer runs on its own thread, it doesn't block the main loop:
finished notes are picked up non-blocking between steps
(`Session::drain_reviews`) and any still in flight are waited on at the end
of the turn (`Session::collect_reviews`), so a turn never ends with a review
silently dropped. Notes reach the interface through `Ui::subconscious`,
implemented separately by the TUI and the REPL.

A reviewer call that comes back empty, or with a bare "OK", produces no note
— the reviewer only speaks up when it has something worth saying.

## Turning it on

Three equivalent ways, in priority order:

1. **`/review [on|off]`** — toggle for the current session; with no argument,
   prints whether it's currently on. Also persists the setting to
   `atelier.toml`.
2. **`atelier.toml`**:
   ```toml
   [review]
   enabled = true
   ```
3. **`ATELIER_REVIEW=on`** (also accepts `yes`/`1`/`true`) — overrides
   `atelier.toml` for the session, same precedence as the other
   `ATELIER_*`/`[defaults]` pairs.

## Reviewer model

By default the reviewer uses the same model as the main conversation. Set an
override in `atelier.toml`:

```toml
[review]
enabled = true
model = "qwen3-coder:30b"
```

`model` is optional; leave it unset to reuse the active model.

## How notes appear

A note is prefixed with 💭 and dimmed, distinguishing it from the model's own
output:

```
💭 the new error path doesn't unwind the temp file — worth a second look
```

Notes can appear after any tool-call batch (as soon as that review finishes)
and, for whichever reviews are still running, at the end of the turn — so you
may see one mid-turn or right before the prompt returns.

## See also

- [`/config`](configuration.md) shows review mode's current state (on/off)
  alongside the rest of the session's settings.
- [Configuration](configuration.md) — `ATELIER_REVIEW` and `[review]` in the
  full settings reference.
