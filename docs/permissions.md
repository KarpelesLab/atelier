# Permissions

atelier's approval model is keyed on **confinement**, not on whether an
action mutates something: a tool that provably cannot leave the project
directory runs without asking; a tool that can run arbitrary, unbounded code
always asks (or is pre-authorized). [Rules](#rules) refine that per call:
allow this command, ask before reading that file, never do the other.

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
applies. A bare tool name (`allow = ["bash"]`) covers the whole tool, as it
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

`/permissions` in a session lists the rules in force, by source.

## Headless mode: `ATELIER_APPROVE` and `--print`

Set `ATELIER_APPROVE=all` (also accepts `yes` or `1`) to answer every
approval prompt with yes for the session — for CI or scripted/unattended
runs. That covers `ask` rules and the defaults; **`deny` rules still refuse**.

`--print`/`-p` (see [Quickstart](quickstart.md)) runs one prompt to
completion with no human able to answer a prompt, so its `Ui::ask_approval`
always answers **Deny** rather than blocking on stdin — the model is told the
tool was denied and can try to proceed without it. Allow rules are how a
`--print` run gets exactly the unconfined calls it needs;
`ATELIER_APPROVE=all` gives it all of them.

## The Claude backend

Atelier's own tools reach claude as `mcp__atelier__<tool>` and run through
the same `ToolRunner`, so the rules above apply to them unchanged (under
their plain names: `bash(…)`, not `mcp__atelier__bash(…)`). Claude's
*built-in* tools (its own `Bash`, `Edit`, …) are approved through claude's
permission requests, which do not consult these rules yet.

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

- Rules do not reach the Claude backend's built-in tools (see above).
- `web_fetch` rules judge the URL asked for; redirects are followed without
  being re-checked.
- `grep`, `glob`, `ls` and `tree` are matched on their JSON arguments, not as
  paths.
- MCP tools have no risk-signal analysis.
- Modes (`plan`, …), a sandbox for `bash`, an approving reviewer and
  project trust are the rest of roadmap M17.
