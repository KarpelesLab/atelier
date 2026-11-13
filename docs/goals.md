# Goals: `/goal`

A model stops when it thinks it is done. A goal is a condition of *yours*
that has to hold before the work stops: while one is set, the end of a turn
is checked against it, and if it does not hold the agent is sent back to
work with the reason.

```
› /goal cargo test passes and clippy is clean
goal set: cargo test passes and clippy is clean
from your next prompt on, the agent is sent back to work until it holds (at most 10 round(s) per prompt)
› fix the failing tests
  … the agent works, and says it is done …
↻ goal not yet met: the transcript shows 2 tests still failing in parser::tests
  … it works on …
✓ goal met: cargo test reports 41 passed, 0 failed, and clippy printed no warnings
```

| Command | Effect |
|---------|--------|
| `/goal <condition>` | Set the session's goal (replacing any). The condition is free text, taken as typed |
| `/goal` | Show the goal and its bounds |
| `/goal clear` | Remove it |

Setting a goal starts nothing by itself: it applies from the next prompt
you send. When the goal is met it is cleared, with one line saying so.

The check is made by the `small` model of [`[models]`](configuration.md#models--a-model-per-role) when one is configured, else by the main model.

## How it is checked

Each time a turn ends — the model answered without asking for another tool
— a **judge** is asked whether the condition holds: one separate model
request, with no tools, given the goal and the last part of the transcript
(about 12 KiB: the recent messages, tool calls and the ends of their
results). It answers `{"met": true|false, "reason": "…"}`.

- **Met**: `✓ goal met: <reason>`, the goal is cleared, the turn ends.
- **Not met**: the reason is sent to the agent as the next message,
  `[goal not yet met] <reason>`, and it works on. That is one *round*.

The judge reads the transcript, not your project. Write goals whose proof
shows up in the agent's own work — "`cargo test` passes" makes the agent run
the tests, and the judge then sees the result — rather than ones nobody can
observe ("the code is elegant"). It is told to go by evidence, not by the
agent saying so: a claim without output to back it counts as not met, and
the agent is told what to show.

If the judge's reply is not the JSON asked for, the prompt fails with an
error instead of guessing a verdict; the goal stays set. The judge uses the
session's model.

## Bounds

A condition that can't be met must not run forever:

```toml
# atelier.toml
[goals]
max_rounds = 10      # times one prompt is sent back to work (default 10)
max_tokens = 50000   # output tokens one prompt may generate (default: no limit)
```

Both are per prompt you send. `max_tokens` counts the output tokens
generated since the prompt started — the agent's, its subagents' and the
judge's — as the endpoint reports them (it has no effect on an endpoint
that reports no usage). When a bound is reached with the goal unmet, the
turn ends with a notice:

```
the goal is not met after 10 round(s): 1 test still fails — stopping here; the goal stays set (/goal clear removes it)
```

The goal **stays set**: your next prompt pursues it again with a fresh
budget, so you decide whether to keep going. **Ctrl-C** stops the turn as
always, and the goal is likewise left set and not checked until your next
prompt ends.

## With hooks, subagents, `--max-turns`

- A [`turn_end` hook](hooks.md) runs first. If it sends the agent back to
  work, the judge is not asked yet; only an answer the hooks let stand is
  held against the goal. A hook that runs your tests is the cheaper and more
  exact check — use the two together.
- [Subagents](agents.md) never have a goal: the goal is the session's, and
  a subagent's report is judged by the agent that asked for it.
- `--max-turns` counts the agent's model requests across all rounds of a
  prompt. The judge's requests are not counted.

## Headless: `--goal`

```sh
atelier -p --goal "cargo test passes" "fix the failing tests" || echo "status $?"
```

`--goal <condition>` sets the goal for a `--print` run. The run ends with
status 0 once the goal is met, and with status **6** (`subtype:
"goal_unmet"`) if a bound is reached first — see [Headless
runs](headless.md#goals).

## With the Claude backend

The same loop applies, but atelier has no model endpoint of its own to ask
there, so the question is put to claude itself, as an extra turn of the
conversation whose output is not shown: claude decides whether the goal
holds (checking with its tools if it needs to) and answers in the same JSON.
It is the agent judging its own work rather than a second opinion.
