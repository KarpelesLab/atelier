//! The event vocabulary atelier speaks to programs.
//!
//! Two interfaces report a session to a process rather than a person:
//! `--print --output-format stream-json` ([`crate::headless`]), one JSON
//! object per line with a `type`, and `atelier serve` ([`crate::serve`]),
//! JSON-RPC notifications named `event/<type>`. They carry the same things —
//! a piece of text, a tool call starting and ending, a question — and a
//! consumer written for one should recognise the other's, so the *payload*
//! of each event is built here, once. What differs is the envelope each
//! interface puts around it (a `type` field; a method name and a turn id).
//!
//! The answer to a question is read here too ([`answer`]): both interfaces
//! take `{"labels": [...]}` or `{"text": "..."}` and hold them to the same
//! rules.

use std::path::Path;

use serde_json::{Value, json};

use crate::tools::ask::{Answer, Question};
use crate::tools::change::{FileChange, Side};

/// The most of a tool's output a `tool_end` event carries, in bytes. Tool
/// results can be whole files; a consumer following along wants the gist,
/// and the event says when it was cut (`"truncated": true`).
const TOOL_OUTPUT_CAP: usize = 16 * 1024;

/// A chunk of text: the model's answer or reasoning, an informational line,
/// a notice, a reviewer's note.
pub fn text(text: &str) -> Value {
    json!({ "text": text })
}

/// A tool call's arguments. They are JSON as the model wrote it; pass them
/// on parsed when they are valid, verbatim when they aren't.
pub fn arguments(raw: &str) -> Value {
    serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::from(raw))
}

/// A tool is about to run.
pub fn tool_start(name: &str, raw_arguments: &str) -> Value {
    json!({ "name": name, "arguments": arguments(raw_arguments) })
}

/// A tool finished (or was refused: then it ends without having started).
pub fn tool_end(name: &str, result: &str, ok: bool) -> Value {
    let (output, truncated) = truncate(result, TOOL_OUTPUT_CAP);
    let mut event = json!({ "name": name, "ok": ok, "output": output });
    if truncated {
        event["truncated"] = Value::Bool(true);
    }
    event
}

/// The most of a diff a `file_change` event carries, in bytes; a longer one
/// is cut at the end of a line and the event says so (`"truncated": true`).
const DIFF_CAP: usize = 64 * 1024;

/// A tool call wrote, created or deleted a file: which, and what changed,
/// as a unified diff. `root` is the project, for the relative path — left
/// out for a file outside it (a subagent's worktree).
///
/// No diff for a file that is not text (`"binary": true`) or is too long to
/// compare (`"truncated": true`, over
/// [`TEXT_CAP`](crate::tools::change::TEXT_CAP)); the event still says the
/// file changed.
pub fn file_change(tool: &str, change: &FileChange, root: &Path) -> Value {
    let absolute = change.path.display().to_string();
    // With `/`, whatever the platform: the name a diff header carries.
    let relative = change.path.strip_prefix(root).ok().map(|rel| {
        let parts: Vec<_> = rel.iter().map(|part| part.to_string_lossy()).collect();
        parts.join("/")
    });
    let mut event = json!({
        "tool": tool,
        "path": absolute,
        "kind": change.kind().name(),
    });
    let name = relative.as_deref().unwrap_or(&absolute);
    let name = name.trim_start_matches('/');
    if let Some(relative) = &relative {
        event["relative_path"] = Value::from(relative.as_str());
    }
    let text = |side: &Side| match side {
        Side::Absent => Some(String::new()),
        Side::Text(text) => Some(text.clone()),
        Side::Binary | Side::Large => None,
    };
    let sides = [&change.before, &change.after];
    if sides.contains(&&Side::Binary) {
        event["binary"] = Value::Bool(true);
    } else if let (Some(before), Some(after)) = (text(&change.before), text(&change.after)) {
        let diff = crate::diff::unified(&before, &after);
        let label = |side: &Side, prefix: &str| match side {
            Side::Absent => "/dev/null".to_string(),
            _ => format!("{prefix}/{name}"),
        };
        let full = format!(
            "--- {}\n+++ {}\n{}",
            label(&change.before, "a"),
            label(&change.after, "b"),
            diff.hunks
        );
        let (cut, truncated) = truncate(&full, DIFF_CAP);
        // Whole lines: half a line of a diff reads as a different change.
        let cut = match truncated {
            true => &cut[..cut.rfind('\n').map_or(0, |end| end + 1)],
            false => cut,
        };
        event["diff"] = Value::from(cut);
        event["added"] = Value::from(diff.added);
        event["removed"] = Value::from(diff.removed);
        if truncated {
            event["truncated"] = Value::Bool(true);
        }
    } else {
        event["truncated"] = Value::Bool(true);
    }
    event
}

/// A question from the model (the `ask` tool), with its options.
pub fn question(q: &Question) -> Value {
    let options: Vec<Value> = q
        .options
        .iter()
        .map(|o| match &o.description {
            Some(d) => json!({ "label": o.label, "description": d }),
            None => json!({ "label": o.label }),
        })
        .collect();
    json!({
        "question": q.question,
        "options": options,
        "multi_select": q.multi_select,
        "allow_other": q.allow_other,
    })
}

/// Read the answer to `question` out of `v`: `{"labels": [...]}` or
/// `{"text": "..."}`. Labels must be options of the question, exactly as
/// given (the asker has them from the [`question`] event); `text` is the
/// free answer and is only taken where the question allows one. The error
/// says what is wrong with an answer that can't be accepted.
pub fn answer(v: &Value, question: &Question) -> Result<Answer, String> {
    if let Some(labels) = v["labels"].as_array() {
        let mut chosen = Vec::new();
        // In the order offered, whatever order they were sent in.
        for option in &question.options {
            if labels.iter().any(|l| l.as_str() == Some(&option.label)) {
                chosen.push(option.label.clone());
            }
        }
        let unknown = labels
            .iter()
            .find(|l| !chosen.iter().any(|c| l.as_str() == Some(c)));
        return match (unknown, chosen.len()) {
            (Some(l), _) => Err(format!("{l} is not a label of this question")),
            (None, 0) => Err("\"labels\" is empty".to_string()),
            (None, 2..) if !question.multi_select => {
                Err("this question takes one label".to_string())
            }
            (None, _) => Ok(Answer::Labels(chosen)),
        };
    }
    match v["text"].as_str().map(str::trim) {
        Some(text) if !text.is_empty() && question.allow_other => {
            Ok(Answer::Text(text.to_string()))
        }
        Some(text) if !text.is_empty() => {
            Err("this question only takes its own labels".to_string())
        }
        _ => Err(r#"an answer needs "labels":[...] or "text":"...""#.to_string()),
    }
}

/// Cut `text` to at most `cap` bytes, on a character boundary; says whether
/// anything was dropped.
pub(crate) fn truncate(text: &str, cap: usize) -> (&str, bool) {
    if text.len() <= cap {
        return (text, false);
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("abc", 10), ("abc", false));
        assert_eq!(truncate("abcdef", 3), ("abc", true));
        // 'é' is two bytes; a cut in its middle backs off.
        assert_eq!(truncate("aé", 2), ("a", true));
    }

    #[test]
    fn tool_events_carry_parsed_arguments_and_capped_output() {
        assert_eq!(
            tool_start("read", r#"{"path":"a"}"#),
            json!({"name": "read", "arguments": {"path": "a"}})
        );
        // Arguments that aren't JSON are passed on as written.
        assert_eq!(tool_start("read", "{oops")["arguments"], "{oops");
        assert_eq!(
            tool_end("read", "body", true),
            json!({"name": "read", "ok": true, "output": "body"})
        );
        let long = "x".repeat(TOOL_OUTPUT_CAP + 1);
        let cut = tool_end("read", &long, false);
        assert_eq!(cut["truncated"], true);
        assert_eq!(cut["output"].as_str().unwrap().len(), TOOL_OUTPUT_CAP);
    }

    #[test]
    fn a_file_change_carries_its_paths_kind_and_diff() {
        let root = std::env::temp_dir().join("atelier-events-root");
        let change = |before: Side, after: Side| FileChange {
            path: root.join("src").join("a.txt"),
            before,
            after,
        };
        let text = |s: &str| Side::Text(s.to_string());

        let edited = file_change("edit", &change(text("one\ntwo\n"), text("one\n2\n")), &root);
        assert_eq!(
            edited,
            json!({
                "tool": "edit",
                "path": root.join("src").join("a.txt").display().to_string(),
                "relative_path": "src/a.txt",
                "kind": "modified",
                "diff": "--- a/src/a.txt\n+++ b/src/a.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+2\n",
                "added": 1,
                "removed": 1,
            })
        );

        let created = file_change("write", &change(Side::Absent, text("new\n")), &root);
        assert_eq!(created["kind"], "created");
        assert_eq!(
            created["diff"],
            "--- /dev/null\n+++ b/src/a.txt\n@@ -0,0 +1 @@\n+new\n"
        );
        let deleted = file_change("apply_patch", &change(text("old\n"), Side::Absent), &root);
        assert_eq!(deleted["kind"], "deleted");
        assert_eq!(
            deleted["diff"],
            "--- a/src/a.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n"
        );

        // Not text, or too much of it: the change without a diff.
        let binary = file_change("write", &change(Side::Binary, text("now text\n")), &root);
        assert_eq!(binary["binary"], true);
        assert!(binary.get("diff").is_none() && binary.get("truncated").is_none());
        let large = file_change("write", &change(Side::Large, text("small\n")), &root);
        assert_eq!(large["truncated"], true);
        assert!(large.get("diff").is_none());

        // A long diff is cut at the end of a line; the counts are the whole
        // change's.
        let long: String = (0..20_000).map(|n| format!("line {n}\n")).collect();
        let cut = file_change("write", &change(Side::Absent, text(&long)), &root);
        let diff = cut["diff"].as_str().unwrap();
        assert!(diff.len() <= DIFF_CAP && diff.ends_with('\n'));
        assert_eq!(cut["truncated"], true);
        assert_eq!(cut["added"], 20_000);

        // Outside the project: no relative path.
        let elsewhere = file_change("edit", &change(text("a"), text("b")), Path::new("/nowhere"));
        assert!(elsewhere.get("relative_path").is_none());
    }
}
