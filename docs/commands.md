# Custom commands

A prompt you type often can live in a file and be run as a slash command.
The file is a Markdown template; its path is the command's name. Commands in
the repository apply to whoever opens it.

## Where they live

| Scope | Directory |
|-------|-----------|
| project | `<project>/.atelier/commands/` |
| project (read-compatible with Claude Code) | `<project>/.claude/commands/` |
| user-global | `$XDG_CONFIG_HOME/atelier/commands/`, else `~/.config/atelier/commands/` |

- `fix.md` is `/fix`; a file in a subdirectory, `git/sync.md`, is `/git:sync`
  (up to four levels deep; symlinked directories are not followed).
- When several files give the same name, the first directory in the table
  wins: the project before your global ones.
- **Built-in commands always win.** A `quit.md` or `help.md` is never
  reached, and is left out of `/help`.
- File and directory names may use letters, digits, `-`, `_` and `.`; others
  are skipped.
- Files are re-read each time, so an edit applies to the next use. `/help`
  lists the custom commands after the built-in ones.

## Format

```markdown
---
description: Fix a GitHub issue
argument-hint: <number> [notes]
---

Fix issue #$1.

The issue: !`gh issue view "$1"`
Current branch: !`git branch --show-current`

Follow the conventions in @CONTRIBUTING.md. Notes from me: $ARGUMENTS
```

Saved as `.atelier/commands/fix.md`, typing `/fix 42 keep the old API` sends
that text as your prompt, with the placeholders filled in. atelier prints one
line saying which file `/fix` expanded from.

The front matter (between two `---` lines, at the very top) is optional.
It is `key: value` lines, not full YAML:

| Key | Meaning |
|-----|---------|
| `description` | shown in `/help`. Without it, the template's first line is used |
| `argument-hint` | shown after the name in `/help` |

Other keys are ignored. That includes `model`, `agent` and `allowed-tools`
from other tools' command files: a command runs with the session's current
model and permissions. (Per-command model is not supported yet.)

## Placeholders

| In the template | Becomes |
|-----------------|---------|
| `$ARGUMENTS` | everything typed after the command name, exactly as typed |
| `$1` … `$9` | the arguments one by one, split like shell words (`/fix 42 "two words"` → `$2` is `two words`); a missing one is empty |
| `` !`command` `` | the output of `command` (see below) |
| `@path` | left as it is: the agent reads the file if it needs it |

If the template uses neither `$ARGUMENTS` nor `$1`…`$9` anywhere, whatever
you type after the name is appended as a final paragraph — so `/review only
the parser` works with a template that is just instructions.

Each placeholder is expanded once. What you type and what a command prints
are inserted as plain text: a `$1` or a `` !`…` `` appearing *in an argument
or in command output* is not expanded.

## Shell output: `` !`command` ``

The command runs with `sh -c` in the project root, and its standard output
replaces the placeholder. It has 10 seconds and 16 KiB: past either, the
output is cut and says so. If it fails, its output, exit status and standard
error are inserted instead, so the model sees what went wrong. It must be on
one line and cannot itself contain a backtick.

**This runs a shell because a file in the repository says so**, so it is
approved exactly like the [`bash` tool](permissions.md): unless auto-approve
is on (`ATELIER_APPROVE=all`, `[defaults] approve`) or you have answered
"always" for `bash`, atelier shows the command and asks — with the usual
risk signals — before running it. Refusing one cancels the whole command;
nothing is sent to the model.

Arguments are passed to the command as data, never pasted into it. The
command line is handed to the shell exactly as the template has it — which is
also what the approval prompt shows — with your arguments as the shell's own
positional parameters and `$ARGUMENTS` as an environment variable. So in

```markdown
Recent history: !`git log --oneline -5 -- "$1"`
```

`$1` is expanded by the shell, as a value: typing `/history "; rm -rf ~"`
passes `git log` one odd file name, not a second command. The only way an
argument reaches a command at all is the template placing `$1` (or
`$ARGUMENTS`) there. Quote it (`"$1"`) as in any shell script: unquoted, the
shell splits it into words and expands wildcards, though it still cannot
start another command. A template that does `` !`sh -c "$1"` `` or
`` !`eval $1` `` runs what you type, because that is what it says.

## Limits

- `--print` takes its prompt literally; slash commands are for the
  interactive TUI and REPL.
- A command typed while a turn is running is queued and runs when the turn
  ends, like the built-in ones.
- MCP prompts do not appear as commands.
