# Loops

`/loop` makes the session send itself a prompt at an interval — for the
things you would otherwise come back and ask about:

```
/loop 5m check the deploy pipeline and tell me if a stage failed
/loop 1h pull main, run the tests, and summarize anything newly broken
```

The prompt runs once right away, then again every interval, for as long as
the session is open.

| Command | |
|---|---|
| `/loop <interval> <prompt…>` | Start a loop. The interval is a whole number of seconds, minutes or hours — `30s`, `5m`, `1h` — and at least `10s` |
| `/loop` | List the loops: interval, runs so far, time to the next run, prompt |
| `/loop stop <n>` | Stop loop `n` |
| `/loop stop` or `/loop stop all` | Stop every loop |

Each run is announced where it happens:

```
⏰ loop 1 (run 3 of 50): check the deploy pipeline and tell me if a stage failed
```

## When a loop runs

A run is an ordinary turn — the prompt is sent as if you had typed it, in
the same conversation, under the session's [mode](permissions.md#modes) and
permission rules. Nothing is approved because a loop asked: a run that
needs an approval you have not given waits at the approval prompt until you
answer. Give a loop a prompt whose work is already allowed (read-only
checks; commands you have always-allowed), or run the session in the mode
you mean it to have unattended.

A loop only runs **while the session is idle**:

- If a turn is running when a loop comes due — yours, or another loop's —
  the loop waits for it to end, then runs.
- Missed runs do not pile up. However many intervals went by during a long
  turn, the loop runs once, and the next run is one interval after that.
- Nothing starts while you have text on the input line; the run waits until
  the line is empty. (The line REPL — what atelier is when its input is not
  a terminal — cannot see a half-typed line.)
- With several loops due at once, they run one after the other, the one
  that has waited longest first.

The prompt is sent as text: a loop's prompt that starts with `/` is not run
as a command.

## When a loop stops

- `/loop stop`, as above.
- **Ctrl-C at an empty prompt** stops every loop. (Ctrl-C while a run is in
  progress stops that turn, as it stops any turn; press it again at the
  prompt to stop the loop itself.)
- After `max_runs` runs — 50 unless `[loops]` says otherwise. The last run
  says it is the last.
- When two of its runs in a row end in an error (the model endpoint is
  down, say): `⏰ loop 1 stopped: its turn failed 2 times in a row`.
- When the session ends. Loops are not saved, and `--continue` does not
  bring them back.

```toml
[loops]
max_runs = 50   # runs one loop gets before it stops by itself
```

## What loops are not

- **Not for headless runs.** `atelier --print` runs its prompt and exits;
  it has no loops.
- **Not a scheduler.** A loop lives in one open session. For something that
  must run whether or not an atelier is open — every night, on every boot —
  use the system's scheduler with a headless run:

  ```cron
  # every weekday at 07:00
  0 7 * * 1-5  cd /home/you/proj && atelier --print "summarize yesterday's commits" >> ~/atelier-daily.log 2>&1
  ```

  See [headless mode](headless.md) for output formats and exit codes.

Loops need nothing but the session: they work the same with
`ATELIER_NO_COORDINATOR=1`. With the [coordinator](coordinator.md), what a
loop's run sends to other instances counts as coming from you (it can wake
an instance set to [wake](coordinator.md#waking-an-idle-instance)), since
the loop is your standing instruction.
