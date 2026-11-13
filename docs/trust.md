# Project trust

A project's `atelier.toml` travels with the repository. Parts of it are
commands to run, or authority to hand out — and a repository you cloned a
minute ago should get neither just because you started atelier in it. So
those parts, the **gated settings**, take effect only once you have accepted
them for that project. Everything else works at once: you can chat, read and
edit in an untrusted project as in any other.

Most projects never see any of this: a project with no gated settings needs
no trust and nothing is printed.

If you used atelier before this check existed, each project that configures
something gated asks once — the settings that used to apply unprompted now
wait for `/trust`.

## What is gated, and why

| Setting | Why it waits |
|---------|--------------|
| `[[mcp]]` | A command of the file's choosing, launched as the session starts |
| `[[mcp_http]]` | A URL of the file's choosing, contacted as the session starts with the headers it lists |
| `[[diagnostics]]` | Commands run every turn, unprompted |
| Auto-detected checkers (no `[[diagnostics]]`, and a `Cargo.toml`, `tsconfig.json`, `go.mod`, `pyproject.toml` or `setup.py`) | They run the project's own build: `cargo check` runs build scripts and proc macros, `npx tsc` runs a binary from `node_modules`. Detection itself is skipped too — one probe (`python -c 'import pyflakes'`) runs in the project directory |
| `[[provider]]` (every entry) | An endpoint the whole conversation — your prompts, file contents, memory — can be sent to; `api_key_env` sends an environment variable of the file's choosing there, `api_key_command` runs a shell command, `headers` are sent along |
| `[permissions] allow` | Calls the agent then makes without asking: a file that says `allow = ["bash"]` would approve its own commands |
| `[permissions.reviewer]` with `enabled = true` | A model answering approval prompts in your place, under a policy file of the repository's choosing: a file that could switch it on could appoint a lenient judge of its own commands |
| `[defaults] mode = "yolo"`, or `approve = true` | Turns every approval prompt off |
| `[[hooks]]` | Shell commands run on lifecycle events |
| `[search] key_env`, `[search] url` | An environment variable of the file's choosing, sent as the key to a search endpoint — and the endpoint itself, which would also be sent the key in your own `ATELIER_SEARCH_KEY`, and the agent's queries |
| `[sandbox] enabled` | Either way it is a say over the `bash` tool: `true` stops the commands the [sandbox](sandbox.md) can run from asking; `false` takes away a sandbox you switched on in your own file, so commands run on the host |

Not gated, because they only restrict or are harmless:

- `[permissions] deny` and `ask` — they apply in an untrusted project too.
- `[permissions.reviewer]` with `enabled = false` — it switches your own
  [approval reviewer](permissions.md#the-approval-reviewer) off for the
  project, which only means more prompts.
- `[defaults] mode = "plan"`, `"ask-edits"` or `"auto-edit"` (the built-in
  default), `context_limit`, `model`.
- `[review]`, `[variant.*]`, `[[price]]`, `[search] provider` on its own.
- `[models]` and `[defaults] provider`, as long as they name a provider of
  your own (the environment's, or one from your user-global file).

Also not gated — they are text put in front of the model, not commands
atelier runs — but listed in the summary so you see the whole picture:
[custom commands](commands.md) (their `` !`command` `` lines go through the
`bash` approval when you invoke one), [skills](skills.md), [agent
definitions](agents.md) and [instruction files](configuration.md#instruction-files).

Your user-global `atelier.toml` (`$XDG_CONFIG_HOME/atelier/atelier.toml`) is
yours and is never gated.

### A provider without a key is gated too

A `[[provider]]` with no key source and no headers runs nothing and leaks no
variable, but it still chooses where the conversation goes — and whoever
answers as the model directs the agent's tool calls. An untrusted project's
providers are therefore left out whole. So that the session still talks,
whatever pointed at one of them (`[defaults] provider`, `[defaults] model`,
`[models]`, `[review] model`) falls back as if it were unset, to your default
endpoint and model.

## In an untrusted project

- MCP servers are neither started nor contacted. (With the
  [Claude backend](claude.md) they are not passed to claude.)
- No checker runs, configured or detected: there are no diagnostics.
- The project's providers do not exist; requests go to your own.
- Only your own allow rules count: the user-global ones, and those you save
  with "always" during the session (they are written to `atelier.toml` as
  usual, and count in later sessions once the project is trusted).
- The project's approval reviewer is not consulted; yours (from the
  user-global file) is, under your policy.
- Sessions start in `auto-edit` unless you choose otherwise (`--mode`,
  `ATELIER_MODE`, `ATELIER_APPROVE`).
- Project hooks do not run. Your own do.
- `web_search` ignores `[search] key_env` and `url`: `ATELIER_SEARCH_KEY`
  still works, and goes to the provider's own endpoint (or
  `ATELIER_SEARCH_URL`), never to one the project named.
- The project's `[sandbox] enabled` says nothing: whether `bash` is
  sandboxed is up to your own file and `ATELIER_SANDBOX`. This one is read
  from the file as it is on disk at each call, so an edit to it made during a
  session — the agent can write `atelier.toml` — does not take effect before
  you have accepted it.

## Accepting

When an interactive session starts in a project with gated settings you have
not accepted — or that changed since you did — atelier prints them and goes
on:

```
project trust: this project's atelier.toml asks for the following, which is NOT in effect yet (a new safety check: …):
  MCP server 'fs', started with each session: npx -y @modelcontextprotocol/server-filesystem .
  checker 'lint', run each turn: make lint
  provider 'hosted', an endpoint the conversation can be sent to: https://api.example.com/v1 (sending $HOSTED_API_KEY as the key)
  allow rule, run without asking: bash(cargo *)
  hook on post_tool [edit|write]: rustfmt "$ATELIER_FILE"
  also loaded, not gated (text for the model; a command file's shell lines ask before they run): 2 command file(s), AGENTS.md
project trust: review them, then /trust to accept (/trust show lists them again). Everything else works meanwhile
```

There is no dialog to answer; the notice stays in the scrollback. A project
whose only gated item is the auto-detected checkers gets a single line.

| Command | Does |
|---------|------|
| `/trust` | Shows the gated settings, accepts them as they are, and puts them into effect: MCP servers are started, checkers run from the next turn, allow rules and hooks apply, providers and the models configured on them are in use. Two things wait for the next session: a `yolo` default (`/mode yolo` switches now) and `session_start` hooks |
| `/trust show` | Lists the gated settings and whether they are in effect |
| `/trust revoke` | Forgets the decision. Checkers, allow rules and hooks stop and the MCP tools are withdrawn at once; a server already started, a provider or a mode the project configured are gone after a restart |
| `/hooks trust` | The same as `/trust` (hooks used to be trusted on their own) |

HTTP header values are not printed in the summary (they may be tokens), only
their names; read the file for the rest.

### What a decision covers

The gated settings are rendered in a canonical form and hashed (SHA-256);
the hash you accepted is stored per project path (the directory atelier was
started in, symlinks resolved) in `$XDG_STATE_HOME/atelier/trust.json`, else
`~/.local/state/atelier/trust.json` — outside the project, so neither the
repository nor the agent, whose file tools are confined to the project, can
accept anything. What goes into the hash, in the file's order:

- each `[[mcp]]`: `name`, `command`, `args`;
- each `[[mcp_http]]`: `name`, `url`, `headers`;
- each `[[diagnostics]]`: `name`, `command` — or, with none configured and a
  marker file present, the one fact that checkers are auto-detected (not
  which: the code they run changes with every pull anyway);
- each `[[provider]]`: `name`, `base_url`, `api_key_env`, `api_key_command`,
  `headers`;
- each `[permissions] allow` rule;
- `[permissions.reviewer]`, when enabled: `model`, `policy`, `when`,
  `timeout_secs`, and a hash of the policy file's contents;
- whether `[defaults]` starts sessions in `yolo`;
- each `[[hooks]]`: `event`, `match`, `command`, `timeout_secs`;
- `[search]` `provider`, `url` and `key_env`, when `key_env` or `url` is set;
- `[sandbox] enabled`, when it is set.

So a `git pull` that edits a hook's command, or the agent editing
`atelier.toml` to add an allow rule or rewriting the reviewer's policy file,
makes the project untrusted again at the next start, and the notice says the
settings changed. (Within a session the policy file is not re-read, so such
a rewrite changes nothing before then either.) Editing anything else
(`[review] enabled`, a `deny` rule, a checker's `when` globs) does not.

Trust is about the project's *settings*. Once you accept `cargo check` as a
checker, it runs whatever the repository's build scripts are after the next
pull, as it would if you ran it yourself.

### atelier's own edits

atelier rewrites `atelier.toml` when you tell it to: `/mcp add` and `/mcp
remove`, "always" at an approval prompt, `/review on`, the `/config` screen.
In a project that is trusted — or had nothing gated — the result is recorded
as trusted, so you are not asked about your own action; a project you set up
through `/mcp add` is trusted by that.

In a project that is *not* trusted, the edit is saved and nothing is
recorded: one `/mcp add` must not vouch for the rest of a file you have not
accepted. What you added works for the session (the server is connected, the
"always" rule counts), and the next start lists it with the rest until you
`/trust`.

## Headless runs and scripts

`--print` never asks. In an untrusted project the gated settings are skipped
with one line on stderr:

```
project trust: skipped 1 MCP server, 2 allow rules, 1 hook: this project is not trusted. Accept for this run with ATELIER_TRUST_PROJECT=1 (ATELIER_TRUST_HOOKS=1: its hooks alone), or for good with `atelier trust accept`
```

| | |
|---|---|
| `ATELIER_TRUST_PROJECT=1` | Accepts the project's gated settings, whatever they are, for that run; nothing is recorded. Set it only where you control the checkout (CI on your own repository) |
| `ATELIER_TRUST_HOOKS=1` | The same for the project's hooks alone, as before |
| `atelier trust status [path]` | Prints where the project stands, its gated settings, and one word on the last line: `trusted`, `not-needed`, `trusted-by-environment`, `untrusted` or `changed`. Exits 0 when the settings are in effect, 1 when not |
| `atelier trust accept [path]` | Accepts the project's settings as they are on disk now (after printing them) |
| `atelier trust revoke [path]` | Forgets the decision |

`path` is the project directory; the default is the current one.

`atelier permissions check` judges a call as a session started there would:
in an untrusted project the file's allow rules and mode default are not
counted, and it says so.

## Limits

- A decision is per directory atelier is started in. A subdirectory of a
  trusted project is another project (it also reads another `atelier.toml`).
- Running `atelier trust accept` is something the agent could do through
  `bash` — which asks first, unless you allowed it.
- The Claude backend's own configuration (`.mcp.json`, `.claude/settings.json`)
  is claude's to gate; atelier only withholds what it would pass along.
- Revoking mid-session cannot take back what already ran.
