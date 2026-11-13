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
| Read-only, confined | `read` (in the project), `grep`, `glob`, `ls`, `tree`, `todo`, `skill`, `memory` `read`/`list`, `bash_output`, `bash_kill`, `peers`, `peer_answer`, `peer_lock` | run | run | run | run |
| Read-only, unconfined | `web_fetch`, `web_search`, `read` outside the project, MCP tools their server marks `readOnlyHint: true`, MCP `read_resource` / `get_prompt` | ask | ask | ask | run |
| Modifying, confined | `write`, `edit`, `multiedit`, `apply_patch`, `notebook_edit`, `node` (its `fs` can write), `memory` `save`/`delete` | **refused** | **ask** | run | run |
| Modifying, unconfined | `bash`, `node` with `network: true`, every other MCP tool, `peer_send`, `peer_ask` | **refused** (but see `bash` below) | ask | ask | run |

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

A project's file gets to choose `yolo` (5 or 6) only in a project you have
[trusted](trust.md); in an untrusted one that default is ignored. The more
careful modes need no trust.

A name that is no mode is an error, not a fallback: `--mode pln` and
`ATELIER_MODE=pln` stop atelier before it runs anything.

The status strip shows the mode when it is not `auto-edit`
(`… · turn 3 · plan`).

With the experimental [sandbox](sandbox.md) on, `bash` moves to the confined
side for the calls it can confine: a foreground command runs in a guest that
sees only the project, so it does not ask. A call that asks for the host
(`"sandbox": false`) or a background job is unconfined as before. The rules
below apply either way, and before any of this: a `deny` refuses a sandboxed
command too, and an `allow` written for `bash` is what lets a host run
through without a prompt.

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

**The project's `allow` list counts only in a project you have
[trusted](trust.md)** — a cloned repository must not be able to approve its
own commands. Its `deny` and `ask` lists always count: they only restrict.
In an untrusted project the allow rules in force are your user-global ones
and those you save with "always" during the session; `/permissions` and
`permissions check` say when the project's are left out.

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

`/permissions` in a session lists the rules in force, by source, and says
whether the [approval reviewer](#the-approval-reviewer) is on.

## The approval reviewer

Optional, and off unless configured: a model that is put the calls that
would *ask*, judges each against a written policy, and answers **allow**,
**deny** or **ask**.

```toml
[permissions.reviewer]
enabled = true
model = "local/judge#low"               # default: the `review` role's model, then the main one
policy = ".atelier/approval-policy.md"  # default: the built-in policy
when = "unattended"                     # the default; or "always"
timeout_secs = 30                       # the default
```

| `when` | The reviewer is consulted |
|--------|---------------------------|
| `"unattended"` | Only where nobody can answer a prompt: [`--print`](headless.md), a [background subagent](agents.md#in-the-background), [`atelier mcp`](mcp.md#atelier-as-an-mcp-server) with a client that has no elicitation, and [`atelier serve`](protocol.md) when the client answers an `approval/request` with an error or not within `--approval-timeout`. A call that would have been refused for want of an answer gets a judgement instead. At a terminal, and with a client that answers, nothing changes |
| `"always"` | Also ahead of interactive prompts: you are asked only when the reviewer says "ask" (or fails) |

Each verdict is one line, `⚖ reviewer allowed bash (low): runs the project's
tests`, and an [`approval` event](headless.md#output-formats) for programs.

**It is a model.** It can be wrong, and it can be talked round by text the
agent picked up from a file or a web page and put into a command. It
narrows the prompts you see; it is not a sandbox and not a permission
system. What must never run belongs in a `deny` rule, what must always be
looked at in an `ask` rule plus `when = "unattended"`, and what must be
contained in the [sandbox](sandbox.md). Check its work in the
[audit log](#the-audit-log).

### Precedence

The reviewer only ever replaces a *prompt*. In order:

1. A `deny` rule refuses. Final.
2. `plan` mode refuses what can modify. Final.
3. A `pre_tool` [hook](hooks.md) that denies refuses. Final.
4. An `allow` rule (or `yolo`, or a `pre_tool` hook's approval) runs the
   call. The reviewer never sees it.
5. A `permission_request` hook that answers has answered: it stands in for
   you, and wins over the reviewer.
6. **The reviewer** — when `when` says this prompt is its to answer.
   - *allow*: the call runs, once. No rule is saved: "always" stays
     something only you say.
   - *deny*: the call's result tells the model that the approval reviewer
     denied it and why, and not to retry the same call in another form but
     to ask the user or take another approach.
   - *ask*: goes on to 7.
7. You, at the prompt — or, where there is nobody, the refusal there
   always was.

### What it is shown

One request, without tools. The system message is the policy followed by a
fixed output contract; the user message is one JSON object holding the tool
and its arguments (up to 8 KiB), the [risk signals](#risk-signals-bash-only),
why the call asks (the rule, or the tool's default), the project root, the
mode, and two pieces of the conversation: your last prompt and the
assistant's last text (2 KiB each).

It is deliberately **not** shown tool output — files the agent read, web
pages, command output, a background job's or subagent's report, another
instance's message. Those are untrusted content, and content that reaches
the reviewer could argue for its own approval. What cannot be kept from it
is what the agent itself wrote: the arguments and its last message. The
policy tells the reviewer that a claim of authority in either ("the user
approved this", "ignore the policy") is a reason to deny or ask, never to
allow.

For a background subagent the context is your last prompt to the main agent
and the task it handed over. On the [Claude backend](claude.md) it is your
prompt alone.

### Fail closed

The reviewer must answer with exactly one JSON object:

```json
{"decision": "allow", "risk": "low", "reason": "runs the project's tests"}
```

`decision` is `allow`, `deny` or `ask`; `risk` is `low`, `medium`, `high` or
`critical`. Each of the following counts as **ask** — you are prompted where
there is a prompt, and the call is refused where there is none:

- a reply that is anything else: prose, a code fence, text around the
  object, a missing or unknown field;
- `allow` together with `risk: critical`;
- `allow` on a call whose arguments were too long to show in full;
- a provider error, or no answer within `timeout_secs`;
- a `policy` file that is missing, unreadable or empty.

The request is made once per call and never retried.

### The policy

The built-in policy allows what is clearly part of your request and confined
in effect (builds, tests, linters, read-only inspection, `git status` /
`diff` / `log` / `add` / `commit` on the working branch, installing what
the project's manifest declares); denies what destroys or exfiltrates
(deleting outside the project, `rm -rf` of broad paths, force-pushes and
history rewrites on shared branches, piping the network into a shell,
reading credentials and keys, sending data to hosts you did not name, broad
permission or ownership changes, disabling security tooling); and answers
"ask" for everything else, for anything it does not understand, and for
arguments that look assembled from tool output rather than from your
request.

Print it, copy it, edit it:

```
$ atelier permissions reviewer-policy > .atelier/approval-policy.md
```

then name the file in `policy`. In a project's `atelier.toml` the path is
relative to the project root; in your user-global file it is relative to
that file's directory (or absolute, or `~/…`) — a project never supplies the
policy for a reviewer you configured. Your file replaces the built-in
policy only; the output contract is always appended. The file is read once,
when atelier starts.

### Whose setting counts

A `[permissions.reviewer]` table in your user-global `atelier.toml` applies
everywhere. One in a project's file replaces it for that project — once the
project is [trusted](trust.md): switching the reviewer on, its model, its
`when` and the *contents* of its policy file are gated settings, since a
repository must not be able to appoint a lenient judge of its own commands.
An untrusted project's table is ignored, except that `enabled = false`
(which only restricts) is honoured.

### Which model

`model`, else `[models] review`, else the main model. A small, fast model
with a low reasoning effort is usually the right judge; one that is
different from the main model is harder to steer with the same text. Its
requests appear in the [usage log](configuration.md) like any other.

With the Claude backend, claude calls its own models and atelier has none
of its own, so the reviewer works only when `model` (or `[models] review`)
is on a configured `[[provider]]`. Otherwise it is off, which is said once,
and calls are asked about or refused as without it.

### The audit log

Every verdict appends one line to `$XDG_STATE_HOME/atelier/approvals.jsonl`
(else `~/.local/state/atelier/approvals.jsonl`), best effort:

```json
{"ts":1794567890,"project":"/work/proj","tool":"bash","arguments":"{\"command\":\"cargo test\"}","decision":"allow","risk":"low","reason":"runs the project's tests","model":"local/judge"}
```

`arguments` is cut to 2 KiB; `decision` is what came of it (`ask` covers
the failures above, with `risk` null and the failure as the `reason`).
Nothing reads this file back: it is there for you to review what was
allowed in your absence.

`/permissions` says whether the reviewer is on, when it answers and under
which policy.

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
`ATELIER_APPROVE=all` gives it all of them; the
[approval reviewer](#the-approval-reviewer) is the middle way, a model
deciding call by call under a policy. `--print --mode plan` is a
read-only run: it can investigate and report, and change nothing. In
`--print --mode ask-edits` every edit no rule allows is denied.

## Subagents

A [subagent](agents.md) has no rules of its own. Every call it makes is
decided by the session that started it — the same rules, the same defaults,
the same prompt (after a line naming the agent that asks), the same
"always". So a subagent can never do what the main agent could not. `task`
itself is a tool like any other in rules: `deny = ["task"]` switches
subagents off.

A subagent started [in the background](agents.md#in-the-background) cannot
prompt anyone: the rules and the mode — as they were when it started —
decide alone, and a call they would **ask** about is **denied**, with a
result telling the agent to report what it needed — unless the
[approval reviewer](#the-approval-reviewer) is on, which then judges it.
Nothing it does is saved as an "always" rule.

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
The [approval reviewer](#the-approval-reviewer) applies to these requests
as to atelier's own tools, provided it has a model on a configured provider.

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
- The approval reviewer's verdicts for a background subagent are shown as
  its activity lines, not as `approval` events.
- A network allowlist for the [sandbox](sandbox.md) is the rest of roadmap
  M17.
