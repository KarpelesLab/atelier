# Troubleshooting

Tools for finding out why atelier does not do what you expect.

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
