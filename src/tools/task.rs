//! The `task` tool: hand a self-contained piece of work to a subagent and
//! get its final report (see [`crate::agents`] for what an agent is).
//!
//! This file is only what the model sees — the name, the description that
//! lists the agents, the parameters — and the reading of a call's arguments.
//! A subagent is a whole conversation on atelier's own loop, with its own
//! tools and the parent's approval path; none of that is reachable from
//! [`Tool::call`], which gets a [`ToolCtx`] and nothing of the session. So
//! the agent loop runs `task` calls itself, by name (`agent::subagent`), and
//! [`Tool::call`] here is never the way one runs.
//!
//! The tool is registered only for atelier's own loop: the Claude backend
//! runs claude's loop, which has subagents of its own.

use std::path::PathBuf;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec};
use crate::agents::{self, Agent, Found, Isolation, TASK_TOOL};

/// Longest description shown for a subagent when the call gave none and it
/// is cut from the prompt.
const MAX_DESCRIPTION_CHARS: usize = 60;

pub struct TaskTool {
    root: PathBuf,
    /// The user-global agents directory, fixed when the tool is built.
    global: Option<PathBuf>,
}

impl TaskTool {
    /// The tool for the project at `root`: its description lists the agents
    /// found there and in `global`.
    pub fn new(root: PathBuf, global: Option<PathBuf>) -> Self {
        Self { root, global }
    }
}

/// A `task` call, read and checked.
pub struct TaskCall {
    pub agent: Agent,
    /// Everything the subagent is told.
    pub prompt: String,
    /// A few words for the transcript.
    pub description: String,
    /// Start it and go on, instead of waiting for its report.
    pub background: bool,
    /// Where it works: what the call asks for, else what the agent's
    /// definition says.
    pub isolation: Isolation,
}

impl TaskCall {
    /// Read a call's `arguments` (JSON text, as the model sent it) against
    /// the agents in `found`. `Err` is the tool result for a bad call.
    pub fn parse(arguments: &str, found: &Found) -> Result<Self, String> {
        let args: Value = serde_json::from_str(arguments)
            .map_err(|e| format!("error: invalid arguments JSON: {e}"))?;
        let text = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        };
        let available = || {
            let names: Vec<&str> = found.agents.iter().map(|a| a.name.as_str()).collect();
            names.join(", ")
        };
        let name = text("agent").ok_or_else(|| {
            format!(
                "error: missing required argument 'agent' (one of: {})",
                available()
            )
        })?;
        let agent = found
            .get(name)
            .ok_or_else(|| format!("error: no agent named {name:?}; available: {}", available()))?;
        let prompt = text("prompt").ok_or("error: missing required argument 'prompt'")?;
        let description = match text("description") {
            Some(d) => d.to_string(),
            None => {
                let first = prompt.lines().next().unwrap_or("");
                if first.chars().count() > MAX_DESCRIPTION_CHARS {
                    let cut: String = first.chars().take(MAX_DESCRIPTION_CHARS).collect();
                    format!("{cut}…")
                } else {
                    first.to_string()
                }
            }
        };
        // `run_in_background` is what Claude models know it as.
        let background = ["background", "run_in_background"]
            .iter()
            .any(|k| args.get(*k).and_then(Value::as_bool) == Some(true));
        // A call may ask for a worktree; it cannot take one away from an
        // agent defined to work in one.
        let isolation = match text("isolation").map(Isolation::parse) {
            None | Some(Ok(Isolation::Shared)) => agent.isolation,
            Some(Ok(Isolation::Worktree)) => Isolation::Worktree,
            Some(Err(e)) => return Err(format!("error: {e}")),
        };
        Ok(Self {
            agent: agent.clone(),
            prompt: prompt.to_string(),
            description,
            background,
            isolation,
        })
    }
}

impl Tool for TaskTool {
    fn name(&self) -> &str {
        TASK_TOOL
    }

    fn requires_approval(&self, _args: &Value) -> bool {
        // Starting an agent does nothing by itself; each call the agent then
        // makes goes through approval like the caller's own.
        false
    }

    // Starting a subagent changes nothing by itself: each of its calls is
    // judged on its own, by the parent's rules and mode.
    fn mutates(&self, _args: &Value) -> bool {
        false
    }

    fn spec(&self) -> ToolSpec {
        // Built per request, so an agent file added mid-session is offered
        // at the next step.
        let found = agents::discover(&self.root, self.global.as_deref());
        let mut description = "Run a subagent on a task and get its final report. The \
            subagent works in a fresh conversation with its own tools: it sees NOTHING of \
            this conversation, only the `prompt` you write. So write a complete, \
            self-contained prompt — the goal, the relevant file paths and facts you \
            already know, what is out of scope, and exactly what to report back. Its \
            final message is returned to you; the user does not see it, so relay what \
            matters. Use it to keep long searches and side investigations out of your own \
            context, and to do independent pieces of work at once: several `task` calls \
            in one step run in parallel. Do not use it for something one or two tool \
            calls would settle. Subagents cannot start subagents.\n\nWith `background: true` \
            the call returns at once with the agent's id and the agent works on while you \
            do: you are told when it ends (a <subagent> block carrying its report), so do \
            not wait for it or poll. A background agent has no user to ask: a call that \
            would need the user's approval is refused to it, so use it for work the \
            permission rules already allow — reading and searching above all.\n\nWith \
            `isolation: \"worktree\"` the agent works in a fresh git worktree of the \
            repository, checked out at HEAD, instead of the project directory — so its edits \
            cannot collide with yours. Your UNCOMMITTED changes are NOT in that worktree: \
            commit first what it must see. If it changed anything, its report ends with \
            where the worktree is, a diff summary and the command to bring the changes \
            over; nothing is merged for you. It needs a git repository with at least one \
            commit, and fails without one. Use it for agents that write, above all in the \
            background.\n\nAgents:"
            .to_string();
        for a in &found.agents {
            description.push_str(&format!("\n- {}: {}", a.name, a.description));
        }
        let names: Vec<&str> = found.agents.iter().map(|a| a.name.as_str()).collect();
        ToolSpec {
            name: self.name().to_string(),
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "enum": names,
                        "description": "Which agent runs the task."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The complete task. The agent sees nothing else of this conversation."
                    },
                    "description": {
                        "type": "string",
                        "description": "The task in 3-6 words, shown to the user while it runs."
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Start the agent and return its id immediately; its report arrives later as a notification (default false)."
                    },
                    "isolation": {
                        "type": "string",
                        "enum": ["worktree"],
                        "description": "\"worktree\": run the agent in its own git worktree at HEAD (uncommitted changes are not carried over)."
                    }
                },
                "required": ["agent", "prompt"]
            }),
        }
    }

    fn call(&self, _ctx: &mut ToolCtx, _args: Value) -> Result<String> {
        // See the module docs: the loop runs these, and only it can.
        bail!("`task` can only be run by atelier's agent loop")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found() -> Found {
        // A project directory that does not exist: the built-ins only.
        agents::discover(std::path::Path::new("/nonexistent-atelier-project"), None)
    }

    #[test]
    fn the_description_lists_the_agents() {
        let tool = TaskTool::new("/nonexistent-atelier-project".into(), None);
        let spec = tool.spec();
        assert_eq!(spec.name, "task");
        assert!(spec.description.contains("\n- explore: Read-only search"));
        assert!(spec.description.contains("\n- general: "));
        assert!(spec.description.contains("self-contained"));
        assert_eq!(
            spec.parameters["properties"]["agent"]["enum"],
            json!(["explore", "general"])
        );
    }

    #[test]
    fn calls_are_checked() {
        let found = found();
        let call = TaskCall::parse(
            r#"{"agent":"explore","prompt":"Find the parser.\nReport lines.","description":"find parser"}"#,
            &found,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(call.agent.name, "explore");
        assert_eq!(call.description, "find parser");
        assert_eq!(call.prompt, "Find the parser.\nReport lines.");

        // Without a description, the prompt's first line stands in.
        let call = TaskCall::parse(r#"{"agent":"general","prompt":"Fix it.\nMore."}"#, &found);
        let call = call.ok().unwrap();
        assert_eq!(call.description, "Fix it.");
        assert!(!call.background);
        assert_eq!(call.isolation, Isolation::Shared);
        let isolated = |value: &str| {
            let args = format!(r#"{{"agent":"explore","prompt":"x","isolation":"{value}"}}"#);
            TaskCall::parse(&args, &found).map(|call| call.isolation)
        };
        assert_eq!(isolated("worktree").ok(), Some(Isolation::Worktree));
        assert_eq!(isolated("none").ok(), Some(Isolation::Shared));
        assert!(
            isolated("vm")
                .err()
                .unwrap()
                .starts_with("error: `isolation` must be")
        );
        for key in ["background", "run_in_background"] {
            let args = format!(r#"{{"agent":"explore","prompt":"x","{key}":true}}"#);
            assert!(TaskCall::parse(&args, &found).ok().unwrap().background);
        }

        let err = |args: &str| TaskCall::parse(args, &found).err().unwrap();
        assert!(err(r#"{"agent":"nope","prompt":"x"}"#).contains("available: explore, general"));
        assert!(err(r#"{"prompt":"x"}"#).contains("'agent'"));
        assert!(err(r#"{"agent":"explore","prompt":" "}"#).contains("'prompt'"));
        assert!(err("not json").contains("invalid arguments"));
    }
}
