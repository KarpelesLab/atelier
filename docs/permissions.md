# Permissions

atelier's approval model is keyed on **confinement**, not on whether an
action mutates something: a tool that provably cannot leave the project
directory runs without asking; a tool that can run arbitrary, unbounded code
always asks (or is pre-authorized). [Rules](#rules) refine that per call:
allow this command, ask before reading that file, never do the other. A
[mode](#modes) moves the whole line: `plan` refuses changes, `ask-edits`
asks before them, `yolo` asks about nothing.

## Confined vs. unconfined

- **Confined = automatic.** `read`, `write`, `edit`, `multiedit`, `grep`,
  `glob`, and `ls` all resolve paths through `tools::confine`
  (`ToolCtx::resolve`), which rejects anything that normalizes outside the
  project root. Because they cannot escape, they override
  `Tool::requires_approval` to `false` and never prompt — including `write`
  and `edit`/`multiedit`. The `node` tool's `fs` is confined
  the same way (see [Scripting](scripting.md)), so a plain `node` call (no
  `network`) is also auto-approved.
- **Unconfined = ask.** `Tool::requires_approval` defaults to `true`, and
  only the tools above override it. That leaves:
  - **`bash`** — runs anything via `sh -c`.
  - **`web_fetch`** and **`web_search`** — reach the network: a URL or a
    query leaves the machine, and what comes back is untrusted text.
  - **Every MCP tool** (`mcp__<server>__<tool>`) — `McpTool` doesn't override
    the default, so any tool from any connected server always requires
    approval, regardless of what it actually does.
  - **`node` with `network: true`** — opts into `fetch`/`httpGet`/
    `httpRequest`, at which point the call is treated as unconfined
    (`NodeTool::requires_approval` returns `true` exactly when `network` is
    set).

## Modes

A **mode** sets how much runs without asking where no rule decides. There
are four, from the most careful to the least:

| Mode | What it is for |
|------|----------------|
| `plan` | Read-only. The agent investigates and proposes; anything that can change something is refused, with a message telling the model to present a plan. Switch modes to let it carry the plan out. |
| `ask-edits` | Every file change asks first, like commands do. |
| `auto-edit` | **The default.** File changes inside the project run; commands and the network ask. |
| `yolo` | Nothing asks. What `ATELIER_APPROVE=all` has always done. |

What each mode does with each class of tool, when no rule matches the call:

| Tool class | Tools | `plan` | `ask-edits` | `auto-edit` | `yolo` |
|------------|-------|--------|-------------|-------------|--------|
| Read-only, confined | `read` (in the project), `grep`, `glob`, `ls`, `tree`, `todo`, `skill`, `memory` `read`/`list`, `bash_output`, `bash_kill`, `peers` | run | run | run | run |
| Read-only, unconfined | `web_fetch`, `web_search`, `read` outside the project, MCP tools their server marks `readOnlyHint: true`, MCP `read_resource` / `get_prompt` | ask | ask | ask | run |
| Modifying, confined | `write`, `edit`, `multiedit`, `apply_patch`, `notebook_edit`, `node` (its `fs` can write), `memory` `save`/`delete` | **refused** | **ask** | run | run |
| Modifying, unconfined | `bash`, `node` with `network: true`, every other MCP tool, `peer_send` | **refused** (but see `bash` below) | ask | ask | run |

And with [rules](#rules):

- A **deny** rule refuses in every mode, `yolo` included.
- An **ask** rule prompts in every mode but `yolo` — except that in `plan`
  mode a modifying call is refused rather than asked about: nobody is asked
  to approve a change there.
- An **allow** rule runs the call without asking in `ask-edits`,
  `auto-edit` and `yolo`. That is how `ask-edits` is tuned:
  `allow = ["edit(docs/**)", "write(docs/**)"]` lets the documentation be
  edited freely while every other change asks. Answering "always" to an
  `ask-edits` prompt saves a rule for that one file.
- In **`plan`** mode an allow rule does not make a modifying tool available,
  with one exception: **`bash` runs a command that allow rules *with a
  pattern* cover in full** — `allow = ["bash(git log *)", "bash(rg *)"]`
  keeps those usable while planning. A whole-tool rule (`allow = ["bash"]`)
  does not count: it names no command, so it says nothing about what is
  safe to run in a read-only mode. Every other `bash` call is refused, and
  the refusal says so. Since an allow rule never covers a command that
  redirects output to a file, sets variables or cannot be followed (see
  [`bash` patterns](#bash-patterns)), those are refused too — but atelier
  does not know what a program does: `bash(cargo *)` lets `cargo build`
  write `target/` in plan mode. Allow, for planning, the commands you
  consider read-only.

A refused call in plan mode returns, as the tool result:

```text
error: plan mode: write is not available — investigate with the read-only tools, then present a plan; the user will switch modes to let you carry it out.
```

The model is also told up front: while plan mode is on, a standing line in
its context says which tools are refused and what is expected of it (and one
line says so when plan mode ends).

Whether a tool can modify something is the tool's own answer
(`Tool::mutates`, default `true`), separate from confinement: `write` is
confined yet modifies; `web_fetch` leaves the machine yet modifies nothing.
An MCP tool counts as modifying unless its server's `tools/list` entry
carries `annotations.readOnlyHint: true` — the server's word is taken for
it, and the call still asks.

### Selecting a mode

In order of precedence:

1. `/mode <name>` during a session (`/mode` alone prints the current mode
   and the list). Not saved.
2. `--mode <name>` on the command line.
3. `ATELIER_MODE=<name>`.
4. `ATELIER_APPROVE=all` (or `yes`, `1`) — `yolo`.
5. `[defaults] mode = "<name>"` in the project's `atelier.toml`.
6. `[defaults] approve = true` — `yolo` (what the `/config` screen's
   auto-approve switch sets).
7. Otherwise `auto-edit`.

A name that is no mode is an error, not a fallback: `--mode pln` and
`ATELIER_MODE=pln` stop atelier before it runs anything.

The status strip shows the mode when it is not `auto-edit`
(`… · turn 3 · plan`).

## Rules

On top of those defaults, `[permissions]` in `atelier.toml` holds three lists
of rules. A rule is `tool` or `tool(pattern)`; the tool part may use `*`
(`mcp__github__*`).

```toml
[permissions]
allow = ["bash(git diff *)", "bash(cargo *)", "mcp__github__*", "web_fetch(domain:docs.rs)"]
ask   = ["read(.env*)", "bash(git push *)"]
deny  = ["read(~/.ssh/**)", "bash(rm -rf *)", "write(.github/**)"]
```

**Decision order:** any matching `deny` refuses the call — the model is told
which rule, and not to retry it in another form; else any matching `ask`
prompts, even for a call that would otherwise run unasked (`read` of a
project file); else a matching `allow` runs it; else the default above
applies, as the [mode](#modes) has it. A bare tool name (`allow = ["bash"]`) covers the whole tool, as it
always has.

**User-global rules** live in `$XDG_CONFIG_HOME/atelier/atelier.toml` (else
`~/.config/atelier/atelier.toml`), same `[permissions]` table. The lists are
unions of both files, so the order above holds across them: a deny from
either file beats an allow from either.

A rule that does not parse is ignored and reported (once per session, in
`/permissions`, and by `permissions check`) — check after editing by hand.

### What a pattern matches

| Tool | Subject | Pattern |
|------|---------|---------|
| `bash` | the command line | see below |
| `read`, `write`, `edit`, `multiedit`, `apply_patch`, `notebook_edit` | the path, absolute and lexically normalized (`a/../../x` is judged as where it lands; symlinks are not followed — the same reading as `tools::confine`) | a glob: `*` within a path component, `**` across them. `~/…` is your home, `/…` absolute, a pattern containing `/` is relative to the project root, one without (`.env*`, `*.pem`) names a file in any directory. A pattern naming a directory covers everything under it |
| `web_fetch` | the URL | `domain:example.com` (that host and its subdomains), or a `*` wildcard over the whole URL |
| anything else (MCP tools, `node`, …) | the arguments as compact JSON, keys sorted | `*` matches any run of characters |

`apply_patch` touches several paths: one matching is enough to deny or ask;
all must match to allow.

### `bash` patterns

- `*` matches any run of characters. A **trailing** `*` means "and any
  arguments": what precedes it must be whole words. `git diff *` and
  `git diff*` are the same rule — both match `git diff` and
  `git diff --stat`, neither matches `git diffx`.
- To match the tail of a word at the end, write `**`: `./scripts/**`.
- No trailing `*`: the whole command must match (`git status` does not match
  `git status -s`).
- Matching is on the command's words with the shell's quoting undone and
  single spaces between them, so `gi""t 'diff'` is `git diff`. Write patterns
  without quotes.

**Compound commands are taken apart.** `&&`, `||`, `;`, `|`, `&`, newlines,
`$(…)`, backticks and `sh -c '…'` / `bash -lc "…"` each yield simple
commands, and every one is judged:

- **deny / ask** fire if *any* part matches. They also look through
  `VAR=x` prefixes, a program's directory (`/bin/rm`), and wrappers (`sudo`,
  `env`, `xargs`, `timeout`, `nice`, `nohup`, `command`, `exec`, `time`, …),
  and are tried on the raw line. They err on the side of matching:
  `sudo echo rm x` is caught by `bash(rm *)`.
- **allow** needs *every* part covered by an allow rule. Only `command`,
  `builtin`, `exec`, `time` and `nohup` (without options) are looked through,
  and a plain `sh -c '<script>'` counts as its script. A part is never
  auto-allowed when it
  - redirects output to a file (`> out`; `2>&1` and `>/dev/null` are fine),
  - sets variables (`PATH=/x git diff`) — unless the rule spells the
    assignment out (`bash(RUST_BACKTRACE=1 cargo test *)`),
  - runs a computed command name (`$CMD …`, `` `which x` … ``, `{a,b} …`),
  - names its program by another path (`./git diff` is not `git diff`),
  - sits in shell control flow (`if`, `for`, `while`, `{ …; }`, `!`).

So with `allow = ["bash(git diff *)"]`: `git diff --stat` runs;
`git diff; rm -rf ~`, `git diff $(curl x | sh)` and `git diff > f` ask.

**A command that cannot be followed is never auto-allowed** by a pattern
rule — it asks. That covers unbalanced quotes, backslash escapes, heredocs
and herestrings, process substitution, subshells `( … )`, function
definitions, `case`, arithmetic `$(( ))`, `$'…'`, `${…}` with anything nested
in it, comments, and redirections to a quoted or computed target. Deny and
ask rules still apply to it, through a cruder reading (cut at every
operator-like character, quotes dropped).

Not understood, by design — rules are a convenience, not a sandbox:

- What the shell computes. `X=rm; $X -rf y` and `{rm,-rf,y}` are not caught
  by `deny = ["bash(rm *)"]` (nor would a pattern rule auto-allow them). A
  deny rule on `bash` stops the model's ordinary attempts, not a determined
  one.
- What a program does with its arguments. `bash(find *)` allows
  `find -exec …`, `bash(git *)` allows `git -c alias.x='!sh' x`,
  `bash(make *)` runs the Makefile. Allow programs, and subcommands, you
  would run unread.

## The approval prompt

When a call needs approval, the agent first prints the rule that asked (if
one did) and any [risk signals](#risk-signals-bash-only), then asks via
`Ui::ask_approval`:

| Answer     | Effect |
|------------|--------|
| **Once**   | Run this one call; ask again next time. |
| **Always** | Run it, and save the **narrowest rule** that lets it through to `[permissions] allow` in the project's `atelier.toml`. The saved rule is printed. |
| **Deny**   | Refuse. The model receives `"error: the user denied permission to run this tool."` and can adapt. |

What "always" saves:

- **`bash`** — the first word of the command plus ` *`, or the first two for
  programs with subcommands (`git`, `cargo`, `npm`, `docker`, …):
  `cargo test --lib` → `bash(cargo test *)`, `git status` →
  `bash(git status)`, `ls -la` → `bash(ls *)`. A compound command gets one
  rule per part not already allowed. Destructive programs (`rm`, `chmod`,
  `kill`, `dd`, …) get the exact command only.
- **Nothing**, for calls no narrow rule describes — they run once: open-ended
  launchers (`sh`, `bash`, `env`, `xargs`, `sudo`, `eval`, `ssh`,
  `python -c`, `node -e`, `find -exec`, …), commands that cannot be followed
  or that an allow rule could never cover (see above), and calls an `ask`
  rule prompted for. Write a rule by hand if you want one.
- **MCP tools and `node`** — the tool name.
- **`web_fetch`** — `web_fetch(domain:<host>)`. **`read`** outside the
  project — that exact path.

## Inspecting rules

```console
$ atelier permissions check -- git diff --stat
decision: allow
rule:     bash(git diff *) (project, /work/proj/atelier.toml)
  `git diff --stat` is allowed by `bash(git diff *)`

$ atelier permissions check -- 'cargo test 2>&1 | tail && make'
decision: ask
rule:     none — `bash` asks by default
  `cargo test` is allowed by `bash(cargo *)`
  no allow rule covers `tail`
  no allow rule covers `make`

$ atelier permissions check --tool read --args '{"path": "/home/me/.ssh/id_rsa"}'
decision: deny
rule:     read(~/.ssh/**) (user, /home/me/.config/atelier/atelier.toml)
```

Run it from the project directory. Words after `--` are joined into one
command; quote it as a single argument when it holds shell syntax.
`--tool <name> [--args '<json>']` checks any other tool (MCP servers are not
connected for this, so an unmatched MCP tool reports the default: ask).

`--mode <name>` judges the call under that mode (without it: the mode a
session started in this directory would be in), and a `mode:` line says so
when the mode is what decided:

```console
$ atelier permissions check --mode plan --tool write --args '{"path": "src/a.rs"}'
decision: deny
rule:     none — `write` runs without asking by default
mode:     plan — read-only: this call can modify something

$ atelier permissions check --mode plan -- git log -3
decision: allow
rule:     bash(git log *) (project, /work/proj/atelier.toml)
  `git log -3` is allowed by `bash(git log *)`
```

`/permissions` in a session lists the rules in force, by source.

## Headless mode: `ATELIER_APPROVE` and `--print`

Set `ATELIER_APPROVE=all` (also accepts `yes` or `1`) to answer every
approval prompt with yes for the session — for CI or scripted/unattended
runs. This is the `yolo` [mode](#modes). It covers `ask` rules and the
defaults; **`deny` rules still refuse**.

`--print`/`-p` (see [Quickstart](quickstart.md)) runs one prompt to
completion with no human able to answer a prompt, so its `Ui::ask_approval`
always answers **Deny** rather than blocking on stdin — the model is told the
tool was denied and can try to proceed without it. Allow rules are how a
`--print` run gets exactly the unconfined calls it needs;
`ATELIER_APPROVE=all` gives it all of them. `--print --mode plan` is a
read-only run: it can investigate and report, and change nothing. In
`--print --mode ask-edits` every edit no rule allows is denied.

## Subagents

A [subagent](agents.md) has no rules of its own. Every call it makes is
decided by the session that started it — the same rules, the same defaults,
the same prompt (after a line naming the agent that asks), the same
"always". So a subagent can never do what the main agent could not. `task`
itself is a tool like any other in rules: `deny = ["task"]` switches
subagents off.

## The Claude backend

Atelier's own tools reach claude as `mcp__atelier__<tool>` and run through
the same `ToolRunner`, so the rules above apply to them unchanged (under
their plain names: `bash(…)`, not `mcp__atelier__bash(…)`), and so does the
mode.

Claude's *built-in* tools are switched off in favour of atelier's by default;
those that remain (`Agent`, `WebSearch`, …), and all of them with
`ATELIER_CLAUDE_BUILTINS=keep`, ask through claude's permission requests.
atelier answers a request with the same rules and mode, after mapping
claude's tool to the one the rules know:

| claude's tool | judged as | modifies |
|---------------|-----------|----------|
| `Bash` | `bash` (the `command`) | yes |
| `Edit`, `Write`, `MultiEdit`, `NotebookEdit` | `edit`, `write`, `multiedit`, `notebook_edit` (the `file_path` / `notebook_path`) | yes |
| `Read`, `NotebookRead` | `read` (the path) | no |
| `WebFetch`, `WebSearch` | `web_fetch` (the `url`), `web_search` | no |
| `Glob`, `Grep`, `LS`, `TodoWrite`, `BashOutput`, `KillShell` | `glob`, `grep`, `ls`, `todo`, `bash_output`, `bash_kill` (arguments as JSON) | no |
| an MCP tool | its own name, `mcp__<server>__<tool>` | yes |
| anything else (`Agent`, `Skill`, …) | its name in lower case (`agent`), arguments as JSON | yes |

So `deny = ["bash(rm *)"]` refuses claude's `Bash` running `rm` (claude is
given the same message naming the rule), `allow = ["bash(git diff *)"]`
answers yes without a prompt, and in `plan` mode claude's `Edit`, `Write`
and `Bash` are refused with the plan-mode message. A request no rule
decides is put to you — claude asked, so it is never waved through outside
`yolo` — and "always" saves an atelier rule to `atelier.toml`, exactly as
for atelier's own tools (claude's own permission settings are not changed).

**The limit:** atelier only sees the calls claude *asks* about. A call that
claude's own configuration already allows — its `permissions.allow`
settings, a `Read` inside the working directory — runs without a request,
and no atelier rule or mode is consulted. With claude's built-ins kept, do
not rely on atelier's deny rules or `plan` mode for tools claude is
configured to run unasked; the default (built-ins replaced by atelier's
tools) has no such gap for the tools it replaces.

## Risk signals (`bash` only)

Before asking for approval on a `bash` call, `src/risk.rs` scans the command
string for patterns worth calling out explicitly, so the prompt shows *why*
something is risky rather than just an opaque command line. Each matched
category prints as `⚠ <signal>` above the prompt:

| Signal | Detects |
|--------|---------|
| recursive force delete (`rm -rf`) | `rm` with both a recursive and a force flag, or a recursive `rm` targeting `/` or `/*` |
| runs with elevated privileges (sudo) | `sudo`, `doas`, or `su` |
| network access | `curl`, `wget`, `nc`, or `ftp` |
| pipes downloaded content into a shell | a pipe (`\|`) into `sh`/`bash`/`zsh`/`dash`/`ksh` (optionally through `sudo`/`env`) |
| touches paths outside the project | a mutating command (`rm`, `mv`, `cp`, `chmod`, `chown`, `chgrp`, `dd`, `tee`, `mkdir`, `rmdir`, `ln`, `shred`, `truncate`) or a `>`/`>>` redirect targeting an absolute, `~`-relative, or `..`-traversing path |
| modifies shell startup files | a path containing `.bashrc`, `.zshrc`, `.profile`, or `.bash_profile` |
| changes file permissions/ownership | `chmod` or `chown` |
| raw disk/device operation | `dd`, any `mkfs*`, or a redirect into `/dev/*` |
| possible fork bomb | the literal `:(){` pattern |
| destructive git operation | `git push --force`/`-f`, `git reset --hard`, or `git clean` with a force flag |

Signals are informational only — they don't block the call or change whether
approval is required, and they only ever apply to `bash` (MCP and `node`
calls show no risk signals today). A command can trigger multiple distinct
signals at once (each category is reported at most once per call).

## Scope not yet covered

- On the Claude backend, rules and modes reach claude's built-in tools only
  through its permission requests: what claude's own settings allow runs
  unseen (see above).
- `web_fetch` rules judge the URL asked for; redirects are followed without
  being re-checked.
- `grep`, `glob`, `ls` and `tree` are matched on their JSON arguments, not as
  paths.
- MCP tools have no risk-signal analysis.
- In `plan` mode, `bash` commands covered by a pattern allow rule run
  unexamined: atelier does not check that they only read.
- A sandbox for `bash`, an approving reviewer and project trust are the
  rest of roadmap M17.
