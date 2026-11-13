//! The coordinator, as the agent sees it: `peers` (who else is running,
//! and which topics to listen to), `peer_send` (talk to them), and
//! `peer_ask` / `peer_answer` (put a question to another agent and wait for
//! its answer; answer one that was put to this agent — see [`super::ask`]).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::Client;
use super::ask::{self, Outcome};
use crate::provider::ToolSpec;
use crate::tools::{Tool, ToolCtx, ToolRegistry};

/// Add the coordinator tools to `registry`.
pub fn register(registry: &mut ToolRegistry, client: &Arc<Client>) {
    registry.register(Box::new(Peers(Arc::clone(client))));
    registry.register(Box::new(PeerSend(Arc::clone(client))));
    registry.register(Box::new(PeerAsk(Arc::clone(client))));
    registry.register(Box::new(PeerAnswer(Arc::clone(client))));
}

/// A `list` answer as lines of text: one per instance, this one marked.
pub fn render_instances(list: &Value) -> Vec<String> {
    let text = |v: &Value| v.as_str().unwrap_or("?").to_string();
    let instances = list["instances"].as_array().cloned().unwrap_or_default();
    if instances.is_empty() {
        return vec!["(no instances connected)".to_string()];
    }
    instances
        .iter()
        .map(|i| {
            let mut line = format!(
                "{}{} — {}, {}, in {}",
                text(&i["name"]),
                if i["you"] == true { " (you)" } else { "" },
                text(&i["status"]),
                text(&i["model"]),
                text(&i["cwd"]),
            );
            let topics: Vec<String> = i["topics"]
                .as_array()
                .map(|a| a.iter().map(|t| format!("#{}", text(t))).collect())
                .unwrap_or_default();
            if !topics.is_empty() {
                line.push_str(&format!(", listening to {}", topics.join(" ")));
            }
            line
        })
        .collect()
}

struct Peers(Arc<Client>);

impl Tool for Peers {
    fn name(&self) -> &str {
        "peers"
    }

    // Lists instances and sets which topics this one hears; nothing of the
    // user's changes.
    fn mutates(&self, _args: &Value) -> bool {
        false
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "peers".into(),
            description: "List the other atelier instances (AI coding agents like you) running \
                          on this machine: their name, whether they are idle or working, their \
                          model and the directory they work in. Optionally subscribe to or \
                          unsubscribe from topics; messages published to a topic you subscribe \
                          to are delivered to you. Use peer_send to talk to an instance."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "subscribe": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Topics to start listening to."
                    },
                    "unsubscribe": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Topics to stop listening to."
                    }
                }
            }),
        }
    }

    // Reads the list of the user's own instances; nothing leaves the machine.
    fn requires_approval(&self, _args: &Value) -> bool {
        false
    }

    fn call(&self, _ctx: &mut ToolCtx, args: Value) -> Result<String> {
        for (key, on) in [("subscribe", true), ("unsubscribe", false)] {
            for topic in args[key].as_array().into_iter().flatten() {
                if let Some(t) = topic.as_str() {
                    self.0.subscribe(t, on)?;
                }
            }
        }
        Ok(render_instances(&self.0.list()?).join("\n"))
    }
}

struct PeerSend(Arc<Client>);

impl Tool for PeerSend {
    fn name(&self) -> &str {
        "peer_send"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "peer_send".into(),
            description: "Send a message to other atelier instances on this machine. 'to' is an \
                          instance name (see peers), '*' for every other instance, or '#topic' \
                          for the instances subscribed to a topic. The message is shown to the \
                          recipient's user and given to its agent at its next step; an idle \
                          instance only reads it when its user next prompts it, so do not wait \
                          for a reply. Replies arrive as messages from the peer."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "Instance name, '*' (everyone), or '#topic'."
                    },
                    "message": { "type": "string" }
                },
                "required": ["to", "message"]
            }),
        }
    }

    // Requires approval (the trait's default): it acts outside the project,
    // on other agents.

    fn call(&self, _ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let (Some(to), Some(message)) = (args["to"].as_str(), args["message"].as_str()) else {
            bail!("'to' and 'message' are required");
        };
        let n = self.0.send(to, message)?;
        Ok(match n {
            0 if to.starts_with('#') => format!("no instance is subscribed to {to}"),
            0 => "no other instance is connected".to_string(),
            n => format!("delivered to {n} instance(s)"),
        })
    }
}

struct PeerAsk(Arc<Client>);

impl Tool for PeerAsk {
    fn name(&self) -> &str {
        "peer_ask"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "peer_ask".into(),
            description: format!(
                "Ask another atelier instance a question and wait for its agent's answer, which \
                 is this tool's result. 'to' is an instance name (see peers). Use it when you \
                 need what the other agent knows before you can continue; use peer_send for \
                 anything that needs no answer. The call blocks until the answer arrives or \
                 the timeout runs out (default {}s, at most {}s). A working instance answers at \
                 its next step; an idle one only once its user prompts it, so a question to \
                 an idle instance may come back unanswered — the result then says so and why.",
                ask::DEFAULT_TIMEOUT.as_secs(),
                ask::MAX_TIMEOUT.as_secs()
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "Instance name." },
                    "question": { "type": "string" },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "How long to wait for the answer."
                    }
                },
                "required": ["to", "question"]
            }),
        }
    }

    // Requires approval and counts as mutating (the trait's defaults): it
    // sets another agent to work, which may act on the question.

    fn call(&self, _ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let (Some(to), Some(question)) = (args["to"].as_str(), args["question"].as_str()) else {
            bail!("'to' and 'question' are required");
        };
        let timeout = args["timeout_secs"]
            .as_u64()
            .map_or(ask::DEFAULT_TIMEOUT, |s| Duration::from_secs(s.max(1)));
        // No answer is a result the model can act on, not a failed call.
        Ok(match self.0.ask(to, question, timeout)? {
            Outcome::Answered(answer) => answer,
            Outcome::Unanswered(why) => format!("no answer: {why}"),
        })
    }
}

struct PeerAnswer(Arc<Client>);

impl Tool for PeerAnswer {
    fn name(&self) -> &str {
        "peer_answer"
    }

    // Says something to an agent that asked for it; nothing of the user's
    // changes.
    fn mutates(&self, _args: &Value) -> bool {
        false
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "peer_answer".into(),
            description: "Answer a question another atelier instance asked you: one that \
                          reached you as <peer-question id=\"…\" from=\"…\">. The other agent is \
                          blocked waiting for it, so answer in the turn the question arrives \
                          in — a question still open when your turn ends is closed as \
                          unanswered. 'id' is the question's id."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The question's id." },
                    "answer": { "type": "string" }
                },
                "required": ["id", "answer"]
            }),
        }
    }

    // The question was admitted from one of the user's own instances, which
    // is waiting for exactly this; answering it reaches nobody new.
    fn requires_approval(&self, _args: &Value) -> bool {
        false
    }

    fn call(&self, _ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let (Some(id), Some(answer)) = (args["id"].as_str(), args["answer"].as_str()) else {
            bail!("'id' and 'answer' are required");
        };
        let from = self.0.answer_question(id, answer)?;
        Ok(format!("answer sent to {from}"))
    }
}
