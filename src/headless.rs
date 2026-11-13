//! Non-interactive mode: `--print`/`-p`.
//!
//! Runs prompts to completion and exits, instead of dropping into the
//! REPL/TUI. Intended for scripting and CI, so stdout is a contract (see
//! `docs/headless.md`), chosen with `--output-format`:
//!
//! - `text` (the default): stdout carries *only* the model's answer —
//!   reasoning, tool activity, and informational messages all go to stderr,
//!   so `atelier -p "..." | some-pipe` sees just the answer text.
//! - `json`: stdout carries one JSON object, the `result` event, once the
//!   prompt is done; progress still goes to stderr.
//! - `stream-json`: stdout carries one JSON object per line as things
//!   happen — an `init` event, then text and reasoning deltas, tool calls,
//!   denied approvals, notices — closed by the same `result` event. Nothing
//!   else is written to stdout, and stderr stays quiet.
//!
//! All three are one [`Ui`], [`HeadlessUi`], that routes each callback by
//! format; the `result` event is built here, around `Session::send`, since
//! only the driver knows how a prompt ended.
//!
//! How a prompt ended is also the exit status, so a script can branch
//! without parsing anything: see the `EXIT_*` constants.
//!
//! `--input-format stream-json` turns the one-shot into a session a parent
//! process drives: each line on stdin is a prompt, run in the same
//! conversation and answered by its own `result`.
//!
//! The `ask` tool needs someone to answer. With `stream-json` both ways
//! there is: the parent process. The question goes out as a `question`
//! event and the run waits for the matching `answer` line on stdin (see
//! [`HeadlessUi::ask_user`]). Every other combination has no one to ask,
//! and the model is told to decide for itself.
//!
//! `--output-schema` asks for an answer of a given JSON shape. It is done
//! with the prompt rather than a provider feature (so it works on every
//! backend): the schema is appended to the prompt, the JSON is pulled out of
//! the final answer and checked by [`crate::schema`], and a failing answer is
//! sent back once with the reason before the run gives up.
//!
//! `atelier review` ([`review`]) is a headless run with its prompt, tools
//! and answer shape decided for the caller; it is built on the pieces here.

pub mod review;

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::time::Instant;

use serde_json::{Value, json};

use crate::agent::{Approval, Session, StepLimit, Ui};
use crate::schema;
use crate::tools::ask::{Answer, Question};

/// Exit status: the run failed (provider, network, I/O).
pub const EXIT_ERROR: u8 = 1;
/// Exit status: bad command line, or a malformed `stream-json` input line.
pub const EXIT_USAGE: u8 = 2;
/// Exit status: a prompt was cut short by `--max-turns`.
pub const EXIT_MAX_TURNS: u8 = 3;
/// Exit status: the answer didn't satisfy `--output-schema`, even after the
/// retry.
pub const EXIT_SCHEMA: u8 = 4;

/// The most of a tool's output a `tool_end` event carries, in bytes. Tool
/// results can be whole files; a consumer following along wants the gist,
/// and the event says when it was cut (`"truncated": true`).
const TOOL_OUTPUT_CAP: usize = 16 * 1024;

/// What stdout carries (`--output-format`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

/// Where prompts come from (`--input-format`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputFormat {
    /// One prompt: the command-line words, or all of stdin.
    Text,
    /// A prompt per stdin line, as `{"type":"user","text":"…"}`.
    StreamJson,
}

/// A headless run, as asked for on the command line (see `cli`).
#[derive(Debug)]
pub struct Options {
    pub output: OutputFormat,
    pub input: InputFormat,
    /// `--max-turns`: the most model requests one prompt may make.
    pub max_turns: Option<u32>,
    /// `--output-schema`: the JSON Schema the final answer must satisfy.
    pub schema: Option<Value>,
    /// The prompt given as arguments; `None` reads it from stdin.
    pub prompt: Option<String>,
}

/// Run the prompts `opts` describes against `session` and return the exit
/// status. Failures are reported on the way (the `result` event, or stderr
/// in `text` mode), not returned.
///
/// With the Claude backend, a one-shot run waits for any background work
/// claude started (subagents, background shells) and renders its reports
/// before the result: exiting earlier would end the claude process and kill
/// that work.
pub fn run(mut session: Session, opts: Options) -> u8 {
    session.set_max_steps(opts.max_turns);
    let mut ui = HeadlessUi {
        format: opts.output,
        structured: opts.schema.is_some(),
        answer: String::new(),
        asks: opts.output == OutputFormat::StreamJson && opts.input == InputFormat::StreamJson,
        questions: 0,
        backlog: VecDeque::new(),
    };
    if opts.output == OutputFormat::StreamJson {
        emit(&init_event(&session));
    }
    let schema = opts.schema.as_ref();

    match opts.input {
        InputFormat::Text => {
            let prompt = match opts.prompt {
                Some(p) => Ok(p),
                None => read_stdin_prompt(),
            };
            let mut outcome = match prompt {
                Ok(p) => run_prompt(&mut session, &mut ui, &p, schema),
                Err(e) => Outcome::failed(End::Error(format!("reading the prompt: {e}"))),
            };
            if let Err(e) = session.wait_background(&mut ui)
                && outcome.end == End::Success
            {
                outcome.end = End::Error(format!("{e:#}"));
            }
            ui.result(&outcome)
        }
        InputFormat::StreamJson => {
            // Line by line without holding the stdin lock: a question asked
            // mid-prompt reads its answer from the same stream.
            while let Some(line) = ui.next_line() {
                let outcome = match line.map_err(|e| e.to_string()).map(|l| parse_input(&l)) {
                    // Blank lines separate nothing; skip them.
                    Ok(Ok(None)) => continue,
                    Ok(Ok(Some(p))) => run_prompt(&mut session, &mut ui, &p, schema),
                    Ok(Err(why)) => Outcome::failed(End::Input(why)),
                    Err(e) => Outcome::failed(End::Error(format!("reading stdin: {e}"))),
                };
                // The first prompt that doesn't succeed ends the run: later
                // ones were written expecting it to have worked.
                let code = ui.result(&outcome);
                if code != 0 {
                    return code;
                }
            }
            // Here the reports follow the last result instead: the parent
            // was told its prompt is done as soon as it was.
            match session.wait_background(&mut ui) {
                Ok(()) => 0,
                Err(e) => {
                    ui.notice(&format!("{e:#}"));
                    EXIT_ERROR
                }
            }
        }
    }
}

/// Read the one-shot prompt from stdin to EOF (used when `--print`/`-p` is
/// given with no prompt words).
fn read_stdin_prompt() -> io::Result<String> {
    let mut s = String::new();
    io::stdin().read_to_string(&mut s)?;
    Ok(s.trim().to_string())
}

/// One line of stdin, without its line ending; `None` at end of input.
fn read_stdin_line() -> Option<io::Result<String>> {
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => Some(Ok(line.trim_end_matches(['\n', '\r']).to_string())),
        Err(e) => Some(Err(e)),
    }
}

/// What a stdin line turned out to be while a question was waiting.
#[derive(Debug, PartialEq)]
enum Reply {
    /// The answer to the question.
    Answer(Answer),
    /// An `answer` line that can't be accepted; the reason is reported and
    /// the question keeps waiting.
    Invalid(String),
    /// Not an answer at all: the next prompt, sent early.
    Other,
}

/// Read a stdin line as the answer to question `id`:
/// `{"type":"answer","id":…,"labels":[…]}` or `{"type":"answer","id":…,
/// "text":"…"}`. Labels must be options of the question, exactly as given
/// (the parent has them from the `question` event); `text` is the free
/// answer and is only taken where the question allows one.
fn parse_answer(line: &str, id: u64, question: &Question) -> Reply {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Reply::Other;
    };
    if v["type"].as_str() != Some("answer") {
        return Reply::Other;
    }
    if v["id"].as_u64() != Some(id) {
        return Reply::Invalid(format!("answer does not carry the question's id ({id})"));
    }
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
            (Some(l), _) => Reply::Invalid(format!("{l} is not a label of this question")),
            (None, 0) => Reply::Invalid("\"labels\" is empty".to_string()),
            (None, 2..) if !question.multi_select => {
                Reply::Invalid("this question takes one label".to_string())
            }
            (None, _) => Reply::Answer(Answer::Labels(chosen)),
        };
    }
    match v["text"].as_str().map(str::trim) {
        Some(text) if !text.is_empty() && question.allow_other => {
            Reply::Answer(Answer::Text(text.to_string()))
        }
        Some(text) if !text.is_empty() => {
            Reply::Invalid("this question only takes its own labels".to_string())
        }
        _ => Reply::Invalid(r#"an answer needs "labels":[...] or "text":"...""#.to_string()),
    }
}

/// The `question` event for the `ask` tool.
fn question_event(id: u64, q: &Question) -> Value {
    let options: Vec<Value> = q
        .options
        .iter()
        .map(|o| match &o.description {
            Some(d) => json!({ "label": o.label, "description": d }),
            None => json!({ "label": o.label }),
        })
        .collect();
    json!({
        "type": "question",
        "id": id,
        "question": q.question,
        "options": options,
        "multi_select": q.multi_select,
        "allow_other": q.allow_other,
    })
}

/// One `--input-format stream-json` line: `{"type":"user","text":"…"}`.
/// `Ok(None)` for a blank line.
fn parse_input(line: &str) -> Result<Option<String>, String> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    let v: Value =
        serde_json::from_str(line).map_err(|e| format!("input line is not JSON: {e}"))?;
    match (v["type"].as_str(), v["text"].as_str()) {
        (Some("user"), Some(text)) => Ok(Some(text.to_string())),
        _ => Err(r#"input line must be {"type":"user","text":"..."}"#.to_string()),
    }
}

/// How a prompt ended.
#[derive(Debug, PartialEq)]
enum End {
    Success,
    /// The run failed; the message says how.
    Error(String),
    /// A `stream-json` input line couldn't be understood.
    Input(String),
    /// `--max-turns` ran out before the model finished.
    MaxTurns,
    /// The answer doesn't satisfy `--output-schema`; the message says where.
    Schema(String),
}

impl End {
    /// The `result` event's `subtype`.
    fn subtype(&self) -> &'static str {
        match self {
            End::Success => "success",
            End::Error(_) => "error",
            End::Input(_) => "input_error",
            End::MaxTurns => "max_turns",
            End::Schema(_) => "schema_error",
        }
    }

    fn exit_code(&self) -> u8 {
        match self {
            End::Success => 0,
            End::Error(_) => EXIT_ERROR,
            End::Input(_) => EXIT_USAGE,
            End::MaxTurns => EXIT_MAX_TURNS,
            End::Schema(_) => EXIT_SCHEMA,
        }
    }

    fn message(&self) -> Option<String> {
        match self {
            End::Success => None,
            End::Error(m) | End::Input(m) => Some(m.clone()),
            End::MaxTurns => Some(StepLimit.to_string()),
            End::Schema(m) => Some(format!("the answer does not satisfy the schema: {m}")),
        }
    }
}

/// Everything the `result` event reports about one prompt.
struct Outcome {
    end: End,
    /// The final answer: the text the model wrote after its last tool call.
    answer: String,
    /// The answer as JSON, once it satisfied `--output-schema`.
    structured: Option<Value>,
    /// Model requests the prompt took (the schema retry included).
    steps: u32,
    duration_ms: u64,
    /// Context size of the last request, and output tokens this prompt
    /// generated (0 when the endpoint doesn't report usage).
    context_tokens: u32,
    output_tokens: u64,
}

impl Outcome {
    /// A prompt that ended before the model was asked anything.
    fn failed(end: End) -> Self {
        Outcome {
            end,
            answer: String::new(),
            structured: None,
            steps: 0,
            duration_ms: 0,
            context_tokens: 0,
            output_tokens: 0,
        }
    }
}

/// Run one prompt to its end — through the schema check and its one retry,
/// when there is a schema.
fn run_prompt(
    session: &mut Session,
    ui: &mut HeadlessUi,
    prompt: &str,
    schema: Option<&Value>,
) -> Outcome {
    let started = Instant::now();
    let output_before = session.usage().1;
    let mut steps = 0;
    let mut structured = None;

    let mut end = match schema {
        Some(s) => send(session, ui, &with_schema(prompt, s), &mut steps),
        None => send(session, ui, prompt, &mut steps),
    };
    if let (End::Success, Some(schema)) = (&end, schema) {
        match structured_answer(schema, &ui.answer) {
            Ok(v) => structured = Some(v),
            Err(why) => {
                ui.notice(&format!(
                    "the answer does not satisfy the output schema ({why}); asking once more"
                ));
                end = send(session, ui, &retry_prompt(&why), &mut steps);
                if end == End::Success {
                    match structured_answer(schema, &ui.answer) {
                        Ok(v) => structured = Some(v),
                        Err(why) => end = End::Schema(why),
                    }
                }
            }
        }
    }

    let (context_tokens, output_after) = session.usage();
    Outcome {
        end,
        answer: ui.answer.clone(),
        structured,
        steps,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        context_tokens,
        output_tokens: output_after.saturating_sub(output_before),
    }
}

/// One `Session::send`, with the answer buffer starting empty and the
/// session's step count added to `steps`.
fn send(session: &mut Session, ui: &mut HeadlessUi, text: &str, steps: &mut u32) -> End {
    ui.answer.clear();
    let result = session.send(text, ui);
    *steps += session.steps();
    match result {
        Ok(()) => End::Success,
        Err(e) if e.is::<StepLimit>() => End::MaxTurns,
        Err(e) => End::Error(format!("{e:#}")),
    }
}

/// The prompt, with the instruction that makes its answer checkable.
fn with_schema(prompt: &str, schema: &Value) -> String {
    format!(
        "{prompt}\n\n\
         When you are done, your final message must be a single JSON value that \
         satisfies the JSON Schema below, and nothing else: no prose before or \
         after it.\n\n{schema}"
    )
}

fn retry_prompt(why: &str) -> String {
    format!(
        "Your final message does not satisfy the required JSON Schema: {why}\n\
         Reply with the corrected JSON value only."
    )
}

/// The JSON value in `answer`, if it satisfies `schema`; otherwise what is
/// wrong, worded so the model can fix it.
fn structured_answer(schema: &Value, answer: &str) -> Result<Value, String> {
    let value = extract_json(answer).ok_or("the message is not a JSON value")?;
    schema::validate(schema, &value)?;
    Ok(value)
}

/// Find the JSON value in a model's answer. Models asked for "JSON only"
/// still wrap it in a ```` ```json ```` fence or a sentence often enough that
/// refusing those would waste the retry, so, in order: the whole answer, the
/// contents of the first code fence, then the outermost `{…}` or `[…]`.
fn extract_json(answer: &str) -> Option<Value> {
    let text = answer.trim();
    if let Ok(v) = serde_json::from_str(text) {
        return Some(v);
    }
    if let Some((_, rest)) = text.split_once("```") {
        // Skip the info string (`json`) up to the end of the fence's line.
        let body = rest.split_once('\n').map_or(rest, |(_, b)| b);
        let body = body.split_once("```").map_or(body, |(b, _)| b);
        if let Ok(v) = serde_json::from_str(body.trim()) {
            return Some(v);
        }
    }
    [('{', '}'), ('[', ']')].iter().find_map(|&(open, close)| {
        let (start, end) = (text.find(open)?, text.rfind(close)?);
        serde_json::from_str(text.get(start..=end)?).ok()
    })
}

/// The first `stream-json` event: what is about to run.
fn init_event(session: &Session) -> Value {
    json!({
        "type": "system",
        "subtype": "init",
        "version": env!("CARGO_PKG_VERSION"),
        // The label names the model actually asked (`chatgpt:<model>` when
        // signed in with ChatGPT), which the configured one may not be.
        "model": session.backend_label(),
        "cwd": std::env::current_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_default(),
        "tools": session.tool_names(),
    })
}

/// The `result` event for a finished prompt.
fn result_event(o: &Outcome) -> Value {
    let mut v = json!({
        "type": "result",
        "subtype": o.end.subtype(),
        "is_error": o.end != End::Success,
        "result": o.answer,
        "num_steps": o.steps,
        "duration_ms": o.duration_ms,
        "usage": {
            "context_tokens": o.context_tokens,
            "output_tokens": o.output_tokens,
        },
    });
    if let Some(m) = o.end.message() {
        v["error"] = Value::from(m);
    }
    if let Some(s) = &o.structured {
        v["structured"] = s.clone();
    }
    v
}

/// Write one event as a line on stdout, flushed so a parent process reading
/// the stream sees it now. A closed stdout isn't worth a panic: the reader
/// went away, and the exit status still tells the story.
fn emit(event: &Value) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{event}");
    let _ = out.flush();
}

/// Cut `text` to at most `cap` bytes, on a character boundary; says whether
/// anything was dropped.
fn truncate(text: &str, cap: usize) -> (&str, bool) {
    if text.len() <= cap {
        return (text, false);
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// The [`Ui`] of a headless run. In `text` mode only the model's answer goes
/// to stdout so it stays clean for piping, and everything else — reasoning,
/// tool activity, info and notices — goes to stderr; `json` keeps the stderr
/// side and holds the answer back for the result; `stream-json` turns every
/// callback into an event.
struct HeadlessUi {
    format: OutputFormat,
    /// `--output-schema` is in force: in `text` mode stdout is reserved for
    /// the validated JSON, so the answer as written goes to stderr.
    structured: bool,
    /// The answer so far: the text since the last tool call. What the model
    /// says before calling a tool is commentary on the way, not the answer.
    answer: String,
    /// Events out and lines in (`stream-json` both ways): a parent process
    /// is following along and can answer the `ask` tool's questions.
    asks: bool,
    /// Questions asked so far; the next one's `id`.
    questions: u64,
    /// Input lines that arrived while a question was waiting and weren't
    /// its answer: prompts sent ahead, served before stdin is read again.
    backlog: VecDeque<String>,
}

impl HeadlessUi {
    fn streaming(&self) -> bool {
        self.format == OutputFormat::StreamJson
    }

    /// The next input line: one set aside while a question waited, else
    /// from stdin. `None` at end of input.
    fn next_line(&mut self) -> Option<io::Result<String>> {
        match self.backlog.pop_front() {
            Some(line) => Some(Ok(line)),
            None => read_stdin_line(),
        }
    }

    /// Report how a prompt ended, in the output format, and return its exit
    /// status.
    fn result(&mut self, outcome: &Outcome) -> u8 {
        match self.format {
            OutputFormat::Text => {
                if let Some(v) = &outcome.structured {
                    println!("{v}");
                }
                if let Some(m) = outcome.end.message() {
                    eprintln!("Error: {m}");
                }
            }
            OutputFormat::Json | OutputFormat::StreamJson => emit(&result_event(outcome)),
        }
        outcome.end.exit_code()
    }
}

impl Ui for HeadlessUi {
    fn reasoning(&mut self, text: &str) {
        if self.streaming() {
            emit(&json!({ "type": "reasoning", "text": text }));
        } else {
            // Keep it off stdout; surface it for anyone watching the run.
            eprint!("{text}");
        }
    }

    fn content(&mut self, text: &str) {
        self.answer.push_str(text);
        match self.format {
            OutputFormat::StreamJson => emit(&json!({ "type": "text", "text": text })),
            OutputFormat::Text if !self.structured => {
                print!("{text}");
                io::stdout().flush().ok();
            }
            OutputFormat::Text => eprint!("{text}"),
            // The answer arrives whole, in the result.
            OutputFormat::Json => {}
        }
    }

    fn tool_start(&mut self, name: &str, arguments: &str) {
        self.answer.clear();
        if self.streaming() {
            // The arguments are JSON as the model wrote it; pass them on
            // parsed when they are valid, verbatim when they aren't.
            let arguments =
                serde_json::from_str::<Value>(arguments).unwrap_or_else(|_| Value::from(arguments));
            emit(&json!({ "type": "tool_start", "name": name, "arguments": arguments }));
        } else {
            eprintln!("⚙ {name} {arguments}");
        }
    }

    fn tool_end(&mut self, name: &str, result: &str, ok: bool) {
        // Also here: a denied call ends without ever starting.
        self.answer.clear();
        if self.streaming() {
            let (output, truncated) = truncate(result, TOOL_OUTPUT_CAP);
            let mut event = json!({ "type": "tool_end", "name": name, "ok": ok, "output": output });
            if truncated {
                event["truncated"] = Value::Bool(true);
            }
            emit(&event);
        } else {
            let mark = if ok { "✓" } else { "✗" };
            eprintln!("{mark} {name}: {result}");
        }
    }

    fn turn_end(&mut self) {
        match self.format {
            OutputFormat::Text if !self.structured => {
                println!();
                io::stdout().flush().ok();
            }
            OutputFormat::Text => eprintln!(),
            OutputFormat::Json | OutputFormat::StreamJson => {}
        }
    }

    fn info(&mut self, text: &str) {
        if self.streaming() {
            emit(&json!({ "type": "info", "text": text }));
        } else {
            eprintln!("{text}");
        }
    }

    /// Headless runs must never block on stdin waiting for a human. When a
    /// tool needs approval and auto-approve is off, deny it — the model is
    /// told the tool was denied and can try to proceed without it. Set
    /// `ATELIER_APPROVE=all` (see `Session::new`) to allow tools in headless
    /// runs; `Session` then skips approval entirely, so this is only ever
    /// reached when a tool genuinely wants a human in the loop and none is
    /// available.
    fn ask_approval(&mut self, tool: &str, arguments: &str) -> Approval {
        if self.streaming() {
            let arguments =
                serde_json::from_str::<Value>(arguments).unwrap_or_else(|_| Value::from(arguments));
            emit(&json!({
                "type": "approval",
                "tool": tool,
                "arguments": arguments,
                "decision": "denied",
            }));
        }
        Approval::Deny
    }

    /// Only a parent process on both ends of `stream-json` can be asked:
    /// the question is an event, the answer a line on stdin. Anything else
    /// that arrives meanwhile is a prompt sent early and is kept for later;
    /// an answer that doesn't fit is reported and the wait goes on. Input
    /// ending before an answer leaves no one to ask.
    fn ask_user(&mut self, question: &Question) -> Option<Answer> {
        if !self.asks {
            return None;
        }
        self.questions += 1;
        let id = self.questions;
        emit(&question_event(id, question));
        loop {
            let line = read_stdin_line()?.ok()?;
            match parse_answer(&line, id, question) {
                Reply::Answer(answer) => return Some(answer),
                Reply::Invalid(why) => self.notice(&format!("question {id}: {why}")),
                Reply::Other => self.backlog.push_back(line),
            }
        }
    }

    fn notice(&mut self, text: &str) {
        if self.streaming() {
            emit(&json!({ "type": "notice", "text": text }));
        } else {
            eprintln!("{text}");
        }
    }

    fn subconscious(&mut self, text: &str) {
        // Only a stream consumer gets the reviewer's notes; the stderr side
        // of the other formats never showed them.
        if self.streaming() {
            emit(&json!({ "type": "review", "text": text }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_bare_fenced_and_embedded_json() {
        assert_eq!(extract_json(" {\"a\": 1}\n"), Some(json!({"a": 1})));
        assert_eq!(extract_json("[1, 2]"), Some(json!([1, 2])));
        assert_eq!(
            extract_json("\"just a string\""),
            Some(json!("just a string"))
        );
        assert_eq!(
            extract_json("Here you go:\n```json\n{\"a\": [1, 2]}\n```\nDone."),
            Some(json!({"a": [1, 2]}))
        );
        assert_eq!(extract_json("```\n[true]\n```"), Some(json!([true])));
        assert_eq!(
            extract_json("The result is {\"ok\": true}."),
            Some(json!({"ok": true}))
        );
        assert_eq!(extract_json("no json here"), None);
        assert_eq!(extract_json(""), None);
    }

    #[test]
    fn structured_answer_reports_the_violation() {
        let schema = json!({"type": "object", "required": ["name"]});
        assert_eq!(
            structured_answer(&schema, "```json\n{\"name\": \"x\"}\n```"),
            Ok(json!({"name": "x"}))
        );
        assert_eq!(
            structured_answer(&schema, "{}"),
            Err("$: missing required property \"name\"".to_string())
        );
        assert_eq!(
            structured_answer(&schema, "sorry, I can't"),
            Err("the message is not a JSON value".to_string())
        );
    }

    #[test]
    fn parses_input_lines() {
        assert_eq!(
            parse_input(r#"{"type":"user","text":"hi"}"#),
            Ok(Some("hi".to_string()))
        );
        assert_eq!(parse_input("   "), Ok(None));
        assert!(parse_input("hi").unwrap_err().contains("not JSON"));
        assert!(parse_input(r#"{"type":"assistant","text":"x"}"#).is_err());
        assert!(parse_input(r#"{"type":"user"}"#).is_err());
    }

    #[test]
    fn result_event_shape() {
        let ok = result_event(&Outcome {
            end: End::Success,
            answer: "{\"n\":1}".to_string(),
            structured: Some(json!({"n": 1})),
            steps: 2,
            duration_ms: 7,
            context_tokens: 15,
            output_tokens: 5,
        });
        assert_eq!(
            ok,
            json!({
                "type": "result",
                "subtype": "success",
                "is_error": false,
                "result": "{\"n\":1}",
                "structured": {"n": 1},
                "num_steps": 2,
                "duration_ms": 7,
                "usage": {"context_tokens": 15, "output_tokens": 5},
            })
        );
        let limit = result_event(&Outcome::failed(End::MaxTurns));
        assert_eq!(limit["subtype"], "max_turns");
        assert_eq!(limit["is_error"], true);
        assert!(limit["error"].as_str().unwrap().contains("--max-turns"));
        assert!(limit.get("structured").is_none());
    }

    #[test]
    fn ends_map_to_distinct_exit_codes() {
        assert_eq!(End::Success.exit_code(), 0);
        assert_eq!(End::Error(String::new()).exit_code(), EXIT_ERROR);
        assert_eq!(End::Input(String::new()).exit_code(), EXIT_USAGE);
        assert_eq!(End::MaxTurns.exit_code(), EXIT_MAX_TURNS);
        assert_eq!(End::Schema(String::new()).exit_code(), EXIT_SCHEMA);
    }

    #[test]
    fn answers_are_matched_to_the_question_and_checked() {
        let q = Question::from_args(&json!({
            "question": "Which?",
            "options": [{"label": "A", "description": "first"}, "B", "C"],
        }))
        .unwrap();
        assert_eq!(
            question_event(3, &q),
            json!({
                "type": "question",
                "id": 3,
                "question": "Which?",
                "options": [{"label": "A", "description": "first"}, {"label": "B"}, {"label": "C"}],
                "multi_select": false,
                "allow_other": true,
            })
        );

        let reply = |line: &str| parse_answer(line, 3, &q);
        assert_eq!(
            reply(r#"{"type":"answer","id":3,"labels":["B"]}"#),
            Reply::Answer(Answer::Labels(vec!["B".into()]))
        );
        assert_eq!(
            reply(r#"{"type":"answer","id":3,"text":" neither "}"#),
            Reply::Answer(Answer::Text("neither".into()))
        );
        // Not answers: kept as input for later.
        assert_eq!(reply(r#"{"type":"user","text":"next"}"#), Reply::Other);
        assert_eq!(reply("not json"), Reply::Other);
        assert_eq!(reply(""), Reply::Other);
        // Answers that don't fit keep the question waiting.
        let invalid = |line: &str| matches!(reply(line), Reply::Invalid(_));
        assert!(invalid(r#"{"type":"answer","id":2,"labels":["B"]}"#));
        assert!(invalid(r#"{"type":"answer","labels":["B"]}"#));
        assert!(invalid(r#"{"type":"answer","id":3,"labels":["b"]}"#));
        assert!(invalid(r#"{"type":"answer","id":3,"labels":["A","Z"]}"#));
        assert!(invalid(r#"{"type":"answer","id":3,"labels":[]}"#));
        assert!(invalid(r#"{"type":"answer","id":3,"labels":["A","B"]}"#));
        assert!(invalid(r#"{"type":"answer","id":3,"text":""}"#));
        assert!(invalid(r#"{"type":"answer","id":3}"#));

        let multi = Question {
            multi_select: true,
            allow_other: false,
            ..q.clone()
        };
        assert_eq!(
            parse_answer(
                r#"{"type":"answer","id":1,"labels":["C","A","C"]}"#,
                1,
                &multi
            ),
            Reply::Answer(Answer::Labels(vec!["A".into(), "C".into()]))
        );
        assert!(matches!(
            parse_answer(r#"{"type":"answer","id":1,"text":"other"}"#, 1, &multi),
            Reply::Invalid(_)
        ));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("abc", 10), ("abc", false));
        assert_eq!(truncate("abcdef", 3), ("abc", true));
        // 'é' is two bytes; a cut in its middle backs off.
        assert_eq!(truncate("aé", 2), ("a", true));
    }
}
