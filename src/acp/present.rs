//! atelier's things in the Agent Client Protocol's vocabulary.
//!
//! The protocol half of [`crate::acp`] moves messages; this half decides
//! what is in them, and is nothing but functions of their inputs, so it can
//! be tested without a client: how a tool call is shown (a kind for the
//! icon, a title, the files it touches, an edit as a diff), how the todo
//! list becomes a plan, how a prompt made of content blocks becomes the
//! text atelier's loop takes, and how the `ask` tool's question becomes a
//! form.
//!
//! Field names are the protocol's (camelCase, see the schema); every shape
//! built here is held to the published JSON schema by `tests/acp.rs`.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::tools::ask::Question;
use crate::tools::{self, apply_patch, path_arg};

/// The most of a tool's output a `tool_call_update` carries, in bytes: the
/// editor shows the gist, the model has the whole (see `events`, which
/// caps `tool_end` the same way).
const OUTPUT_CAP: usize = 16 * 1024;

/// The longest title a tool call is given, in characters: it is one line in
/// the editor's transcript. The whole input is in `rawInput`.
const TITLE_CHARS: usize = 120;

/// A file larger than this is not sent as the "before" of a `write`.
const DIFF_BYTES: u64 = 1024 * 1024;

/// A text content block.
pub fn text(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

/// A tool call's content entry holding text.
pub fn text_content(body: &str) -> Value {
    json!({ "type": "content", "content": text(body) })
}

/// A tool's output as content, cut to what an editor needs of it.
pub fn output(result: &str) -> Value {
    let (shown, truncated) = crate::events::truncate(result, OUTPUT_CAP);
    match truncated {
        true => text_content(&format!("{shown}\n… (truncated)")),
        false => text_content(shown),
    }
}

/// The protocol's category for a tool, which picks the editor's icon and
/// how it lays the call out. There is no `delete` or `move` among them:
/// atelier has no tool for either (`bash` does both, and is `execute`).
pub fn kind(tool: &str) -> &'static str {
    match tool {
        "read" | "skill" | "bash_output" => "read",
        "write" | "edit" | "multiedit" | "apply_patch" | "notebook_edit" => "edit",
        "grep" | "glob" | "ls" | "tree" | "web_search" | "mcp_search" => "search",
        "bash" | "bash_kill" | "node" => "execute",
        "web_fetch" => "fetch",
        "todo" | "task" => "think",
        _ => "other",
    }
}

/// One line saying what this call does, for the transcript.
pub fn title(tool: &str, args: &Value) -> String {
    let arg = |name: &str| args.get(name).and_then(Value::as_str).unwrap_or("");
    let path = path_arg(args).unwrap_or("");
    // An argument the model left out would leave a verb with nothing after
    // it: the tool's name says more.
    let doing = |verb: &str, what: &str| match what.trim() {
        "" => tool.to_string(),
        what => format!("{verb}{what}"),
    };
    let title = match tool {
        "read" => doing("Read ", path),
        "write" => doing("Write ", path),
        "edit" | "multiedit" | "notebook_edit" => doing("Edit ", path),
        "apply_patch" => doing("Patch ", &apply_patch::target_paths(args).join(", ")),
        "bash" => doing("", arg("command").lines().next().unwrap_or("")),
        "grep" => doing("Search for ", arg("pattern")),
        "glob" => doing("Find ", arg("pattern")),
        "ls" | "tree" => doing("List ", if path.is_empty() { "." } else { path }),
        "web_fetch" => doing("Fetch ", arg("url")),
        "web_search" => doing("Search the web for ", arg("query")),
        "todo" => "Update the task list".to_string(),
        "task" => doing("Agent: ", arg("description")),
        "skill" => doing("Load the skill ", arg("name")),
        "ask" => doing("", arg("question")),
        _ => match tool.strip_prefix("mcp__").and_then(|t| t.split_once("__")) {
            Some((server, name)) => format!("{name} ({server})"),
            None => tool.to_string(),
        },
    };
    match title.char_indices().nth(TITLE_CHARS) {
        Some((cut, _)) => format!("{}…", &title[..cut]),
        None => title,
    }
}

/// `path` as the protocol wants every path: absolute. A tool's own path is
/// relative to the project root.
fn absolute(root: &Path, path: &str) -> PathBuf {
    tools::normalize(&root.join(path))
}

/// The files a call reads or changes, so the editor can follow along —
/// with the line where that is known from the arguments: where a `read`
/// starts, where the text an `edit` replaces is now.
pub fn locations(tool: &str, args: &Value, root: &Path) -> Vec<Value> {
    let place = |path: &str, line: Option<u64>| {
        let mut place = json!({ "path": absolute(root, path).display().to_string() });
        if let Some(line) = line {
            place["line"] = Value::from(line);
        }
        place
    };
    match tool {
        "apply_patch" => {
            let mut paths = apply_patch::target_paths(args);
            paths.dedup();
            paths.iter().map(|p| place(p, None)).collect()
        }
        "read" | "write" | "edit" | "multiedit" | "notebook_edit" => {
            let Ok(path) = path_arg(args) else {
                return Vec::new();
            };
            let line = match tool {
                "read" => args.get("offset").and_then(Value::as_u64),
                "edit" => line_of(&absolute(root, path), &args["old_string"]),
                "multiedit" => line_of(&absolute(root, path), &args["edits"][0]["old_string"]),
                _ => None,
            };
            vec![place(path, line)]
        }
        _ => Vec::new(),
    }
}

/// The 1-based line `needle` starts on in the file at `path`, if it is
/// there. Read from the disk: a hint for the editor's cursor, nothing the
/// edit depends on.
fn line_of(path: &Path, needle: &Value) -> Option<u64> {
    let needle = needle.as_str().filter(|n| !n.is_empty())?;
    let text = std::fs::read_to_string(path).ok()?;
    let at = text.find(needle)?;
    Some(text[..at].matches('\n').count() as u64 + 1)
}

/// The change a call makes, as the protocol's diff entries, from its
/// arguments: the text an `edit` replaces and its replacement, a `write`'s
/// whole file against the one on disk, an `apply_patch` hunk by hunk. Empty
/// for the tools that change no file.
///
/// The "old" and "new" of `edit`, `multiedit` and `apply_patch` are the
/// lines concerned, not the whole file: that is what the arguments hold.
pub fn diffs(tool: &str, args: &Value, root: &Path) -> Vec<Value> {
    let diff = |path: &str, old: Option<&str>, new: &str| {
        json!({
            "type": "diff",
            "path": absolute(root, path).display().to_string(),
            "oldText": old,
            "newText": new,
        })
    };
    let replaced = |path: &str, edit: &Value| {
        let old = edit.get("old_string").and_then(Value::as_str)?;
        let new = edit.get("new_string").and_then(Value::as_str)?;
        Some(diff(path, Some(old), new))
    };
    match (tool, path_arg(args)) {
        ("edit", Ok(path)) => replaced(path, args).into_iter().collect(),
        ("multiedit", Ok(path)) => {
            let edits = args.get("edits").and_then(Value::as_array);
            let edits = edits.into_iter().flatten();
            edits.filter_map(|edit| replaced(path, edit)).collect()
        }
        ("write", Ok(path)) => {
            let Some(content) = args.get("content").and_then(Value::as_str) else {
                return Vec::new();
            };
            // What is there now, when it is text of a sensible size; a new
            // file has no "before" (`null`).
            let target = absolute(root, path);
            let small = std::fs::metadata(&target).is_ok_and(|m| m.len() <= DIFF_BYTES);
            let before = small.then(|| std::fs::read_to_string(&target).ok());
            vec![diff(path, before.flatten().as_deref(), content)]
        }
        ("apply_patch", _) => apply_patch::changes(args)
            .iter()
            .map(|(path, old, new)| diff(path, old.as_deref(), new))
            .collect(),
        _ => Vec::new(),
    }
}

/// The `todo` tool's list as the protocol's plan: every entry, each time
/// (the client replaces what it had). atelier's items are done or not, and
/// carry no priority.
pub fn plan(root: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(root.join(".atelier").join("todos.json")).ok()?;
    let items: Vec<Value> = serde_json::from_str(&raw).ok()?;
    let entry = |item: &Value| {
        let done = item["done"].as_bool().unwrap_or(false);
        Some(json!({
            "content": item["text"].as_str()?,
            "priority": "medium",
            "status": if done { "completed" } else { "pending" },
        }))
    };
    Some(json!({
        "sessionUpdate": "plan",
        "entries": items.iter().filter_map(entry).collect::<Vec<_>>(),
    }))
}

/// A prompt as atelier's loop takes it: text, and images to attach.
#[derive(Debug, Default, PartialEq)]
pub struct Prompt {
    pub text: String,
    /// `data:` URLs, for [`crate::agent::Session::stage_images`].
    pub images: Vec<String>,
}

/// Turn the content blocks of a `session/prompt` into a [`Prompt`].
///
/// Text is taken as written. A resource link becomes a Markdown link in
/// its place, with the path spelled out for a `file://` one, so the agent
/// can `read` it. An embedded resource leaves the same link where it stood
/// and its text at the end, fenced and labelled with where it came from —
/// it may be an unsaved buffer, which no `read` would find. Images (and
/// embedded resources that are images) are attached.
///
/// `Err` says which block cannot be taken: the answer to the request is
/// then "invalid params".
pub fn prompt(blocks: &[Value]) -> Result<Prompt, String> {
    let mut out = Prompt::default();
    let mut attached = String::new();
    for (i, block) in blocks.iter().enumerate() {
        let field = |holder: &Value, name: &str| -> Result<String, String> {
            match holder.get(name).and_then(Value::as_str) {
                Some(value) => Ok(value.to_string()),
                None => Err(format!("prompt[{i}]: \"{name}\" must be a string")),
            }
        };
        match block.get("type").and_then(Value::as_str) {
            Some("text") => out.text.push_str(&field(block, "text")?),
            Some("resource_link") => {
                let uri = field(block, "uri")?;
                out.text.push_str(&link(&field(block, "name")?, &uri));
            }
            Some("image") => {
                let (mime, data) = (field(block, "mimeType")?, field(block, "data")?);
                out.images.push(format!("data:{mime};base64,{data}"));
            }
            Some("resource") => {
                let resource = &block["resource"];
                let uri = field(resource, "uri")?;
                let mime = resource.get("mimeType").and_then(Value::as_str);
                let name = uri.rsplit('/').next().unwrap_or(&uri);
                out.text.push_str(&link(name, &uri));
                if let Some(body) = resource.get("text").and_then(Value::as_str) {
                    let from = file_path(&uri).unwrap_or_else(|| uri.clone());
                    attached.push_str(&format!("\n\n<file path=\"{from}\">\n{body}\n</file>"));
                } else if let Some(mime) = mime.filter(|m| m.starts_with("image/")) {
                    let data = field(resource, "blob")?;
                    out.images.push(format!("data:{mime};base64,{data}"));
                } else {
                    // Bytes that are not an image: nothing the model can be
                    // shown. Its name stays in the text.
                    field(resource, "blob")?;
                }
            }
            Some(other) => {
                return Err(format!("prompt[{i}]: unsupported content type \"{other}\""));
            }
            None => return Err(format!("prompt[{i}]: a content block needs a \"type\"")),
        }
    }
    out.text.push_str(&attached);
    Ok(out)
}

/// A resource named in the text: `[name](uri)`, and for a file its path.
fn link(name: &str, uri: &str) -> String {
    match file_path(uri) {
        Some(path) => format!("[{name}]({path})"),
        None => format!("[{name}]({uri})"),
    }
}

/// The path a `file://` URI names (percent-decoded), if it is one.
fn file_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // `file://host/path`: only the local form names a path here.
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return None;
    }
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (bytes[i], bytes.get(i + 1), bytes.get(i + 2)) {
            (b'%', Some(&hi), Some(&lo)) if hex(hi).is_some() && hex(lo).is_some() => {
                decoded.push((hex(hi)? * 16 + hex(lo)?) as u8);
                i += 3;
            }
            (byte, _, _) => {
                decoded.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(decoded).ok()
}

/// The names of a question's form fields (see [`question_form`]).
const CHOICE: &str = "choice";
const OTHER: &str = "other";

/// The `ask` tool's question as the `requestedSchema` of a form
/// elicitation: the options as one field — a string to pick, or an array
/// when several may be chosen — and, where the question allows an answer of
/// the user's own, a free-text field next to it.
pub fn question_form(question: &Question) -> Value {
    let options: Vec<Value> = question
        .options
        .iter()
        .map(|o| {
            let title = match &o.description {
                Some(description) => format!("{} — {description}", o.label),
                None => o.label.clone(),
            };
            json!({ "const": o.label, "title": title })
        })
        .collect();
    let mut properties = Map::new();
    let choice = match question.multi_select {
        true => json!({ "type": "array", "title": "Options", "items": { "anyOf": options } }),
        false => json!({ "type": "string", "title": "Option", "oneOf": options }),
    };
    properties.insert(CHOICE.to_string(), choice);
    if question.allow_other {
        let other = json!({ "type": "string", "title": "Something else" });
        properties.insert(OTHER.to_string(), other);
    }
    // With a free-text field the choice is optional: either answers.
    let required: Vec<&str> = if question.allow_other {
        Vec::new()
    } else {
        vec![CHOICE]
    };
    json!({ "type": "object", "properties": properties, "required": required })
}

/// The submitted form's `content` as the answer [`crate::events::answer`]
/// reads (`{"labels": [...]}` or `{"text": "..."}`): what was typed wins
/// over what was ticked, as it does at the terminal's prompt.
pub fn question_answer(content: &Value) -> Value {
    if let Some(typed) = content[OTHER].as_str().filter(|t| !t.trim().is_empty()) {
        return json!({ "text": typed });
    }
    match &content[CHOICE] {
        Value::String(label) => json!({ "labels": [label] }),
        Value::Array(labels) => json!({ "labels": labels }),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ask::Choice;

    #[test]
    fn tools_have_a_kind_and_a_title() {
        assert_eq!(kind("read"), "read");
        assert_eq!(kind("multiedit"), "edit");
        assert_eq!(kind("bash"), "execute");
        assert_eq!(kind("grep"), "search");
        assert_eq!(kind("web_fetch"), "fetch");
        assert_eq!(kind("todo"), "think");
        assert_eq!(kind("mcp__github__create_issue"), "other");

        assert_eq!(
            title("read", &json!({"path": "src/main.rs"})),
            "Read src/main.rs"
        );
        // The names Claude models use for a path are taken too.
        assert_eq!(title("edit", &json!({"file_path": "a.rs"})), "Edit a.rs");
        assert_eq!(
            title("bash", &json!({"command": "cargo test\necho done"})),
            "cargo test"
        );
        assert_eq!(title("ls", &json!({})), "List .");
        assert_eq!(
            title("mcp__github__create_issue", &json!({})),
            "create_issue (github)"
        );
        // Missing arguments fall back to the tool's name; long ones are cut.
        assert_eq!(title("grep", &json!({})), "grep");
        assert_eq!(title("bash", &json!({"command": ""})), "bash");
        let long = title("bash", &json!({"command": "x".repeat(500)}));
        assert_eq!(long.chars().count(), TITLE_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn file_tools_say_where_and_what_they_change() {
        let root = std::env::temp_dir().join(format!("atelier-acp-present-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let at = |name: &str| root.join(name).display().to_string();

        assert_eq!(
            locations("read", &json!({"path": "a.txt", "offset": 2}), &root),
            vec![json!({"path": at("a.txt"), "line": 2})]
        );
        let edit = json!({"path": "a.txt", "old_string": "three", "new_string": "3"});
        assert_eq!(
            locations("edit", &edit, &root),
            vec![json!({"path": at("a.txt"), "line": 3})]
        );
        assert_eq!(
            diffs("edit", &edit, &root),
            vec![json!({"type": "diff", "path": at("a.txt"), "oldText": "three", "newText": "3"})]
        );
        // A write over an existing file shows what was there; a new file
        // has no "before".
        let over = diffs("write", &json!({"path": "a.txt", "content": "new"}), &root);
        assert_eq!(over[0]["oldText"], "one\ntwo\nthree\n");
        let fresh = diffs("write", &json!({"path": "b.txt", "content": "new"}), &root);
        assert_eq!(
            fresh,
            vec![json!({"type": "diff", "path": at("b.txt"), "oldText": null, "newText": "new"})]
        );
        let multi = json!({"path": "a.txt", "edits": [
            {"old_string": "one", "new_string": "1"},
            {"old_string": "two", "new_string": "2"},
        ]});
        assert_eq!(diffs("multiedit", &multi, &root).len(), 2);
        let patch = json!({"patch": "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+2\n"});
        assert_eq!(
            diffs("apply_patch", &patch, &root),
            vec![json!({
                "type": "diff", "path": at("a.txt"), "oldText": "one\ntwo", "newText": "one\n2",
            })]
        );
        assert_eq!(
            locations("apply_patch", &patch, &root),
            vec![json!({"path": at("a.txt")})]
        );
        // Tools that change no file have neither.
        assert!(diffs("bash", &json!({"command": "ls"}), &root).is_empty());
        assert!(locations("bash", &json!({"command": "ls"}), &root).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_todo_list_is_a_plan() {
        let root = std::env::temp_dir().join(format!("atelier-acp-plan-{}", std::process::id()));
        assert_eq!(plan(&root), None);
        std::fs::create_dir_all(root.join(".atelier")).unwrap();
        let todos = json!([{"text": "write it", "done": true}, {"text": "test it", "done": false}]);
        std::fs::write(root.join(".atelier/todos.json"), todos.to_string()).unwrap();
        assert_eq!(
            plan(&root).unwrap(),
            json!({"sessionUpdate": "plan", "entries": [
                {"content": "write it", "priority": "medium", "status": "completed"},
                {"content": "test it", "priority": "medium", "status": "pending"},
            ]})
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn content_blocks_become_a_prompt() {
        let blocks = [
            json!({"type": "text", "text": "Compare "}),
            json!({"type": "resource_link", "uri": "file:///p/my%20notes.md", "name": "notes"}),
            json!({"type": "text", "text": " with "}),
            json!({"type": "resource", "resource": {
                "uri": "file:///p/src/lib.rs", "mimeType": "text/x-rust", "text": "fn main() {}",
            }}),
            json!({"type": "text", "text": " and "}),
            json!({"type": "resource_link", "uri": "https://example.com/spec", "name": "the spec"}),
            json!({"type": "image", "mimeType": "image/png", "data": "QUJD"}),
            json!({"type": "resource", "resource": {
                "uri": "file:///p/shot.png", "mimeType": "image/png", "blob": "REVG",
            }}),
        ];
        let prompt = prompt(&blocks).unwrap();
        assert_eq!(
            prompt.text,
            "Compare [notes](/p/my notes.md) with [lib.rs](/p/src/lib.rs) and \
             [the spec](https://example.com/spec)[shot.png](/p/shot.png)\n\n\
             <file path=\"/p/src/lib.rs\">\nfn main() {}\n</file>"
        );
        assert_eq!(
            prompt.images,
            ["data:image/png;base64,QUJD", "data:image/png;base64,REVG"]
        );

        let audio = [json!({"type": "audio", "mimeType": "audio/wav", "data": ""})];
        assert_eq!(
            super::prompt(&audio).unwrap_err(),
            "prompt[0]: unsupported content type \"audio\""
        );
        assert!(super::prompt(&[json!({"type": "text"})]).is_err());
        assert!(super::prompt(&[json!("text")]).is_err());
    }

    #[test]
    fn a_question_is_a_form_and_its_content_an_answer() {
        let mut question = Question {
            question: "Which one?".to_string(),
            options: vec![
                Choice {
                    label: "Alpha".to_string(),
                    description: Some("the first".to_string()),
                },
                Choice {
                    label: "Beta".to_string(),
                    description: None,
                },
            ],
            multi_select: false,
            allow_other: false,
        };
        assert_eq!(
            question_form(&question),
            json!({
                "type": "object",
                "properties": {"choice": {"type": "string", "title": "Option", "oneOf": [
                    {"const": "Alpha", "title": "Alpha — the first"},
                    {"const": "Beta", "title": "Beta"},
                ]}},
                "required": ["choice"],
            })
        );
        question.multi_select = true;
        question.allow_other = true;
        let form = question_form(&question);
        assert_eq!(form["properties"]["choice"]["type"], "array");
        assert_eq!(form["properties"]["other"]["type"], "string");
        assert_eq!(form["required"], json!([]));

        assert_eq!(
            question_answer(&json!({"choice": "Beta"})),
            json!({"labels": ["Beta"]})
        );
        assert_eq!(
            question_answer(&json!({"choice": ["Alpha", "Beta"], "other": "  "})),
            json!({"labels": ["Alpha", "Beta"]})
        );
        assert_eq!(
            question_answer(&json!({"choice": "Beta", "other": "neither"})),
            json!({"text": "neither"})
        );
        assert_eq!(question_answer(&json!({})), Value::Null);
    }
}
