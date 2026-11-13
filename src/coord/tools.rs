//! The coordinator, as the agent sees it: `peers` (who else is running,
//! and which topics to listen to) and `peer_send` (talk to them).

use std::sync::Arc;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::Client;
use crate::provider::ToolSpec;
use crate::tools::{Tool, ToolCtx, ToolRegistry};

/// Add the coordinator tools to `registry`.
pub fn register(registry: &mut ToolRegistry, client: &Arc<Client>) {
    registry.register(Box::new(Peers(Arc::clone(client))));
    registry.register(Box::new(PeerSend(Arc::clone(client))));
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
