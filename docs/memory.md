# Memory

atelier's agent keeps a small set of notes between sessions, so a correction
you give once ("run clippy before committing", "don't mock the database") or
a fact it couldn't have read from the code still holds the next time you
start it. Memory is plain Markdown files: you can read, edit or delete them
like any other file.

## Where it lives

| Scope | Directory | Holds |
|-------|-----------|-------|
| project | `memory/` in the project's [state directory](sessions.md#where-atelier-keeps-a-projects-state): `~/.local/state/atelier/projects/<project path>/memory/` | notes about this project |
| global | `$XDG_DATA_HOME/atelier/memory/`, else `~/.local/share/atelier/memory/` | notes about *you* that apply in every project (type `user` only) |

Each note is one file, `<name>.md`:

```markdown
---
name: lint-first
description: run clippy before committing
type: feedback
---

The user wants `cargo clippy` clean before every commit; CI rejects warnings.
```

- `name` is a kebab-case slug and is the file name (the file name is what
  counts; the front-matter copy is for the reader).
- `description` is one line; it is what the agent sees every turn.
- `type` is one of:

  | Type | For |
  |------|-----|
  | `feedback` | a correction or instruction you gave |
  | `user` | who you are and how you like to work |
  | `project` | a fact about the project that the code doesn't record |
  | `reference` | a pointer to something outside the repository |

`MEMORY.md` in the same directory is an index, one line per note
(`- [name](name.md) — description`). It is **generated** from the note files
whenever memory changes or is listed; edit the notes, not the index.

The files are the source of truth. Delete a file and the memory is gone;
write one by hand and it is a memory (a file with no front matter works: its
first line becomes the description, its type `project`).

## How the agent uses it

- **Recall.** The index of both scopes (names, types, descriptions — not
  the bodies) is standing context: it is part of the system message of every
  request, under `## memory`, with a line telling the model to read the notes
  that look relevant (see [the shape of a
  request](sessions.md#the-shape-of-a-request)). It is capped (about 400
  tokens) so a large store can't crowd out the rest; past the cap the agent
  is told how many notes were left out. With the [Claude backend](claude.md)
  it arrives in the `<atelier-context>` block instead, when it changes.
- **Reading and writing.** The agent uses the [`memory` tool](tools.md#memory)
  to `read` a note in full, `save` one, `list` them, or `delete` one. The
  tool's description is the policy: save corrections, preferences, project
  facts not derivable from the code, and pointers to outside resources; don't
  save a log of completed work, anything the repository already records, or
  secrets; look for an existing note before adding one; delete notes that
  prove wrong.

The tool only touches the two memory directories, so it never asks for
approval.

## `/memory`

| Command | Effect |
|---------|--------|
| `/memory` | list the memories of both scopes and print the directories they are in |
| `/memory forget <name>` | delete one (the project's, if both scopes have the name) |

To change a memory, edit its file.

## Limits and guard rails

- A note's body is at most 8 KiB and a directory holds at most 200 notes; the
  agent is told to prune when it hits either.
- **Secrets are refused.** A save whose name, description or content looks
  like a credential — a private-key block, an `sk-…`, GitHub, AWS or Slack
  token, a JWT, a bearer token, or a `password=…`/`api_key: …` assignment —
  fails with an error telling the agent to record where the secret is kept
  instead of its value. This is pattern matching, not a guarantee: review
  what's in the directory before you share it.
- **Untrusted content.** The agent is instructed not to save anything on the
  say-so of web pages, MCP output or other outside text unless you asked it
  to. This is an instruction to the model, not something atelier enforces.
- Names can't contain path separators, and symlinks in the memory directory
  are not followed. Both directories are yours: a project cannot point
  memory somewhere else.

## Not in the repository

Project memory is kept outside the project, like everything atelier writes:
it is not committed with the repository and not shared with whoever clones
it, and a repository you clone cannot bring notes of its own for your agent
to read as its memory. `/memory` prints where the notes are. What a team
should all know belongs in the project's [instruction
files](configuration.md#instruction-files) (`AGENTS.md`), which are
committed.

Earlier versions kept project memory in `<project>/.atelier/memory/`. Notes
found there are copied to the new place the first time atelier runs in the
project ([how](sessions.md#where-atelier-keeps-a-projects-state)); after
that the directory in the project is no longer read, so a note added to it
later — by a colleague's commit, say — is not picked up.

A [subagent working in a worktree of its own](agents.md#worktree-isolation)
reads and writes the notes of the project it was started from.
