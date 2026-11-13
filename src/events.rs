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

use serde_json::{Value, json};

use crate::tools::ask::{Answer, Question};

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
}
