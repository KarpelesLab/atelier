# Subagents

A subagent is a second conversation the agent starts for one piece of work:
"find every place the config is read", "fix the flaky test in `net/`". It runs
in a **fresh context** — it sees only the task it was given — uses its own
tools, and hands back one final report. The main conversation pays a
paragraph for what would otherwise have been forty file reads.

The model starts one with the [`task` tool](tools.md#task), naming an
**agent**: a system prompt plus limits (which tools, how many steps, which
model). Two are built in; you define more as Markdown files.

## Built-in agents

| Agent | Tools | For |
|-------|-------|-----|
| `explore` | `read`, `grep`, `glob`, `ls`, `tree`, `web_fetch`, `web_search` | Read-only search: where things are and how they work, reported with `file:line` references. Cannot modify anything |
| `general` | every tool the session has, except `task` | A self-contained multi-step task: research, an isolated change, running and fixing something |

`/agents` lists every agent available, where each is defined, its tools and
its step cap.

## Agents as files

An agent is one Markdown file, `<name>.md`, directly in one of:

| Scope | Directory |
|-------|-----------|
| project | `<project>/.atelier/agents/`, then `<project>/.claude/agents/` |
| user-global | `$XDG_CONFIG_HOME/atelier/agents/` (else `~/.config/atelier/agents/`) |

When two agents have the same name the first found in that order is used,
and any file beats a built-in — so a project can replace `explore` with its
own by defining `.atelier/agents/explore.md`. The directories are re-read at
every step, so a file you add or edit applies at once.

### A complete example

`.atelier/agents/test-fixer.md`:

```markdown
---
name: test-fixer
description: Runs the test suite, finds why a test fails and fixes the code (not the test). Use when a test is failing and the cause is not yet known.
model: qwen3-coder:30b
tools: read, grep, glob, edit, multiedit, bash, bash_output
max_steps: 30
---

You fix failing tests in this repository.

1. Run the failing test alone first (`cargo test <name> -- --nocapture`) and
   read the whole failure before opening any file.
2. Find the cause in the code under test. Change the test itself only when it
   is the test that is wrong, and say so explicitly.
3. Re-run the test, then the whole suite, before you finish.

End with a report: the cause in one sentence, the files you changed and why,
and the result of the final test run. If you could not fix it, say what you
ruled out.
```

The front matter sits between two `---` lines at the very top (`key: value`
lines, not full YAML); the text after it is the agent's **system prompt**.

| Key | Required | Meaning |
|-----|----------|---------|
| `description` | yes | What the agent does **and when to use it**. It is all the calling model knows about the agent, so it decides whether the agent is ever used |
| `name` | no | One word, at most 64 characters. Default: the file's name without `.md` |
| `model` | no | The model that serves this agent's requests — a model of your endpoint (or of your ChatGPT plan when signed in). Default: the session's model. `inherit` means the same |
| `tools` | no | The tools it may call: names, with `*` as a wildcard (`mcp__docs__*`). A comma list, a bracketed one (`[read, grep]`), or `- item` lines. Default: every tool the session has |
| `max_steps` | no | Model requests it may make before it is told to wrap up (see below). Default 40 |

Other keys are ignored. A file without a `description` or without a prompt is
skipped; `/agents` says which and why.

**No agent gets `task`**, whatever `tools` says: subagents do not start
subagents.

### `.claude/agents`

Claude Code's agent files are read as they are. Their `tools` name Claude's
tools (`Read`, `WebFetch`, `NotebookEdit`); these are matched to atelier's
(`read`, `web_fetch`, `notebook_edit`) ignoring case and underscores — a name
with no counterpart simply grants nothing. Their `model` (`sonnet`, `opus`,
`inherit`) is ignored: it does not name a model of atelier's endpoint.

## What a subagent has

| | |
|---|---|
| Conversation | Its own, starting at the task's prompt. Nothing of the parent's conversation, and the parent gets nothing of the child's but the final message |
| System prompt | The agent's, plus the project's instruction files (`AGENTS.md`) — the project's rules apply to it too. Not the per-turn snapshot (git status, diagnostics, layout): what matters of it belongs in the task's prompt |
| Tools | The parent's, filtered to the agent's `tools`. MCP servers are shared, not reconnected |
| Files | The same project directory, with its own read tracking: it must read a file before editing it, whatever the parent has read |
| Model | The agent's `model`, else the session's |
| Persistence | None. A subagent is never saved or resumed, and the [reviewer](review.md) does not watch it |

Some state is the project's, not the conversation's, and is therefore shared:
the `todo` list, the memory notes, and `bash` background jobs.

## Permissions

A subagent cannot do what its parent could not. It has no rules of its own:
every call it wants to make is decided by the parent session, under the same
[permission rules](permissions.md) — a `deny` holds, an `allow` lets it
through, and anything that would ask you asks you, in the same prompt, after
a line naming the agent:

```
⤷ the subagent general asks to run:
⚠ allow tool 'bash'?  {"command":"cargo test"}
```

"Always" saves the rule for the session and the project as usual. In headless
runs nobody can answer, so such calls are refused unless auto-approve is on —
exactly as for the main agent.

A rule can also govern `task` itself: `deny = ["task"]` switches subagents
off, `ask = ["task"]` asks before each one starts.

## The step cap

Each model request a subagent makes is a step. When it has used `max_steps`
of them without finishing, it gets **one more request with no tools** and the
instruction to stop and report what is done, what is not, and how to
continue. That report is the result — marked as a wrap-up in its footer — so
a task that was too big comes back as a partial answer the parent can act on,
not as an error.

## Several at once

`task` calls that follow one another in a single model step run **at the
same time**, and their results come back in the order they were called.
Calls of other tools stay sequential. Approval prompts from concurrent
subagents are asked one at a time.

Parallel subagents work in the same directory. Reading in parallel is always
safe; two subagents editing the same files will trip over each other (the
stale-edit check catches a file that changed since it was read, it does not
merge). Give parallel writers disjoint files.

## What you see

Scrollback only, no panes. One line when a subagent starts, its tool calls as
dim one-liners while it runs, one line when it ends — never its text or
reasoning, which the main agent summarizes for you:

```
⚙ task {"agent":"explore","prompt":"…","description":"find config readers"}
⤷ explore: find config readers
  ⤷ grep ATELIER_
  ⤷ read src/config.rs
⤶ explore: done (3 steps)
✓ The environment is read in two places: …
```

When several run together they are numbered (`explore#1`, `explore#2`) and
every line says whose it is. Ctrl-C stops the turn and the subagents with it.

In [headless](headless.md) `stream-json` output these lines are ordinary
`info` events between the `task` call's `tool_start` and `tool_end`.

The tool result the main agent gets is the subagent's final message and a
footer: `[explore: 3 steps · 412 output tokens]`. A subagent's output tokens
are added to the session's count.

## With the Claude backend

`task` and these agents belong to atelier's own loop. With `--claude`, claude
runs the loop and has subagents of its own (its `Agent` tool, which atelier
leaves on, and `.claude/agents`), so atelier's `task` tool is not offered to
it. `/agents` still lists what atelier would run.

## Not yet

Planned under the same roadmap item (M18), not in this version:

- **Background agents** — started and left to report when done.
- **Worktree isolation** — an agent working in its own git worktree, its diff
  handed back, so parallel writers cannot collide.
- **Subagents over the coordinator** — a child as another atelier instance
  (a process, or one you opened in another terminal), with messages between
  siblings.
- Per-agent reasoning effort and permission rules in the front matter.
- Hook events around a subagent's start and stop.
