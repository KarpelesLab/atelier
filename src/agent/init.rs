//! `/init`: have the agent write the repository's first `AGENTS.md`.
//!
//! This is a prompt, not code that guesses. What belongs in an instruction
//! file depends on the project — its build, its conventions, where it bites —
//! and finding that out means reading it, which is what the agent loop and
//! its tools are for. So the command expands into the brief below and the
//! turn runs like any other: the file is written with the `write` tool,
//! under the usual permissions, and the user sees every step.
//!
//! The brief is mostly about what to leave out. An instruction file is sent
//! with every request (see `context::instructions`), so a line that tells
//! the model nothing new costs tokens on every turn for as long as the file
//! lives.

/// The brief `/init` sends.
const BRIEF: &str = "\
Write an `AGENTS.md` for this repository: the standing instructions a coding \
agent is given before every task here.

First look at the repository. Read the README, the build manifests and lock \
files, the CI configuration, the test and lint setup, and the directory \
layout. Read any instructions already written for an agent: `AGENTS.md`, \
`CLAUDE.md`, `.cursorrules`, `.atelier/instructions.md`. Open a few source \
files to see the conventions actually in use.

Then write `AGENTS.md` at the repository root. Include only what an agent \
could not cheaply rediscover:

- How to build, test and lint: the exact commands, including how to run a \
single test. Check that each one exists (a script, a make target, a manifest \
entry) rather than assuming it.
- Conventions that differ from the ecosystem's defaults — naming, layout, \
error handling, commit messages, anything a newcomer would get wrong by \
following habit.
- The architecture in a few sentences, naming the files and directories that \
matter and what each is for.
- What is easy to get wrong here: generated files that must not be edited, \
steps that must happen in order, commands that are slow or destructive, \
traps the code or its history warns about.

Keep it under about 60 lines. It is sent with every request, so every line \
has to earn its place:

- No generic advice (\"write clean code\", \"add tests\", \"handle errors\").
- Nothing a capable model already knows about the language, the framework or \
the tools.
- Do not restate the README; where it already covers something, point to it.
- No section that would be empty, and no placeholders. If you could not find \
something out, leave it out.

If an `AGENTS.md` already exists, improve it rather than overwrite it: keep \
what is accurate, fix what is stale, cut what breaks the rules above, add \
what is missing — then say what you changed and why. If there is a \
`CLAUDE.md` and no `AGENTS.md`, write `AGENTS.md` with the content that \
applies to any agent, and leave `CLAUDE.md` as it is.

When you are done, show the path of the file and a one-paragraph summary of \
what it covers.";

/// The prompt for `/init`, with what the user typed after the command (which
/// may be nothing) as further guidance.
pub(super) fn prompt(extra: &str) -> String {
    let extra = extra.trim();
    if extra.is_empty() {
        return BRIEF.to_string();
    }
    format!("{BRIEF}\n\nFurther guidance from the user:\n{extra}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_brief_stands_alone_or_carries_the_users_guidance() {
        assert_eq!(prompt("  "), BRIEF);
        let with = prompt(" focus on the web/ app ");
        assert!(with.starts_with(BRIEF));
        assert!(with.ends_with("Further guidance from the user:\nfocus on the web/ app"));
    }

    #[test]
    fn the_brief_says_what_to_cover_and_what_to_leave_out() {
        for needle in [
            "a single test",
            "differ from the ecosystem's defaults",
            "under about 60 lines",
            "No generic advice",
            "Do not restate the README",
            "improve it rather than overwrite it",
            "leave `CLAUDE.md` as it is",
        ] {
            assert!(BRIEF.contains(needle), "{needle}");
        }
    }
}
