# Headless runs: `--print`

`atelier --print` (or `-p`) runs prompts to completion and exits instead of
opening the TUI/REPL. It is the interface for scripts, CI and programs that
drive atelier: stdout is a contract chosen with `--output-format`, and the
exit status says how the run ended.

```sh
atelier -p "summarize src/main.rs"                    # answer text on stdout
atelier -p < prompt.txt                               # no prompt words: read stdin
atelier -p --output-format json "list the TODOs"      # one JSON object at the end
atelier -p --output-format stream-json "fix the test" # JSON events as they happen
atelier --continue -p "keep going"                    # resume the saved session first
atelier -p -- "--verbose is ignored, why?"            # after --, nothing is an option
```

## Flags

| Flag | Meaning |
|------|---------|
| `-p`, `--print` | Run headlessly. The prompt is the remaining words, joined with spaces, or all of stdin when there are none |
| `--output-format text\|json\|stream-json` | What stdout carries (default `text`); see below |
| `--input-format text\|stream-json` | Where prompts come from (default `text`: arguments or stdin); see [Multi-turn input](#multi-turn-input) |
| `--max-turns N` | Stop a prompt after N model requests; see [Step limit](#step-limit) |
| `--output-schema FILE` | A JSON Schema the final answer must satisfy; see [Structured answers](#structured-answers) |
| `-c`, `--continue` | Resume this directory's saved session before running (see [Sessions](sessions.md)) |
| `--` | End of options: everything after it is the prompt |

Options can come before or after the prompt, and take their value as
`--flag value` or `--flag=value`. An option atelier doesn't know is a usage
error (exit status 2), not part of the prompt — put `--` before a prompt
that starts with a dash. The headless flags are an error without `--print`.

Nothing can answer an approval prompt in this mode, so tool calls that need
approval are **denied** — the model is told and carries on without them —
unless the tool is allowed in `atelier.toml` or `ATELIER_APPROVE=all` is set
(see [Permissions](permissions.md)).

## Output formats

**`text`** (default). Only the model's answer goes to stdout, streamed as it
is written; reasoning, tool activity, notices and errors go to stderr.

**`json`**. stdout carries exactly one line: the [`result`](#result) object,
printed when the prompt is done. Progress still goes to stderr.

**`stream-json`**. stdout carries one JSON object per line, as things
happen, and nothing else; stderr stays quiet apart from start-up messages
(such as the `--continue` notice). The stream opens with `init` and each
prompt closes with `result`.

### Events (`stream-json`)

Every event has a `type`. Consumers should ignore types and fields they
don't know: the set will grow.

| `type` | Fields | When |
|--------|--------|------|
| `system` (`subtype: "init"`) | `version`, `model`, `cwd`, `tools` (names) | Once, first. `model` is the one requests go to (`chatgpt:<model>` when signed in with ChatGPT) |
| `reasoning` | `text` | A chunk of the model's thinking, when the endpoint streams it |
| `text` | `text` | A chunk of the model's answer. Concatenate the chunks to get the text |
| `tool_start` | `name`, `arguments` | A tool is about to run. `arguments` is the parsed JSON object (or the raw string, if the model wrote invalid JSON) |
| `tool_end` | `name`, `ok`, `output`, `truncated`? | A tool finished; `ok` is false for an error result. `output` is cut to 16 KiB, with `"truncated": true` when it was |
| `question` | `id`, `question`, `options`, `multi_select`, `allow_other` | The model asked the user something (the [`ask` tool](tools.md#ask)) and the run is waiting for an `answer` line on stdin. Only with `--input-format stream-json`; see [Questions](#questions) |
| `approval` | `tool`, `arguments`, `decision` | A tool call needed approval. `decision` is always `"denied"` today (headless runs have nobody to ask); `"allowed"` is reserved. A `tool_end` with `ok: false` follows, without a `tool_start` |
| `info` | `text` | Informational output, e.g. the risk signals of a call that needs approval, or the progress lines of a [subagent](agents.md) (`⤷ explore: …`, its tool calls, `⤶ explore: done (7 steps)`) between the `task` call's `tool_start` and `tool_end`. A subagent's own text, reasoning and tool results are not streamed |
| `notice` | `text` | Out-of-band status, e.g. the schema retry, or an error from background work |
| `review` | `text` | A note from the [subconscious reviewer](review.md), when it is on |
| `result` | see below | A prompt ended |

### `result`

The last event of each prompt in `stream-json`, and the whole output in
`json`:

```json
{
  "type": "result",
  "subtype": "success",
  "is_error": false,
  "result": "The tests pass now.",
  "num_steps": 3,
  "duration_ms": 8421,
  "usage": { "context_tokens": 5120, "output_tokens": 311 }
}
```

| Field | Meaning |
|-------|---------|
| `subtype` | How the prompt ended: `success`, `error`, `max_turns`, `schema_error`, `input_error` (table below) |
| `is_error` | `true` for every subtype but `success` |
| `result` | The final answer: the text the model wrote after its last tool call (what it says before calling a tool is commentary; the `text` events carry all of it). May be empty or partial when `is_error` |
| `error` | Present when `is_error`: what went wrong |
| `structured` | Present with `--output-schema`, on success: the answer as a JSON value |
| `num_steps` | Model requests the prompt took: one per batch of tool calls, plus the final answer |
| `duration_ms` | Wall-clock time of the prompt |
| `usage.context_tokens` | Prompt + completion tokens of the last request, i.e. the conversation's size |
| `usage.output_tokens` | Tokens the model generated for this prompt |

Usage numbers are 0 when the endpoint doesn't report usage.

## Exit status

| Status | `subtype` | Meaning |
|--------|-----------|---------|
| 0 | `success` | The prompt was answered |
| 1 | `error` | The run failed: provider or network error, unreadable stdin |
| 2 | `input_error` | Usage error: unknown option, bad value, unreadable `--output-schema` file (no output on stdout, the message and usage on stderr) — or a malformed `--input-format stream-json` line (a `result` is emitted) |
| 3 | `max_turns` | `--max-turns` ran out before the model finished |
| 4 | `schema_error` | The answer didn't satisfy `--output-schema`, even after the retry |

A denied tool call is not a failure by itself: the model sees the denial
and the run ends however the model ends it.

## Step limit

`--max-turns N` caps the model requests one prompt may make. A plain answer
is one request; every round of tool calls adds one. When the model still
wants to go on after N, the turn ends there: the conversation is saved in a
consistent state (so `atelier --continue -p "go on"` picks it up), the
result's `subtype` is `max_turns`, and the exit status is 3.

```sh
atelier -p --max-turns 20 "fix the failing test" || echo "stopped: $?"
```

## Structured answers

`--output-schema schema.json` asks for a final answer that is a JSON value
matching the schema. The schema is appended to the prompt, the JSON is taken
from the model's final message (a ```` ```json ```` fence or a sentence
around it is tolerated) and validated. If it doesn't match, the model is
told what is wrong and gets **one** more try; if that fails too, the run
ends with `schema_error` and exit status 4.

On success the parsed value is in `result.structured` (`json`,
`stream-json`), or printed alone on stdout as one line of JSON (`text` —
the answer as the model wrote it goes to stderr).

```sh
cat > schema.json <<'EOF'
{
  "type": "object",
  "properties": {
    "summary": { "type": "string" },
    "risk": { "enum": ["low", "medium", "high"] },
    "files": { "type": "array", "items": { "type": "string" } }
  },
  "required": ["summary", "risk"],
  "additionalProperties": false
}
EOF
atelier -p --output-schema schema.json "review the last commit" | jq -r .risk
```

The validator is built in and covers the common subset: `type` (a name or a
list), `properties`, `required`, `additionalProperties` (`false` or a
schema), `items`, `enum`, `const`, nested to any depth. Other keywords
(`pattern`, `minimum`, `$ref`, `oneOf`, …) are ignored — the model still
sees them in the prompt, but they are not enforced.

The retry is a second prompt in the conversation: it gets its own
`--max-turns` budget, and its requests count in `num_steps`.

## Multi-turn input

`--input-format stream-json` makes the run a session driven over stdin: each
line is one prompt,

```json
{"type":"user","text":"what does src/agent/mod.rs do?"}
```

run in the same conversation as the ones before it and closed by its own
`result` (so with `--output-format json`, stdout is one result line per
prompt). Blank lines are skipped; end of input ends the run. A prompt may
not also be given as arguments.

The run stops at the first prompt that doesn't succeed, with that prompt's
exit status: later prompts were written expecting it to have worked. A line
that isn't a `user` object stops it the same way (`input_error`, status 2).

### Questions

The model can put a question to the user with the [`ask`
tool](tools.md#ask). In a headless run there is usually no one to ask, and
the tool tells the model so (it is asked to decide for itself and state its
assumption). The exception is `stream-json` in both directions, where the
parent process stands in for the user. The question arrives as an event,

```json
{"type":"question","id":1,"question":"Which database?","options":[{"label":"SQLite","description":"one file"},{"label":"Postgres"}],"multi_select":false,"allow_other":true}
```

and the prompt waits until stdin carries the answer with the same `id` —
labels exactly as given (one, or several when `multi_select`), or free text
when `allow_other`:

```json
{"type":"answer","id":1,"labels":["Postgres"]}
{"type":"answer","id":1,"text":"neither, keep it in memory"}
```

An `answer` that doesn't fit (wrong `id`, an unknown label, several labels
for a single choice, text where only the options are allowed) is reported
in a `notice` event and the question keeps waiting. A `user` line that
arrives meanwhile is not lost: it runs as the next prompt. If stdin ends
first, the model is told there was no one to ask. `id` counts up from 1
over the run.

A parent process can hold stdin open and send the next prompt after reading
each `result`:

```sh
printf '%s\n' \
  '{"type":"user","text":"read README.md"}' \
  '{"type":"user","text":"now list what is missing from it"}' |
  atelier -p --input-format stream-json --output-format stream-json |
  jq -r 'select(.type == "result") | .result'
```

## Recipes with `jq`

```sh
# Just the answer, from JSON mode.
atelier -p --output-format json "explain the build" | jq -r .result

# Fail a CI step on anything but success, keeping the reason.
out=$(atelier -p --output-format json "check the changelog") ||
  { echo "$out" | jq -r '.subtype + ": " + .error' >&2; exit 1; }

# Follow a run live: tool calls as they start, then the answer.
atelier -p --output-format stream-json "fix the failing test" |
  jq -r 'if .type == "tool_start" then "> \(.name) \(.arguments | tojson)"
         elif .type == "result" then "\n\(.result)" else empty end'

# Which tools failed?
atelier -p --output-format stream-json "run the linters" |
  jq -c 'select(.type == "tool_end" and .ok == false) | {name, output}'

# Cost of a run.
atelier -p --output-format json "refactor foo" | jq '{num_steps, usage}'
```

## With the Claude backend

With `--claude` (see [Claude backend](claude.md)) the same flags apply, with
these differences:

- `--max-turns` is handed to claude as its own `--max-turns` when it is
  launched, so the count is claude's (its agentic turns), not atelier's; a
  turn it ends that way is reported as `max_turns` / status 3 all the same.
- `init`'s `model` is the configured name, if any: claude settles on the
  actual model when it starts, at the first prompt.
- `num_steps` counts the assistant messages claude reported, and
  `usage.context_tokens` its input tokens at the last one.
- `--output-schema` works the same way (prompt, extract, validate, retry);
  claude's own `--json-schema` is not used.
- A one-shot run waits for background work claude started (subagents,
  background shells) before printing the result. With `--input-format
  stream-json` each result is printed as soon as its prompt is done, so
  events from background work may follow the last result.
