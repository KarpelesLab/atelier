# Troubleshooting

Tools for finding out why atelier does not do what you expect.

## Checking the setup: `atelier doctor`

```sh
atelier doctor              # the checklist
atelier doctor --offline    # the same, without contacting the endpoint
atelier doctor --mcp        # also start the configured MCP servers
atelier doctor --json       # one JSON object, for a script or a bug report
```

Run it in the project directory. It reads what a session there would read —
through the same code — and prints one line per check, without starting a
session: no hook runs, no coordinator is launched, nothing is written.

```
ok    version: atelier 0.1.1 (x86_64-unknown-linux-fullrust), linux/x86_64
FAIL  project config: /work/app/atelier.toml does not parse, so it is ignored as a whole — at `defaults.context_limit`: invalid type: string "lots", expected u32
ok    user config: none (/home/me/.config/atelier/atelier.toml)
ok    backend: OpenAI-compatible endpoint http://localhost:8080/v1 (ATELIER_BASE_URL); no API key
ok    model: qwen3-coder:30b (ATELIER_MODEL)
ok    endpoint: GET http://localhost:8080/v1/models answered in 12 ms with 3 model(s)
ok    model offered: qwen3-coder:30b is in the endpoint's list
ok    instructions: 1 file(s), in load order: /work/app/AGENTS.md (2114 bytes)
ok    memory: 4 project note(s), 1 global
ok    skills: 2 found
ok    commands: 1 custom command(s)
warn  agents: 2 found; skipped: /work/app/.atelier/agents/lint.md: no `description` in the front matter
warn  hooks: 0 of the user's, 2 of the project's — NOT approved, so they do not run: review them and /hooks trust
ok    mcp 'docs': npx -y docs-server — not started (--mcp starts it)
ok    coordinator: not running; the first interactive or --print run starts it (/run/user/1000/atelier)
ok    git: on main, 3 changed file(s)
ok    terminal: stdin is a terminal, stdout is a terminal, TERM=xterm-256color, 120x40
```

| Status | Meaning |
|--------|---------|
| `ok` | As it should be. The line still states the fact — which file, which model and what chose it — because that is often the answer |
| `warn` | Works, but probably not as you meant: a skill or agent file that was skipped, project hooks waiting for approval, a model the endpoint does not list, no git repository |
| `FAIL` | Will not work: a settings file that does not parse, an endpoint that does not answer, a `claude` executable that is not there, an MCP server that does not start (with `--mcp`) |

The exit status is 1 when any check failed, 0 otherwise (warnings
included), 2 for a usage error. Problems are always lines of the report,
never a crash or a stack trace.

What each check looks at:

| Check | What it reports |
|-------|-----------------|
| `version` | The version, the release target (or "built from source"), and the OS/architecture |
| `project config`, `user config` | `./atelier.toml` and `~/.config/atelier/atelier.toml`: present or not, and whether they parse. A file that does not parse is **ignored as a whole** by a session (with one warning at startup); the report names the key at fault — `defaults.context_limit`, `mcp[1]`, `hooks[0].timeout_secs` — or, for a syntax error, the parser's message |
| `backend`, `model` | Where requests go and under which model name, and what decided each: an environment variable, `[defaults]` in `atelier.toml`, or the built-in default. Says whether an API key is set, never the key. When signed in with ChatGPT, says so (requests then go to OpenAI, not to the configured endpoint). `--claude` checks the Claude backend instead: that the executable is on `PATH` |
| `endpoint`, `model offered` | `GET <base>/models`: whether it answered, how long it took (connect timeout 5 s), and whether the configured model is in the list — a warning when it is not, since some servers serve one model under any name. Skipped with `--offline` |
| `instructions` | The [instruction files](configuration.md#instruction-files) that load at startup, in order, with sizes |
| `memory`, `skills`, `commands`, `agents` | How many of each a session would find; files that were skipped are named with the reason |
| `hooks` | The user's and the project's [hooks](hooks.md), and whether the project's are approved to run |
| `mcp` | Each configured [MCP server](mcp.md). They are programs, so they are only listed — unless `--mcp`, which starts each one and reports the time to connect and its number of tools |
| `coordinator` | Whether the [coordinator](coordinator.md) is running (it is asked, never started), or why it is off |
| `git` | Branch and number of changed files, or that this is not a repository |
| `terminal` | Whether stdin and stdout are terminals, `TERM`, and the window size — what the inline TUI needs; without a terminal atelier falls back to the line REPL |

`--json` prints `{"ok": bool, "checks": [{"name", "status", "detail"}, …]}`
with `status` one of `ok`, `warn`, `fail`:

```sh
atelier doctor --json | jq -r '.checks[] | select(.status != "ok") | "\(.status): \(.name): \(.detail)"'
```

## Seeing the prompt: `atelier debug prompt`

```sh
atelier debug prompt "fix the failing test"      # the request, as JSON, on stdout
atelier debug prompt < prompt.txt                # the prompt from stdin
atelier debug prompt "hi" | jq -r '.messages[0].content'   # just the system message
atelier debug prompt "hi" | jq -r '.tools[].function.name' # the tools on offer
```

Prints the body of the **first request** the prompt would make — the system
message (system prompt, instruction files, memory index), the user message
with its `<context>` block (git status, diagnostics, layout, …), the tool
definitions, the model — and sends nothing. Use it to answer "did my
`AGENTS.md` load?", "what does the model see of my project?", "why is this
tool not offered?" without paying for a turn or reading a server's logs.

It is the request `atelier --print "<the same prompt>"` would send from the
same directory, built by the code the agent loop itself runs rather than by
a description of it, so the two cannot disagree. Specifically:

- The body is in the format of the backend in use: chat completions
  (`messages`, `tools`) for an OpenAI-compatible endpoint, the Responses
  API (`instructions`, `input`, `tools`) when signed in with ChatGPT.
- [Hooks](hooks.md) that shape a prompt run as they would: `session_start`
  and `user_prompt` (their context is part of what is sent; a `user_prompt`
  hook that blocks the prompt makes the command fail with its reason).
  No tool runs, no model is called, and no session is saved.
- No credential is in the output: the API key travels in a request header,
  which this does not print.
- MCP tools are not listed, as in `--print`: headless runs do not connect
  the configured MCP servers.

It only shows the first request of a turn. Later ones are that request plus
the conversation so far; `ATELIER_DEBUG=1` prints every request body to
stderr as it is sent, when you need those.

Not available with the [Claude backend](claude.md): there claude assembles
the request, and atelier only adds a context block to your message.

Exit status: 0, or 1 when the request could not be built (a blocking hook,
the Claude backend), 2 for a usage error.
