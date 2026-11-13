# Skills

A skill is a directory of instructions — often with scripts and reference
files — for one kind of task: cutting a release, filling in a PDF form,
writing a migration. The agent is told only each skill's name and
description; it loads the instructions when a task calls for them. So you can
install many skills and pay a line of context for each, not a page.

The format is the `SKILL.md` one shared by Claude Code, opencode and Codex;
their skill directories are read as they are.

## Where they live

A skill is a directory containing a `SKILL.md`, directly under one of:

| Scope | Directories, in order |
|-------|-----------------------|
| project | `<project>/.atelier/skills/`, `<project>/.claude/skills/`, `<project>/.agents/skills/` |
| user-global | `$XDG_CONFIG_HOME/atelier/skills/` (else `~/.config/atelier/skills/`), `~/.claude/skills/`, `~/.agents/skills/` |

When two skills have the same name, the first one found in that order is
used: the project's before your global ones. The directories are re-read
every time, so a skill you add or edit is available at once.

## A complete example

```
.atelier/skills/release/
├── SKILL.md
├── checklist.md
└── scripts/
    └── bump-version.sh
```

`SKILL.md`:

```markdown
---
name: release
description: Cut a release — bump the version, update the changelog, tag. Use when asked to release or publish a new version.
---

# Releasing

1. Make sure the tree is clean and the tests pass.
2. Run `scripts/bump-version.sh <major|minor|patch>` from this skill's
   directory; it edits `Cargo.toml` and prints the new version.
3. Move the "Unreleased" entries in `CHANGELOG.md` under the new version.
4. Go through `checklist.md` before tagging.
5. Commit as `chore: release vX.Y.Z` and tag `vX.Y.Z`. Do not push.
```

The front matter (between two `---` lines at the very top; `key: value`
lines, not full YAML) must give:

| Key | Meaning |
|-----|---------|
| `name` | what the skill is called: one word, at most 64 characters. Conventionally the directory's name |
| `description` | one line saying what it does **and when to use it** — this is all the agent sees until it loads the skill, so it decides whether the skill is ever used |

Other keys are ignored. A `SKILL.md` without both is skipped; `/skills` says
which were skipped and why.

## How the agent uses it

- **Advertised.** Every turn, the project context has a `## skills` item:
  `name — description` for each skill, and a line telling the model to load a
  skill before starting a task it covers. It is capped (about 300 tokens,
  and 200 characters per description); past the cap the agent is told how
  many were left out and how to list them. Project skills come first, so they
  are the ones that survive the cap.
- **Loaded on demand.** The [`skill` tool](tools.md#skill) returns the body of
  `SKILL.md`, the skill's directory and a list of its other files. The agent
  then reads those files or runs those scripts as the instructions say.
- **Its files.** `skill` with a `file` argument returns one of the skill's
  own files. This is how a *global* skill's reference files are read: they
  are outside the project, where [`read` needs your approval](permissions.md).
  The `skill` tool never asks, and in exchange reads only inside the skill's
  own directory — `..` and symlinks that lead out of it are refused.
  Running a skill's script is a `bash` call and is approved like any other.

`/skills` lists the skills found, each with its directory.

## Trust

A skill's instructions are followed by the agent like your own prompt, and
its scripts are run if the instructions say so (subject to `bash` approval).
Install skills from sources you would run code from. A project skill must
resolve inside the project: a skill directory in the repository that is a
symlink to somewhere else on your disk is skipped. Among your global skill
directories, symlinks are fine — that is a common way to install them.

## With the Claude backend

The list arrives in the `<atelier-context>` block and `skill` is one of the
tools atelier offers claude. claude also discovers `.claude/skills/` by
itself, so skills there may be announced twice; either way of loading them
works.
