//! Tool calls a model writes as text instead of structured `tool_calls`.
//!
//! An open model served without its server's tool-call parser (llama.cpp
//! without `--jinja`, vLLM without `--enable-auto-tool-choice`, a template
//! Ollama doesn't know) still calls tools — in its own markup, inside the
//! answer. [`TextCalls`] watches the streamed content for the dialects in
//! use and turns each well-formed call into a real [`ToolCall`]:
//!
//! - Hermes / Qwen: `<tool_call>{"name": …, "arguments": {…}}</tool_call>`;
//! - Qwen3-Coder: `<function=name><parameter=key>value</parameter></function>`,
//!   bare or inside `<tool_call>`;
//! - Mistral: `[TOOL_CALLS][{"name": …, "arguments": {…}}]`, and the newer
//!   `[TOOL_CALLS]name[ARGS]{…}`;
//! - Llama 3.x: `{"name": …, "parameters": {…}}` — only when it is the
//!   whole answer, since a JSON object is also something an answer shows.
//!
//! Only calls of tools the request offered are taken; anything else (a
//! malformed block, an unknown name) stays in the answer as written. Text
//! is passed on as it comes, except from a possible opener onward: that is
//! held until it is known to be a call (and then never shown) or not (and
//! then shown as it was). Inside a `<think>` block nothing is looked for.
//!
//! `ATELIER_TEXT_TOOL_CALLS=off` turns this off. It only matters when the
//! model returned no structured calls: those always win.

use serde_json::{Map, Value};

use super::{ToolCall, ToolSpec};

/// Env var turning text tool calls off: `0`, `off`, `no` or `false`.
const ENV_VAR: &str = "ATELIER_TEXT_TOOL_CALLS";

const TOOL_CALL: &str = "<tool_call>";
const TOOL_CALL_END: &str = "</tool_call>";
const FUNCTION: &str = "<function=";
const FUNCTION_END: &str = "</function>";
const PARAMETER: &str = "<parameter=";
const PARAMETER_END: &str = "</parameter>";
const MISTRAL: &str = "[TOOL_CALLS]";
const MISTRAL_ARGS: &str = "[ARGS]";
const MISTRAL_CALL_ID: &str = "[CALL_ID]";
const THINK: &str = "<think>";
const THINK_END: &str = "</think>";

/// What, in the answer, may begin a call (or a `<think>` block).
const OPENERS: &[&str] = &[TOOL_CALL, FUNCTION, MISTRAL, THINK];

/// Whether text tool calls are looked for: not when the env var says off.
pub(crate) fn enabled() -> bool {
    std::env::var(ENV_VAR).map_or(true, |v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "no" | "false"
        )
    })
}

/// The streamed answer, sorted into text to show and calls to run.
pub(crate) struct TextCalls<'a> {
    tools: &'a [ToolSpec],
    /// Everything the model has sent as content.
    raw: String,
    /// How much of `raw` has been dealt with: shown, or taken as a call.
    done: usize,
    /// What has been shown: the answer, without the calls.
    text: String,
    calls: Vec<ToolCall>,
    /// Inside a `<think>` block of the content.
    thinking: bool,
    /// A call was just taken: a `</tool_call>` right after it is its own.
    after_call: bool,
}

impl<'a> TextCalls<'a> {
    pub(crate) fn new(tools: &'a [ToolSpec]) -> Self {
        Self {
            tools,
            raw: String::new(),
            done: 0,
            text: String::new(),
            calls: Vec::new(),
            thinking: false,
            after_call: false,
        }
    }

    /// Takes a piece of content; returns the part that can be shown now.
    pub(crate) fn push(&mut self, delta: &str) -> String {
        self.raw.push_str(delta);
        self.scan(false)
    }

    /// The stream has ended: returns what is left to show, every held block
    /// now settled one way or the other.
    pub(crate) fn finish(&mut self) -> String {
        self.scan(true)
    }

    /// The answer as shown, calls left out.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// The answer of a stream stopped early: what was shown, then whatever
    /// was held — nothing is run, so nothing is taken out.
    pub(crate) fn interrupted(&self) -> String {
        format!("{}{}", self.text, &self.raw[self.done..])
    }

    pub(crate) fn take_calls(&mut self) -> Vec<ToolCall> {
        std::mem::take(&mut self.calls)
    }

    /// Settles as much of `raw` past `done` as can be, and returns the part
    /// of it that is shown. At `eof`, everything is settled.
    fn scan(&mut self, eof: bool) -> String {
        let mut shown = String::new();
        while self.done < self.raw.len() {
            let rest = &self.raw[self.done..];
            if self.thinking {
                match rest.find(THINK_END) {
                    Some(at) => {
                        let end = at + THINK_END.len();
                        self.show(end, &mut shown);
                        self.thinking = false;
                        continue;
                    }
                    None => {
                        // Only a possible start of `</think>` is held.
                        let keep = if eof {
                            0
                        } else {
                            partial_suffix(rest, &[THINK_END])
                        };
                        self.show(rest.len() - keep, &mut shown);
                        break;
                    }
                }
            }
            // Ollama swallows a `<tool_call>` it fails to parse, but not its
            // closer: that goes with the call taken in its place.
            if self.after_call {
                let closer = rest.trim_start();
                if closer.starts_with(TOOL_CALL_END) {
                    self.done += rest.len() - closer.len() + TOOL_CALL_END.len();
                    self.after_call = false;
                    continue;
                }
                if !eof && TOOL_CALL_END.starts_with(closer) {
                    break;
                }
                self.after_call = false;
            }
            // A whole-answer call: nothing shown yet but a `<think>` block.
            let answer = self.text.rsplit(THINK_END).next().unwrap_or_default();
            if answer.trim().is_empty() && self.calls.is_empty() {
                match whole_json(rest) {
                    Whole::Maybe if !eof => break,
                    Whole::Call if !eof => break,
                    Whole::Call => {
                        let json = rest.trim_end();
                        let json = json.strip_suffix(TOOL_CALL_END).unwrap_or(json);
                        if let Some(calls) = json_calls(json, self.tools) {
                            self.take(rest.len(), calls);
                        } else {
                            self.show(rest.len(), &mut shown);
                        }
                        break;
                    }
                    _ => {}
                }
            }
            let Some((at, opener)) = first_opener(rest) else {
                let keep = if eof {
                    0
                } else {
                    partial_suffix(rest, OPENERS)
                };
                self.show(rest.len() - keep, &mut shown);
                break;
            };
            self.show(at, &mut shown);
            let rest = &self.raw[self.done..];
            let body = &rest[opener.len()..];
            match opener {
                THINK => {
                    self.show(THINK.len(), &mut shown);
                    self.thinking = true;
                }
                TOOL_CALL | FUNCTION => {
                    let end = if opener == TOOL_CALL {
                        TOOL_CALL_END
                    } else {
                        FUNCTION_END
                    };
                    // Through the closer — or, at the end, an unclosed block.
                    let (len, inner) = match body.find(end) {
                        Some(e) => (opener.len() + e + end.len(), &body[..e]),
                        None if eof => (rest.len(), body),
                        None => break,
                    };
                    let calls = if opener == TOOL_CALL {
                        tool_call_body(inner, self.tools)
                    } else {
                        xml_call(inner, self.tools).map(|c| vec![c])
                    };
                    match calls {
                        Some(calls) => self.take(len, calls),
                        None => self.show(len, &mut shown),
                    }
                }
                _ => {
                    // Mistral's calls end the answer: wait for it to end.
                    if !eof {
                        break;
                    }
                    match mistral_calls(body, self.tools) {
                        Some(calls) => self.take(rest.len(), calls),
                        None => self.show(rest.len(), &mut shown),
                    }
                }
            }
        }
        shown
    }

    /// Shows the next `len` bytes.
    fn show(&mut self, len: usize, shown: &mut String) {
        let piece = &self.raw[self.done..self.done + len];
        self.text.push_str(piece);
        shown.push_str(piece);
        self.done += len;
    }

    /// Takes the next `len` bytes as `calls`, giving each an id.
    fn take(&mut self, len: usize, calls: Vec<(String, String)>) {
        for (name, arguments) in calls {
            let id = call_id(self.calls.len());
            self.calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
        self.done += len;
        self.after_call = true;
    }
}

/// A fresh id for the `n`th call recovered from text: unique enough that it
/// doesn't repeat an earlier turn's, which a server may hold against it.
fn call_id(n: usize) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!("call_{:08x}{n}", nanos ^ std::process::id())
}

/// The earliest opener in `text`, and which.
fn first_opener(text: &str) -> Option<(usize, &'static str)> {
    OPENERS
        .iter()
        .filter_map(|&o| text.find(o).map(|at| (at, o)))
        .min_by_key(|&(at, _)| at)
}

/// How many bytes at the end of `text` could be the start of one of
/// `openers` — text that can't be shown until the next piece says.
fn partial_suffix(text: &str, openers: &[&str]) -> usize {
    openers
        .iter()
        .flat_map(|o| (1..o.len()).map(move |n| &o[..n]))
        .filter(|prefix| text.ends_with(prefix))
        .map(str::len)
        .max()
        .unwrap_or(0)
}

/// Where an answer that may be one JSON call stands.
enum Whole {
    /// It doesn't start like one.
    No,
    /// It might: too little has come to say.
    Maybe,
    /// It starts like one (`{"name"` or `{"type"`); the end will tell.
    Call,
}

/// Whether `text`, the answer so far, starts like a Llama-style call.
fn whole_json(text: &str) -> Whole {
    let text = text.trim_start();
    let Some(rest) = text.strip_prefix('{') else {
        return if text.is_empty() {
            Whole::Maybe
        } else {
            Whole::No
        };
    };
    let rest = rest.trim_start();
    let Some(key) = rest.strip_prefix('"') else {
        return if rest.is_empty() {
            Whole::Maybe
        } else {
            Whole::No
        };
    };
    for wanted in ["name\"", "type\""] {
        if key.starts_with(wanted) {
            return Whole::Call;
        }
        if wanted.starts_with(key) {
            return Whole::Maybe;
        }
    }
    Whole::No
}

/// The calls in a `<tool_call>` block: a Qwen3-Coder `<function=…>`, or
/// JSON — one call, or an array of them.
fn tool_call_body(body: &str, tools: &[ToolSpec]) -> Option<Vec<(String, String)>> {
    let body = body.trim();
    if let Some(xml) = body.strip_prefix(FUNCTION) {
        let xml = xml.trim_end();
        let xml = xml.strip_suffix(FUNCTION_END).unwrap_or(xml);
        return xml_call(xml, tools).map(|c| vec![c]);
    }
    json_calls(body, tools)
}

/// Every call in `text`, which must be nothing but JSON calls: objects or
/// arrays of them, apart by whitespace or `;`. `None` if anything else is
/// there, or a call names a tool not offered.
fn json_calls(text: &str, tools: &[ToolSpec]) -> Option<Vec<(String, String)>> {
    let mut calls = Vec::new();
    let mut rest = text;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ';');
        if rest.is_empty() {
            break;
        }
        let (value, len) = first_json(rest)?;
        match value {
            Value::Array(items) => {
                for item in &items {
                    calls.push(json_call(item, tools)?);
                }
            }
            other => calls.push(json_call(&other, tools)?),
        }
        rest = &rest[len..];
    }
    (!calls.is_empty()).then_some(calls)
}

/// The JSON value `text` starts with, and how long it is.
fn first_json(text: &str) -> Option<(Value, usize)> {
    let mut values = serde_json::Deserializer::from_str(text).into_iter::<Value>();
    let value = values.next()?.ok()?;
    Some((value, values.byte_offset()))
}

/// One JSON call: `{"name", "arguments"}` (or `"parameters"`, Llama's), the
/// arguments an object or a string holding one; or the same nested under
/// `"function"`, OpenAI's own shape.
fn json_call(value: &Value, tools: &[ToolSpec]) -> Option<(String, String)> {
    let call = value.as_object()?;
    if let Some(function) = call.get("function").filter(|f| f.is_object()) {
        return json_call(function, tools);
    }
    let name = call.get("name")?.as_str()?;
    offered(name, tools)?;
    let arguments = match call.get("arguments").or_else(|| call.get("parameters")) {
        None | Some(Value::Null) => "{}".to_string(),
        Some(args @ Value::Object(_)) => args.to_string(),
        Some(Value::String(s)) => {
            serde_json::from_str::<Map<String, Value>>(s).ok()?;
            s.clone()
        }
        Some(_) => return None,
    };
    Some((name.to_string(), arguments))
}

/// The tool named `name`, if the request offered it.
fn offered<'t>(name: &str, tools: &'t [ToolSpec]) -> Option<&'t ToolSpec> {
    tools.iter().find(|t| t.name == name)
}

/// A Qwen3-Coder call, from after `<function=` up to (not including) its
/// `</function>`: `name>` then `<parameter=key>value</parameter>` pairs.
/// Each value is read as JSON unless its schema says it is a string — the
/// markup itself doesn't say.
fn xml_call(text: &str, tools: &[ToolSpec]) -> Option<(String, String)> {
    // Unclosed, it may run up to the closer of the `<tool_call>` it was in.
    let text = text.trim_end();
    let text = text.strip_suffix(TOOL_CALL_END).unwrap_or(text);
    let (name, mut rest) = text.split_once('>')?;
    let tool = offered(name.trim(), tools)?;
    let mut args = Map::new();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let param = rest.strip_prefix(PARAMETER)?;
        let (key, after) = param.split_once('>')?;
        // A missing `</parameter>` ends the value at the next parameter.
        let (raw, next) = match after.find(PARAMETER_END) {
            Some(e) => (&after[..e], &after[e + PARAMETER_END.len()..]),
            None => match after.find(PARAMETER) {
                Some(e) => (&after[..e], &after[e..]),
                None => (after, ""),
            },
        };
        let raw = raw.strip_prefix('\n').unwrap_or(raw);
        let raw = raw.strip_suffix('\n').unwrap_or(raw);
        let key = key.trim();
        args.insert(key.to_string(), parameter_value(tool, key, raw));
        rest = next;
    }
    Some((tool.name.clone(), Value::Object(args).to_string()))
}

/// The value of parameter `key` of `tool`, written as `raw`.
fn parameter_value(tool: &ToolSpec, key: &str, raw: &str) -> Value {
    let ty = &tool.parameters["properties"][key]["type"];
    if ty == "string" {
        return Value::String(raw.to_string());
    }
    serde_json::from_str(raw.trim()).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// Mistral's calls, from after the first `[TOOL_CALLS]` to the end of the
/// answer: a JSON array of calls, or `name[ARGS]{…}` (with a `[CALL_ID]…`
/// the newest templates add), possibly several, each after its own
/// `[TOOL_CALLS]`. Nothing else may follow.
fn mistral_calls(text: &str, tools: &[ToolSpec]) -> Option<Vec<(String, String)>> {
    let mut calls = Vec::new();
    let mut rest = text;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        if let Some(next) = rest.strip_prefix(MISTRAL) {
            rest = next;
            continue;
        }
        if rest.starts_with('[') {
            let (value, len) = first_json(rest)?;
            for item in value.as_array()? {
                calls.push(json_call(item, tools)?);
            }
            rest = &rest[len..];
            continue;
        }
        let at = rest.find(MISTRAL_ARGS)?;
        let name = rest[..at].split(MISTRAL_CALL_ID).next()?.trim();
        offered(name, tools)?;
        let args = &rest[at + MISTRAL_ARGS.len()..];
        let (value, len) = first_json(args)?;
        if !value.is_object() {
            return None;
        }
        calls.push((name.to_string(), value.to_string()));
        rest = &args[len..];
    }
    (!calls.is_empty()).then_some(calls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tools() -> Vec<ToolSpec> {
        let spec = |name: &str, parameters: Value| ToolSpec {
            name: name.into(),
            description: String::new(),
            parameters,
        };
        vec![
            spec(
                "read",
                json!({"type": "object", "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer"},
                }}),
            ),
            spec(
                "bash",
                json!({"type": "object", "properties": {"command": {"type": "string"}}}),
            ),
        ]
    }

    /// Feeds `pieces` one by one: what was shown along the way (piece by
    /// piece, then at the end), the final answer and the calls.
    fn run(pieces: &[&str]) -> (Vec<String>, String, Vec<(String, Value)>) {
        let tools = tools();
        let mut tc = TextCalls::new(&tools);
        let mut shown: Vec<String> = pieces.iter().map(|p| tc.push(p)).collect();
        shown.push(tc.finish());
        let calls = tc
            .take_calls()
            .into_iter()
            .map(|c| (c.name, serde_json::from_str(&c.arguments).unwrap()))
            .collect();
        assert_eq!(shown.concat(), tc.text(), "shown is the answer");
        (shown, tc.text().to_string(), calls)
    }

    #[test]
    fn plain_text_passes_through_at_once() {
        let (shown, text, calls) = run(&["Hello ", "world", ", a < b and [x]."]);
        assert_eq!(shown, ["Hello ", "world", ", a < b and [x].", ""]);
        assert_eq!(text, "Hello world, a < b and [x].");
        assert!(calls.is_empty());
    }

    #[test]
    fn a_possible_opener_is_held_only_until_it_is_not_one() {
        let (shown, _, _) = run(&["see <", "b>bold</b>"]);
        assert_eq!(shown, ["see ", "<b>bold</b>", ""]);
        let (shown, _, _) = run(&["x [TOOL", "S] y"]);
        assert_eq!(shown, ["x ", "[TOOLS] y", ""]);
        // A dangling one is shown at the end.
        let (shown, _, _) = run(&["the end <tool_"]);
        assert_eq!(shown, ["the end ", "<tool_"]);
    }

    #[test]
    fn hermes_blocks_become_calls() {
        let (shown, text, calls) = run(&[
            "Let me look.\n<tool",
            "_call>\n{\"name\": \"read\", \"argu",
            "ments\": {\"path\": \"src/main.rs\"}}\n</tool_",
            "call>\n<tool_call>\n{\"name\": \"bash\", \"arguments\": \"{\\\"command\\\":\\\"ls\\\"}\"}\n</tool_call>",
        ]);
        assert_eq!(shown[0], "Let me look.\n");
        assert_eq!(text, "Let me look.\n\n");
        assert_eq!(
            calls,
            [
                ("read".into(), json!({"path": "src/main.rs"})),
                ("bash".into(), json!({"command": "ls"})),
            ]
        );
    }

    #[test]
    fn an_unclosed_block_at_the_end_still_counts() {
        let (_, text, calls) =
            run(&["<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a\"}}"]);
        assert_eq!(text, "");
        assert_eq!(calls, [("read".into(), json!({"path": "a"}))]);
    }

    #[test]
    fn unknown_tools_and_malformed_blocks_stay_text() {
        for block in [
            "<tool_call>{\"name\": \"delete_all\", \"arguments\": {}}</tool_call>",
            "<tool_call>{\"name\": \"read\", \"arguments\": [1]}</tool_call>",
            "<tool_call>not json</tool_call>",
            "<function=nope><parameter=x>1</parameter></function>",
            "[TOOL_CALLS] is the token Mistral uses",
        ] {
            let (_, text, calls) = run(&["before ", block, " after"]);
            assert_eq!(text, format!("before {block} after"));
            assert!(calls.is_empty(), "{block}");
        }
    }

    #[test]
    fn qwen3_coder_xml_becomes_a_call() {
        let (_, text, calls) = run(&[
            "I'll read it.\n\n<tool_call>\n<func",
            "tion=read>\n<parameter=path>\nsrc/main.rs\n</parameter>\n<parameter=off",
            "set>\n10\n</parameter>\n</function>\n</tool_call>",
        ]);
        assert_eq!(text, "I'll read it.\n\n");
        assert_eq!(
            calls,
            [("read".into(), json!({"path": "src/main.rs", "offset": 10}))]
        );
        // Bare, a string that looks like a number left a string, and a
        // missing `</parameter>`.
        let (_, text, calls) = run(&[
            "<function=bash>\n<parameter=command>\n42\n</function>",
            "<function=read><parameter=path>a\nb</parameter></function>",
        ]);
        assert_eq!(text, "");
        assert_eq!(
            calls,
            [
                ("bash".into(), json!({"command": "42"})),
                ("read".into(), json!({"path": "a\nb"})),
            ]
        );
    }

    /// What qwen3-coder sends through Ollama when Ollama's own parser gives
    /// up on the call: the opener eaten, the rest left in the answer.
    #[test]
    fn a_call_whose_opener_the_server_ate_still_counts() {
        let (shown, text, calls) = run(&[
            "<function=read>\n<parameter=path>\nCargo.toml\n</parameter>\n</function>",
            "\n</tool",
            "_call>",
        ]);
        assert!(shown.iter().all(String::is_empty), "{shown:?}");
        assert_eq!(text, "");
        assert_eq!(calls, [("read".into(), json!({"path": "Cargo.toml"}))]);
        let (_, text, calls) = run(&["{\"name\": \"read\", \"arguments\": {}}\n</tool_call>"]);
        assert_eq!(text, "");
        assert_eq!(calls, [("read".into(), json!({}))]);
        // Nor its `</function>`, sometimes.
        let (_, text, calls) =
            run(&["<function=read>\n<parameter=path>\na\n</parameter>\n</tool_call>"]);
        assert_eq!(text, "");
        assert_eq!(calls, [("read".into(), json!({"path": "a"}))]);
        // Only right after a call.
        let (_, text, _) = run(&["see </tool_call> there"]);
        assert_eq!(text, "see </tool_call> there");
    }

    #[test]
    fn mistral_calls_in_both_forms() {
        let (shown, text, calls) = run(&[
            "[TOOL_",
            "CALLS][{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}}, ",
            "{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}]",
        ]);
        assert!(shown.iter().all(String::is_empty), "{shown:?}");
        assert_eq!(text, "");
        assert_eq!(calls.len(), 2);
        let (_, text, calls) = run(&[
            "On it.[TOOL_CALLS]read[ARGS]{\"path\": \"a\"}",
            "[TOOL_CALLS]bash[CALL_ID]abc123def[ARGS]{\"command\": \"ls\"}",
        ]);
        assert_eq!(text, "On it.");
        assert_eq!(
            calls,
            [
                ("read".into(), json!({"path": "a"})),
                ("bash".into(), json!({"command": "ls"})),
            ]
        );
    }

    #[test]
    fn a_whole_json_answer_is_a_call_only_when_whole() {
        let (shown, text, calls) = run(&[
            "{\"na",
            "me\": \"read\", \"parameters\": {\"path\": \"a\"}}",
        ]);
        assert!(shown.iter().all(String::is_empty));
        assert_eq!(text, "");
        assert_eq!(calls, [("read".into(), json!({"path": "a"}))]);
        let (_, _, calls) = run(&[
            "{\"type\": \"function\", \"name\": \"bash\", \"parameters\": {\"command\": \"ls\"}}",
        ]);
        assert_eq!(calls, [("bash".into(), json!({"command": "ls"}))]);
        // Followed by prose, it is an answer showing JSON.
        let answer = "{\"name\": \"read\", \"parameters\": {}}\nis what a call looks like.";
        let (_, text, calls) = run(&[answer]);
        assert_eq!(text, answer);
        assert!(calls.is_empty());
        // Other JSON is shown as it comes.
        let (shown, _, _) = run(&["{\"files\": ", "3}"]);
        assert_eq!(shown, ["{\"files\": ", "3}", ""]);
        // After some prose, a JSON object is never a call.
        let (_, _, calls) = run(&["Here: {\"name\": \"read\", \"parameters\": {}}"]);
        assert!(calls.is_empty());
    }

    #[test]
    fn nothing_is_looked_for_inside_think() {
        let (shown, text, calls) = run(&[
            "<thi",
            "nk>maybe <tool_call>{\"name\":\"read\",\"arguments\":{}}</tool_call></th",
            "ink>Done.",
        ]);
        assert_eq!(shown[0], "");
        assert_eq!(
            text,
            "<think>maybe <tool_call>{\"name\":\"read\",\"arguments\":{}}</tool_call></think>Done."
        );
        assert!(calls.is_empty());
        // After it, calls are.
        let (_, text, calls) = run(&[
            "<think>hm</think>\n<tool_call>{\"name\":\"read\",\"arguments\":{}}</tool_call>",
        ]);
        assert_eq!(text, "<think>hm</think>\n");
        assert_eq!(calls, [("read".into(), json!({}))]);
    }

    #[test]
    fn the_env_var_turns_it_off() {
        // SAFETY: test-only; the one test that sets this var, cleared after.
        unsafe { std::env::set_var(ENV_VAR, "off") };
        assert!(!enabled());
        unsafe { std::env::set_var(ENV_VAR, "1") };
        assert!(enabled());
        unsafe { std::env::remove_var(ENV_VAR) };
        assert!(enabled());
    }
}
