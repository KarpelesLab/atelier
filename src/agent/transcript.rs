//! The conversation so far, as items an interface can show.
//!
//! A client that attaches to a session already under way — `atelier serve
//! --continue` — has seen none of its events; `session/history`
//! ([`crate::serve`]) gives it the conversation from what the session
//! keeps. That is the message list the model is sent (see `prompt`), so the
//! items are read from it rather than from what was once displayed:
//!
//! - `summary`: what compaction made of the turns it removed (`compact`).
//!   Always first: the items after it are all that is left verbatim.
//! - `user`: a prompt, with how many images were attached (the count; the
//!   images themselves stay in the session). The text is the message as it
//!   was sent, which is more than was typed when the harness added to it —
//!   a hook's `<hook-context>` block, a `<peer-message>`, the notice of a
//!   background job that ended, a `[turn_end hook]` reason. Those stay
//!   where they are, inside their markers.
//! - `assistant`: the model's text.
//! - `tool`: a call and its result together, the result as
//!   [`events::tool_end`] would have reported it (same cap, same
//!   `truncated`). A call with no result yet has neither `ok` nor `output`.
//! - `tool_images`: the images the calls above returned (a count).
//!
//! # What is left out
//!
//! The system prompt — instruction files, the memory index, the tool list —
//! is not a message of the conversation; it is built for each request. The
//! project context gathered for a turn (git status, diagnostics, the mode
//! note) rides with the user's message in a field of its own
//! ([`Message::context`]) and is not shown either. Reasoning is streamed
//! and not kept, so there is none to return.
//!
//! Whether a call succeeded is not stored, only its result; `ok` is read
//! back from it — every refusal and failure the loop writes starts with
//! `error:` (see `permissions::DENIED`, `ToolRunner::exec_only`).
//!
//! With the Claude backend the conversation is Claude Code's to keep, in
//! its own store: atelier has no messages, and the transcript is empty.

use serde_json::{Value, json};

use super::{Session, compact};
use crate::events;
use crate::mcp;
use crate::provider::Message;

/// What ends the note left where pruning dropped a turn's tool images
/// (`compact::prune_tool_output`), after their number.
const PRUNED_IMAGES: &str = " image(s) returned by the tool calls above, pruned]";

impl Session {
    /// The conversation as a list of items, oldest first (see the module
    /// docs). Empty with the Claude backend.
    pub fn transcript(&self) -> Vec<Value> {
        let mut items = Vec::new();
        if let Some(summary) = &self.summary {
            items.push(json!({ "type": "summary", "text": summary }));
        }
        for (at, message) in self.history.iter().enumerate() {
            match message.role.as_str() {
                "user" => user_items(message, &mut items),
                "assistant" => {
                    if !message.content.trim().is_empty() {
                        items.push(json!({ "type": "assistant", "text": message.content }));
                    }
                    // A call's result is among the tool messages that follow
                    // this one. Looked up there, not across the history: some
                    // servers number their calls from zero every time.
                    let results = self.history[at + 1..]
                        .iter()
                        .take_while(|m| m.role == "tool");
                    for call in &message.tool_calls {
                        let result = results
                            .clone()
                            .find(|m| m.tool_call_id.as_deref() == Some(&call.id));
                        items.push(self.tool_item(call, result));
                    }
                }
                // Tool results went out with their calls; a system message
                // is never stored.
                _ => {}
            }
        }
        items
    }

    /// The id of the Claude Code session this one continues (Claude
    /// backend), once there is one: where its conversation is kept.
    pub fn claude_session(&self) -> Option<&str> {
        self.claude_session.as_deref()
    }

    /// A tool call with its result, if it has one.
    fn tool_item(&self, call: &crate::provider::ToolCall, result: Option<&Message>) -> Value {
        // As the events named it: an `mcp_call` of a deferred tool is a
        // call of that tool.
        let resolved = mcp::deferred::resolve(&self.tools, &call.name, &call.arguments);
        let (name, arguments) = match &resolved {
            Some((name, arguments)) => (name.as_str(), arguments.as_str()),
            None => (call.name.as_str(), call.arguments.as_str()),
        };
        let mut item = match result {
            Some(result) => {
                let ok = !result.content.starts_with("error:");
                let mut item = events::tool_end(name, &result.content, ok);
                if compact::is_stub(&result.content) {
                    item["pruned"] = Value::Bool(true);
                }
                item
            }
            None => json!({ "name": name }),
        };
        item["type"] = Value::from("tool");
        item["id"] = Value::from(call.id.as_str());
        item["arguments"] = events::arguments(arguments);
        item
    }
}

/// A user message: the prompt — or the note that carries the images a
/// turn's tool calls returned, and with it whatever the user queued
/// meanwhile (see `Session::push_user_text`).
fn user_items(message: &Message, items: &mut Vec<Value>) {
    let content = message.content.as_str();
    let note = if let Some(rest) = content.strip_prefix(compact::TOOL_IMAGES_NOTE) {
        Some((message.images.len(), false, rest))
    } else {
        // `[3 image(s) returned by the tool calls above, pruned]`
        content
            .strip_prefix('[')
            .and_then(|s| s.split_once(PRUNED_IMAGES))
            .and_then(|(n, rest)| Some((n.parse().ok()?, true, rest)))
    };
    let Some((images, pruned, rest)) = note else {
        items.push(json!({
            "type": "user",
            "text": content,
            "images": message.images.len(),
        }));
        return;
    };
    let mut item = json!({ "type": "tool_images", "images": images });
    if pruned {
        item["pruned"] = Value::Bool(true);
    }
    items.push(item);
    if !rest.trim().is_empty() {
        items.push(json!({ "type": "user", "text": rest.trim(), "images": 0 }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolCall;

    fn session() -> Session {
        let cfg = crate::config::Config::single(
            crate::provider::Provider::from_env("http://127.0.0.1:9/v1".into(), None),
            "m".into(),
        );
        let root = std::env::temp_dir().join("atl-transcript-unused");
        let trust = crate::trust::testing::open(&root);
        Session::new(
            cfg,
            root,
            crate::tools::builtin_registry(),
            Vec::new(),
            crate::settings::Settings::default(),
            trust,
        )
    }

    fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    #[test]
    fn calls_are_paired_with_their_results_and_context_is_left_out() {
        let mut s = session();
        s.summary = Some("earlier: set up the project".into());
        s.history = vec![
            Message::user_with_images("look at this", vec!["data:image/png;base64,AAAA".into()])
                .with_context(Some("## git status\nclean".into())),
            Message::assistant_tool_calls(
                "reading",
                vec![
                    call("c1", "read", r#"{"path":"a.txt"}"#),
                    call("c2", "bash", r#"{"command":"make"}"#),
                ],
            ),
            Message::tool_result("c1", "ALPHA"),
            Message::tool_result("c2", crate::permissions::DENIED),
            Message::user_with_images(
                format!("{}\n\nand hurry", compact::TOOL_IMAGES_NOTE),
                vec!["data:a".into(), "data:b".into()],
            ),
            // The same ids again, as some servers do: each call finds the
            // result that follows it.
            Message::assistant_tool_calls("", vec![call("c1", "read", "{oops")]),
            Message::tool_result("c1", "x".repeat(20_000)),
            Message::assistant("done"),
            Message::user("next"),
            Message::assistant_tool_calls("", vec![call("c9", "ls", "{}")]),
        ];
        let items = s.transcript();
        let expected = json!([
            { "type": "summary", "text": "earlier: set up the project" },
            { "type": "user", "text": "look at this", "images": 1 },
            { "type": "assistant", "text": "reading" },
            { "type": "tool", "id": "c1", "name": "read", "arguments": { "path": "a.txt" },
              "ok": true, "output": "ALPHA" },
            { "type": "tool", "id": "c2", "name": "bash", "arguments": { "command": "make" },
              "ok": false, "output": crate::permissions::DENIED },
            { "type": "tool_images", "images": 2 },
            { "type": "user", "text": "and hurry", "images": 0 },
        ]);
        assert_eq!(Value::from(items[..7].to_vec()), expected);
        // Arguments that are not JSON come back as written; a long result
        // is capped as its event was.
        assert_eq!(items[7]["arguments"], "{oops");
        assert_eq!(items[7]["truncated"], true);
        assert_eq!(items[7]["output"].as_str().unwrap().len(), 16 * 1024);
        assert_eq!(items[8], json!({ "type": "assistant", "text": "done" }));
        assert_eq!(items[9]["text"], "next");
        // A call the turn has not answered yet.
        assert_eq!(
            items[10],
            json!({ "type": "tool", "id": "c9", "name": "ls", "arguments": {} })
        );
        assert_eq!(items.len(), 11);
        // Nothing of the turn's context anywhere.
        assert!(!Value::from(items).to_string().contains("git status"));
    }

    #[test]
    fn pruned_results_and_images_say_so() {
        let mut s = session();
        s.history = vec![
            Message::user("go"),
            Message::assistant_tool_calls("", vec![call("c1", "read", r#"{"path":"big"}"#)]),
            Message::tool_result("c1", "line\n".repeat(400)),
            Message::user_with_images(compact::TOOL_IMAGES_NOTE, vec!["data:a".into()]),
            Message::assistant("it is"),
        ];
        assert_eq!(compact::prune_tool_output(&mut s.history, 0), 2);
        let items = s.transcript();
        assert_eq!(items[1]["pruned"], true);
        assert!(
            items[1]["output"]
                .as_str()
                .unwrap()
                .starts_with("[output of ")
        );
        assert_eq!(
            items[2],
            json!({ "type": "tool_images", "images": 1, "pruned": true })
        );
    }
}
