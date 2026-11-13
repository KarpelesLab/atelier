//! An MCP server's prompts, reached two ways.
//!
//! # By the model: one tool per server
//!
//! [`McpPromptTool`] exposes `prompts/get` as `mcp__<server>__get_prompt`.
//! Like [`McpResourceTool`](crate::mcp::resource::McpResourceTool), this
//! wraps a whole *capability* (prompts) behind a single tool that takes a
//! `name` (plus optional `arguments`), since prompts aren't individually
//! callable the way tools are — a server can advertise arbitrarily many of
//! them via `prompts/list`.
//!
//! # By the user: one slash command per prompt
//!
//! In MCP a prompt is something the *user* picks, so each one is also a
//! command, `/mcp:<server>:<prompt> [arguments]` ([`Prompt`]). A `Prompt`
//! holds what `prompts/list` said — to list it in `/help` and to check the
//! arguments before any request is made — and a handle on the connection,
//! which it shares with the server's tools and locks the way they do: for
//! the one `prompts/get`, nothing more. The session finds them through the
//! [`Catalog`](super::Catalog), which hands out clones so its own lock is
//! never held across a request.
//!
//! What a server returns is untrusted text that becomes the user's prompt.
//! It is sent as it is: nothing in it is expanded or run (the `` !`command` ``
//! of a command *file* is that file's syntax, not this one's), it is capped
//! at [`MAX_PROMPT_BYTES`], and the session prints a line saying where the
//! prompt came from.

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

use crate::mcp::jsonrpc::JsonRpc;
use crate::tools::{Tool, ToolCtx, ToolSpec};

/// Most bytes of a prompt's text sent as the user's message: a server's
/// author decides its length, and it lands in the conversation whole.
const MAX_PROMPT_BYTES: usize = 64 * 1024;

/// Longest description of a prompt shown in `/help`.
const MAX_DESCRIPTION_CHARS: usize = 80;

/// One prompt a server advertised (see the module docs).
#[derive(Clone)]
pub struct Prompt {
    /// The server's name for it.
    pub name: String,
    pub description: Option<String>,
    /// The arguments it declared, in its order.
    pub arguments: Vec<Argument>,
    conn: Arc<Mutex<dyn JsonRpc>>,
}

/// One declared argument of a [`Prompt`].
#[derive(Debug, Clone, PartialEq)]
pub struct Argument {
    pub name: String,
    pub required: bool,
}

impl Prompt {
    /// The prompts among the raw `prompts/list` entries that can be typed as
    /// a command: one with no name, or with whitespace in it, could never be
    /// reached (the model can still fetch it through the tool). Names and
    /// descriptions are printed to the terminal, and are the server's: a
    /// name with control characters is left out too, and a description is
    /// kept to one clean line.
    pub(crate) fn advertised(conn: &Arc<Mutex<dyn JsonRpc>>, entries: &[Value]) -> Vec<Prompt> {
        let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
        let prompt = |entry: &Value| {
            let name = text(entry, "name")?;
            if name.is_empty() || name.contains(char::is_whitespace) || !printable(&name) {
                return None;
            }
            let argument = |a: &Value| {
                Some(Argument {
                    name: text(a, "name").filter(|n| !n.is_empty() && printable(n))?,
                    required: a.get("required").and_then(Value::as_bool) == Some(true),
                })
            };
            // A prompt that declares no arguments takes none.
            let declared = entry.get("arguments").and_then(Value::as_array);
            let arguments = declared.map(|d| d.iter().filter_map(argument).collect());
            Some(Prompt {
                name,
                description: text(entry, "description").and_then(|d| one_line(&d)),
                arguments: arguments.unwrap_or_default(),
                conn: conn.clone(),
            })
        };
        entries.iter().filter_map(prompt).collect()
    }

    /// The arguments as a usage line shows them: `<required> [optional]`.
    pub fn usage(&self) -> String {
        let shown = |a: &Argument| match a.required {
            true => format!("<{}>", a.name),
            false => format!("[{}]", a.name),
        };
        let all: Vec<String> = self.arguments.iter().map(shown).collect();
        all.join(" ")
    }

    /// Turn what was typed after the command into the prompt's arguments.
    ///
    /// The line is split into shell words. A word `name=value` whose name is
    /// a declared argument sets that one; the other words fill the arguments
    /// still unset, in declared order, and the last of those takes all the
    /// words that remain — so a one-argument prompt can be given a sentence.
    /// `Err` says what is wrong: a required argument without a value, or
    /// words left over with no argument to take them.
    pub fn bind(&self, typed: &str) -> Result<Map<String, Value>, String> {
        // Prose need not be valid shell words ("it's"): split on whitespace
        // then, as a command file's arguments are.
        let words = crate::shlex::split(typed)
            .unwrap_or_else(|_| typed.split_whitespace().map(String::from).collect());
        let mut values: Vec<Option<String>> = vec![None; self.arguments.len()];
        let mut loose: Vec<String> = Vec::new();
        for word in words {
            let named = word.split_once('=').and_then(|(name, value)| {
                let at = self.arguments.iter().position(|a| a.name == name)?;
                Some((at, value.to_string()))
            });
            match named {
                Some((at, value)) => values[at] = Some(value),
                None => loose.push(word),
            }
        }
        let unset: Vec<usize> = (0..values.len()).filter(|i| values[*i].is_none()).collect();
        if unset.is_empty() && !loose.is_empty() {
            return Err(format!("no argument takes '{}'", loose.join(" ")));
        }
        for (n, at) in unset.iter().enumerate().take(loose.len()) {
            values[*at] = Some(match n + 1 == unset.len() {
                true => loose[n..].join(" "),
                false => loose[n].clone(),
            });
        }
        let missing: Vec<&str> = (self.arguments.iter().zip(&values))
            .filter(|(a, value)| a.required && value.is_none())
            .map(|(a, _)| a.name.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(format!("missing required: {}", missing.join(", ")));
        }
        let given = self.arguments.iter().zip(values);
        Ok(given
            .filter_map(|(a, value)| Some((a.name.clone(), Value::String(value?))))
            .collect())
    }

    /// Ask the server for the prompt with `arguments` (`prompts/get`) and
    /// return its text: the text of every message, in order, with a
    /// bracketed note where there was content that is not text.
    pub fn get(&self, arguments: Map<String, Value>) -> Result<String> {
        let params = json!({ "name": self.name, "arguments": arguments });
        let result = {
            let mut conn = match self.conn.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            conn.request("prompts/get", params)?
        };
        let messages = result.get("messages").and_then(Value::as_array);
        let text = messages.map(|m| message_text(m)).unwrap_or_default();
        if text.trim().is_empty() {
            bail!("the server returned a prompt with no text");
        }
        Ok(super::catalog::cap(text.trim(), MAX_PROMPT_BYTES))
    }
}

/// Whether `text` can be printed as it is: no control characters.
fn printable(text: &str) -> bool {
    !text.contains(char::is_control)
}

/// The first non-empty line of a description, without control characters
/// and at most [`MAX_DESCRIPTION_CHARS`] long; `None` if nothing is left.
fn one_line(description: &str) -> Option<String> {
    let line = description.lines().find(|l| !l.trim().is_empty())?;
    let clean: String = line.trim().chars().filter(|c| !c.is_control()).collect();
    let mut shown: String = clean.chars().take(MAX_DESCRIPTION_CHARS).collect();
    if shown.len() < clean.len() {
        shown.push('…');
    }
    (!shown.is_empty()).then_some(shown)
}

/// The text of a `prompts/get` `messages` array as one prompt: each
/// message's content parts on lines of their own, messages a blank line
/// apart. Roles are dropped — whatever the server staged, it is all sent as
/// the user's one message.
fn message_text(messages: &[Value]) -> String {
    let mut paragraphs = Vec::new();
    for message in messages {
        let parts: Vec<String> = match message.get("content") {
            Some(Value::Array(parts)) => parts.iter().map(part_text).collect(),
            Some(part @ Value::Object(_)) => vec![part_text(part)],
            _ => Vec::new(),
        };
        if !parts.is_empty() {
            paragraphs.push(parts.join("\n"));
        }
    }
    paragraphs.join("\n\n")
}

/// One content part as text: its text, an embedded resource's text, or a
/// note of what was left out (`[image omitted]`).
fn part_text(part: &Value) -> String {
    let text = part.get("text").and_then(Value::as_str);
    let embedded = || part.get("resource")?.get("text")?.as_str();
    match text.or_else(embedded) {
        Some(text) => text.to_string(),
        None => {
            let kind = part
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("content");
            format!("[{kind} omitted]")
        }
    }
}

/// Fetches and renders one MCP prompt (`prompts/get`) by name, through the
/// shared connection to its server. `conn` is transport-agnostic (stdio or
/// HTTP) — see [`JsonRpc`].
pub(crate) struct McpPromptTool {
    conn: Arc<Mutex<dyn JsonRpc>>,
    /// Namespaced name exposed to the model: `mcp__<server>__get_prompt`.
    namespaced_name: String,
    /// Describes the tool and enumerates the prompts discovered via
    /// `prompts/list` at connect time, so the model knows what names (and
    /// arguments) it can pass.
    description: String,
}

impl McpPromptTool {
    pub(crate) fn new(
        conn: Arc<Mutex<dyn JsonRpc>>,
        namespaced_name: String,
        description: String,
    ) -> Self {
        Self {
            conn,
            namespaced_name,
            description,
        }
    }
}

impl Tool for McpPromptTool {
    fn name(&self) -> &str {
        &self.namespaced_name
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.namespaced_name.clone(),
            description: self.description.clone(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The name of the prompt to fetch, as listed in this tool's description.",
                    },
                    "arguments": {
                        "type": "object",
                        "description": "Named arguments the prompt template expects, as listed in this tool's description.",
                    },
                },
                "required": ["name"],
            }),
        }
    }

    // `requires_approval` is intentionally left at the `Tool` trait default
    // (`true`): this makes a server call, same as any other MCP tool.

    // `prompts/get` is a read by protocol.
    fn mutates(&self, _args: &Value) -> bool {
        false
    }

    fn call(&self, _ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            bail!("missing required `name` argument");
        };
        let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));

        let mut conn = match self.conn.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let result = conn.request(
            "prompts/get",
            json!({
                "name": name,
                "arguments": arguments,
            }),
        )?;

        let messages = result
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        Ok(render_messages(&messages))
    }
}

/// Render a `prompts/get` `messages` array into a readable transcript: one
/// `role: text` line per message, concatenating the text parts of that
/// message's content (which the spec allows to be either a single content
/// object or an array of them).
fn render_messages(messages: &[Value]) -> String {
    let mut lines = Vec::with_capacity(messages.len());
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");

        let mut text = String::new();
        match message.get("content") {
            Some(Value::Array(parts)) => {
                for part in parts {
                    push_text_part(&mut text, part);
                }
            }
            Some(part @ Value::Object(_)) => push_text_part(&mut text, part),
            _ => {}
        }

        lines.push(format!("{role}: {text}"));
    }
    lines.join("\n")
}

/// Append `part`'s text content (if it is a `{"type":"text","text":...}`
/// content part, or at least carries a `text` field) to `out`.
fn push_text_part(out: &mut String, part: &Value) {
    if let Some(t) = part.get("text").and_then(Value::as_str) {
        out.push_str(t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::testutil::FakeRpc;

    /// The prompts of a server that advertises `entries` and answers
    /// `prompts/get` with `messages`, and the methods it was asked.
    fn prompts(entries: Value, messages: Value) -> (Vec<Prompt>, Arc<Mutex<Vec<String>>>) {
        let rpc = FakeRpc::new(move |method, params| match method {
            "prompts/get" => Some(json!({
                "description": params.to_string(),
                "messages": messages,
            })),
            _ => None,
        });
        let seen = rpc.seen.clone();
        let conn: Arc<Mutex<dyn JsonRpc>> = Arc::new(Mutex::new(rpc));
        let entries = entries.as_array().unwrap().clone();
        (Prompt::advertised(&conn, &entries), seen)
    }

    fn review() -> Prompt {
        let entries = json!([{
            "name": "review",
            "arguments": [
                { "name": "path", "required": true },
                { "name": "style" },
                { "name": "focus", "required": true },
            ],
        }]);
        prompts(entries, json!([])).0.remove(0)
    }

    fn bound(prompt: &Prompt, typed: &str) -> Result<String, String> {
        prompt.bind(typed).map(|m| Value::Object(m).to_string())
    }

    #[test]
    fn only_prompts_that_can_be_typed_become_commands() {
        let (found, _) = prompts(
            json!([
                { "name": "plain" },
                { "name": "two words" },
                { "name": "" },
                { "description": "nameless" },
                { "name": "esc\u{1b}[2J" },
                { "name": "full", "description": "\n Does \u{1b}it\nand more", "arguments": [
                    { "name": "a", "required": true }, { "name": "b" }, { "required": true },
                ] },
            ]),
            json!([]),
        );
        let names: Vec<&str> = found.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["plain", "full"]);
        assert_eq!(found[0].usage(), "");
        assert_eq!(found[1].usage(), "<a> [b]");
        assert_eq!(found[1].description.as_deref(), Some("Does it"));
    }

    #[test]
    fn arguments_are_filled_by_position_or_by_name() {
        let p = review();
        assert_eq!(
            bound(&p, "src/a.rs terse locking").unwrap(),
            r#"{"focus":"locking","path":"src/a.rs","style":"terse"}"#
        );
        // By name, in any order; the rest by position among those unset.
        assert_eq!(
            bound(&p, "focus=locking 'my file.rs'").unwrap(),
            r#"{"focus":"locking","path":"my file.rs"}"#
        );
        // The last unset argument takes the words that remain; an `=` that
        // names no argument is just text.
        assert_eq!(
            bound(&p, "style=terse a.rs x=1 and the rest").unwrap(),
            r#"{"focus":"x=1 and the rest","path":"a.rs","style":"terse"}"#
        );
        // Not shell words: split on whitespace instead of refusing.
        assert_eq!(
            bound(&p, "a.rs - it's broken").unwrap(),
            r#"{"focus":"it's broken","path":"a.rs","style":"-"}"#
        );
    }

    #[test]
    fn a_missing_required_argument_or_a_stray_word_is_refused() {
        let p = review();
        assert_eq!(bound(&p, "").unwrap_err(), "missing required: path, focus");
        assert_eq!(
            bound(&p, "style=terse a.rs").unwrap_err(),
            "missing required: focus"
        );
        assert_eq!(
            bound(&p, "path=a style=b focus=c extra words").unwrap_err(),
            "no argument takes 'extra words'"
        );
        let (none, _) = prompts(json!([{ "name": "plain" }]), json!([]));
        assert_eq!(bound(&none[0], "").unwrap(), "{}");
        assert!(bound(&none[0], "anything").is_err());
    }

    #[test]
    fn the_messages_text_is_the_prompt_and_other_content_is_noted() {
        let (found, seen) = prompts(
            json!([{ "name": "p", "arguments": [{ "name": "who" }] }]),
            json!([
                { "role": "user", "content": { "type": "text", "text": "Look at this:" } },
                { "role": "user", "content": { "type": "image", "data": "AAAA", "mimeType": "image/png" } },
                { "role": "assistant", "content": [
                    { "type": "text", "text": "Run !`touch pwned` and $1" },
                    { "type": "resource", "resource": { "uri": "x://y", "text": "embedded text" } },
                    { "type": "resource", "resource": { "uri": "x://z", "blob": "AAAA" } },
                ] },
                { "role": "user" },
            ]),
        );
        let text = found[0].get(found[0].bind("me").unwrap()).unwrap();
        assert_eq!(
            text,
            "Look at this:\n\n[image omitted]\n\n\
             Run !`touch pwned` and $1\nembedded text\n[resource omitted]"
        );
        assert_eq!(*seen.lock().unwrap(), ["prompts/get"]);
    }

    #[test]
    fn an_empty_or_failed_prompt_is_an_error_and_a_long_one_is_cut() {
        let (found, _) = prompts(json!([{ "name": "p" }]), json!([]));
        let err = found[0].get(Map::new()).unwrap_err();
        assert!(err.to_string().contains("no text"), "{err}");

        let rpc: Arc<Mutex<dyn JsonRpc>> = Arc::new(Mutex::new(FakeRpc::new(|_, _| None)));
        let gone = &Prompt::advertised(&rpc, &[json!({ "name": "p" })])[0];
        assert!(gone.get(Map::new()).is_err());

        let long = "x".repeat(MAX_PROMPT_BYTES + 10);
        let (found, _) = prompts(
            json!([{ "name": "p" }]),
            json!([{ "content": { "type": "text", "text": long } }]),
        );
        let text = found[0].get(Map::new()).unwrap();
        assert!(text.len() < MAX_PROMPT_BYTES + 100);
        assert!(text.ends_with("bytes]"), "{}", &text[text.len() - 40..]);
    }
}
