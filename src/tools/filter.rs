//! Showing the model part of a command's output, without a pipe.
//!
//! Agents end commands in `| tail -n 20` or `| grep error` by habit, to
//! keep a result short. The pipe costs more than it saves: the exit status
//! reported is the filter's, not the command's; whatever the filter dropped
//! is gone, so a failure whose message was above the last twenty lines
//! reads as a success with a short log; and the command no longer writes to
//! what it was started on, which is how a prompt goes unseen.
//!
//! So `bash` and `bash_output` take the filter as parameters instead (see
//! [`Filter`]): the command runs unpiped, its own status is reported, and
//! the filter shapes only what the model is shown. All of the output is
//! saved (see [`spill`]) under a handle the result names, so another part
//! of it — a different `tail`, a `grep` with context, a range of lines — is
//! a `bash_output` or a `read` away, not another run.
//!
//! **A filter never hides a failure.** A filtered result says that it is
//! filtered and how much it leaves out; a pattern that matched nothing says
//! so, and how many lines there were; and when the command failed and the
//! filter left out the end of its output, the end is shown anyway
//! ([`FAILURE_TAIL`] lines): that is where the error usually is.
//!
//! A command that still ends in a plain `tail`, `head` or `grep` runs as
//! written — rewriting it would change its exit status, and what of its
//! stderr is filtered — and its result carries a note pointing at the
//! parameters ([`pipe_note`]).

use std::path::Path;

use anyhow::{Context, Result};
use regex::Regex;
use serde_json::{Value, json};

use super::spill;
use crate::permissions::shell;

/// How many lines from the end of a failed command's output are shown when
/// the filter left them out.
pub const FAILURE_TAIL: usize = 20;
/// The most of one such line, in characters.
const FAILURE_LINE_CHARS: usize = 400;

/// What of a command's output the model asked to see.
pub struct Filter {
    /// Only the lines that match (or, with `invert`, that do not)…
    grep: Option<(Regex, String)>,
    invert: bool,
    /// …with this many lines around each.
    context: usize,
    /// Of those, the first this many…
    head: Option<usize>,
    /// …and the last this many.
    tail: Option<usize>,
}

/// The filter's parameters, for the schema of a tool that takes them.
pub fn schema() -> Value {
    json!({
        "tail": {
            "type": "integer",
            "description": "Show only the last N lines of the output. Use this, never `| tail`."
        },
        "head": {
            "type": "integer",
            "description": "Show only the first N lines of the output. Use this, never `| head`."
        },
        "grep": {
            "type": "string",
            "description": "Show only the lines matching this regex. Use this, never `| grep`. With tail or head: the last or first N of the matching lines."
        },
        "context": {
            "type": "integer",
            "description": "With grep: also show this many lines before and after each match."
        },
        "invert": {
            "type": "boolean",
            "description": "With grep: show the lines that do not match instead."
        }
    })
}

/// A count given as a number or, as some models write it, a string of one.
fn count(args: &Value, key: &str) -> Option<usize> {
    let value = args.get(key)?;
    let n = match value {
        Value::String(s) => s.trim().parse::<u64>().ok()?,
        other => other.as_u64()?,
    };
    usize::try_from(n).ok()
}

impl Filter {
    /// The filter `args` asks for, if any.
    pub fn from_args(args: &Value) -> Result<Option<Self>> {
        let grep = match args.get("grep").and_then(Value::as_str) {
            Some(pattern) if !pattern.is_empty() => {
                let re = Regex::new(pattern)
                    .with_context(|| format!("invalid `grep` regex {pattern:?}"))?;
                Some((re, pattern.to_string()))
            }
            _ => None,
        };
        // Zero lines is nothing anybody asks for: no limit.
        let head = count(args, "head").filter(|&n| n > 0);
        let tail = count(args, "tail").filter(|&n| n > 0);
        if grep.is_none() && head.is_none() && tail.is_none() {
            return Ok(None);
        }
        Ok(Some(Filter {
            invert: grep.is_some() && args.get("invert").and_then(Value::as_bool) == Some(true),
            context: grep
                .as_ref()
                .and_then(|_| count(args, "context"))
                .unwrap_or(0),
            grep,
            head,
            tail,
        }))
    }

    /// The filter in the words of its parameters: `grep "error", last 20`.
    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some((_, pattern)) = &self.grep {
            let mut part = format!("grep {pattern:?}");
            if self.invert {
                part.push_str(" inverted");
            }
            if self.context > 0 {
                part.push_str(&format!(" with {} lines of context", self.context));
            }
            parts.push(part);
        }
        if let Some(n) = self.head {
            parts.push(format!("first {n}"));
        }
        if let Some(n) = self.tail {
            parts.push(format!("last {n}"));
        }
        parts.join(", ")
    }

    /// The lines of `lines` to show, by index, in order.
    fn select(&self, lines: &[&str]) -> Selected {
        let mut keep: Vec<usize> = match &self.grep {
            Some((re, _)) => {
                let hits = (0..lines.len()).filter(|&i| re.is_match(lines[i]) != self.invert);
                hits.collect()
            }
            None => (0..lines.len()).collect(),
        };
        let matched = self.grep.as_ref().map(|_| keep.len());
        // The first and the last of what matched; both, when both are asked.
        let (head, tail) = (self.head.unwrap_or(0), self.tail.unwrap_or(0));
        if (self.head.is_some() || self.tail.is_some()) && head + tail < keep.len() {
            let last = keep.split_off(keep.len() - tail);
            keep.truncate(head);
            keep.extend(last);
        }
        if self.context > 0 {
            let mut around = Vec::new();
            for &i in &keep {
                let from = i.saturating_sub(self.context);
                let to = (i + self.context).min(lines.len().saturating_sub(1));
                // In order and without repeats: `keep` is ascending.
                let start = around.last().map_or(from, |&l: &usize| from.max(l + 1));
                around.extend(start..=to);
            }
            keep = around;
        }
        Selected { keep, matched }
    }
}

/// What a filter kept of some lines.
struct Selected {
    keep: Vec<usize>,
    /// How many lines the pattern selected, when there is a pattern.
    matched: Option<usize>,
}

/// `text` cut to `max` characters, with a mark when something was cut.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

/// How to look at kept output again, for a result that shows part of it.
fn whereabouts(kept: &Result<spill::Kept, String>) -> String {
    match kept {
        Ok(kept) => format!(
            "full output: {} (to see another part without rerunning: bash_output with id \
             \"out-{}\" and tail/head/grep, or read with offset/limit)",
            kept.path, kept.id
        ),
        Err(e) => format!("the full output could not be saved: {e}"),
    }
}

/// What the model is given of `output` — everything a command (or a job,
/// since last asked) printed: all of it when it is short and nothing asks
/// otherwise, its start and its end when it is long, what `filter` selects
/// when there is one. Whenever something is left out, the whole text is
/// saved and the result says where, and how to look at another part.
///
/// `failed`: the command did not end well (a status other than zero, a
/// signal, the timeout). Then a filter that left out the end of the output
/// does not get the last word: the end follows what it selected.
pub fn render(
    root: &Path,
    output: String,
    filter: Option<&Filter>,
    failed: bool,
    budget: usize,
) -> String {
    let Some(filter) = filter else {
        if output.len() <= budget {
            return output;
        }
        let place = whereabouts(&spill::keep(root, &output));
        return spill::cut(&output, budget, &place);
    };
    let place = |output: &str| whereabouts(&spill::keep(root, output));
    shape(output, filter, failed, budget, place)
}

/// What `filter` selects of `output`, which is the output already kept
/// under `id`: another look at it, which saves nothing new.
pub fn again(id: &str, output: String, filter: &Filter, budget: usize) -> String {
    let place = |_: &str| format!("all of it is still out-{id}");
    shape(output, filter, false, budget, place)
}

/// [`render`] with a filter. `place` saves the output, if it has to be, and
/// says where it is: called only when something of it is left out.
fn shape(
    output: String,
    filter: &Filter,
    failed: bool,
    budget: usize,
    place: impl FnOnce(&str) -> String,
) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return "[no output: nothing to filter]\n".to_string();
    }
    let Selected { keep, matched } = filter.select(&lines);
    let all = keep.len() == lines.len();
    // Shown whole after all: the filter left nothing out.
    if all && output.len() <= budget {
        return output;
    }
    let place = place(&output);
    let total = spill::thousands(lines.len());

    let mut out = String::new();
    if matched == Some(0) {
        let (_, pattern) = filter.grep.as_ref().expect("a pattern matched nothing");
        let what = if filter.invert {
            format!("every one of the {total} lines of output matches {pattern:?}")
        } else {
            format!("no line of the {total} lines of output matches {pattern:?}")
        };
        out.push_str(&format!(
            "[filtered: {what}, so nothing is shown — {place}]\n"
        ));
    } else {
        out.push_str(&format!(
            "[filtered: {} of {total} lines shown ({}) — {place}]\n",
            spill::thousands(keep.len()),
            filter.describe()
        ));
        let mut body = String::new();
        for (n, &i) in keep.iter().enumerate() {
            // A gap in what is shown, marked as grep marks it.
            if n > 0 && keep[n - 1] + 1 != i {
                body.push_str("--\n");
            }
            body.push_str(lines[i]);
            body.push('\n');
        }
        if body.len() > budget {
            let narrow = "counted among the filtered lines; narrow the filter to see them";
            body = spill::cut(&body, budget, narrow);
        }
        out.push_str(&body);
    }

    // The end of a failed command's output, if the filter dropped any of it.
    let end = lines.len().saturating_sub(FAILURE_TAIL);
    let shown_from = keep.partition_point(|&i| i < end);
    if failed && keep.len() - shown_from < lines.len() - end {
        out.push_str(&format!(
            "[the command failed, and the filter left out the end of its output. Its last {} \
             lines, unfiltered:]\n",
            lines.len() - end
        ));
        for line in &lines[end..] {
            out.push_str(&clip(line, FAILURE_LINE_CHARS));
            out.push('\n');
        }
    }
    out
}

/// The kept output a `bash_output` id names (`out-ab12cd`, or the file's
/// path as a result gave it), if it is one.
pub fn kept_id(id: &str) -> Option<&str> {
    let name = id.rsplit('/').next().unwrap_or(id);
    let name = name.strip_suffix(".txt").unwrap_or(name);
    let hex = name.strip_prefix("out-").or_else(|| {
        // The path of a kept file, and nothing that merely looks like hex.
        let mut above = id.rsplit('/').skip(1);
        (above.next() == Some(spill::DIR)).then_some(name)
    })?;
    (hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit())).then_some(hex)
}

/// For a command that ends in a pipe into a plain `tail`, `head` or `grep`:
/// a note, for its result, saying what the pipe cost and which parameter
/// does the same without it. Nothing for a pipe that transforms (`| sort |
/// uniq -c`, `| wc -l`, `| jq`, `grep -c`), which is the command's business.
pub fn pipe_note(command: &str) -> Option<String> {
    let words = shell::last_stage(command)?;
    let program = shell::basename(words.first()?);
    let rest = &words[1..];
    let instead = match program {
        "tail" => format!("\"tail\": {}", line_count(rest, false)?),
        "head" => format!("\"head\": {}", line_count(rest, true)?),
        "grep" | "egrep" | "fgrep" | "rg" => grep_parameters(rest)?,
        _ => return None,
    };
    // What `sh` reports for a pipeline is its last stage's status — unless
    // the command line turned `pipefail` on.
    let status = if command.contains("pipefail") {
        String::new()
    } else {
        format!(" The exit status reported is {program}'s, not the command's.")
    };
    Some(format!(
        "[note: this command pipes its output into `{program}`.{status} The pipe hides errors: \
         what {program} dropped is gone, and a failure can look like a success. Next time run \
         the command without the pipe and pass {instead} to bash instead: its own exit status \
         is reported, and the whole output is kept, so another part of it can be looked at \
         without running it again.]"
    ))
}

/// The N of a `tail -n N` / `head -N` that does nothing else, as the
/// parameter takes it. `None` for anything more: following a file, counting
/// bytes, a file to read, `tail -n +N` and `head -n -N` (all but N lines).
fn line_count(args: &[String], head: bool) -> Option<usize> {
    let mut n = 10;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let value = if arg == "-n" || arg == "--lines" {
            args.next()?.as_str()
        } else if let Some(v) = arg.strip_prefix("--lines=") {
            v
        } else if let Some(v) = arg.strip_prefix("-n") {
            v
        } else {
            // `-20`, the old spelling.
            arg.strip_prefix('-')?
        };
        // `tail -n -5` is `tail -n 5`; `head -n -5` is something else.
        let value = match value.strip_prefix('-') {
            Some(v) if !head => v,
            _ => value,
        };
        if !value.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        n = value.parse().ok()?;
    }
    (n > 0).then_some(n)
}

/// The parameters that do what a `grep` with these arguments does to its
/// input, when it only selects lines of it: one pattern, no file, and none
/// of the options that count, test, extract or list.
fn grep_parameters(args: &[String]) -> Option<String> {
    let mut pattern = None;
    let mut extra = String::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if let Some(long) = arg.strip_prefix("--") {
            match long {
                "invert-match" => extra.push_str(", \"invert\": true"),
                "ignore-case" | "extended-regexp" | "line-buffered" | "color=never"
                | "no-messages" => {}
                _ => return None,
            }
        } else if let Some(flags) = arg.strip_prefix('-').filter(|f| !f.is_empty()) {
            for (at, flag) in flags.char_indices() {
                match flag {
                    'v' => extra.push_str(", \"invert\": true"),
                    'i' | 'E' | 'a' | 's' | 'I' => {}
                    'e' if pattern.is_none() => {
                        let glued = &flags[at + 1..];
                        pattern = Some(if glued.is_empty() {
                            args.next()?.clone()
                        } else {
                            glued.to_string()
                        });
                        break;
                    }
                    'A' | 'B' | 'C' => {
                        let glued = &flags[at + 1..];
                        let lines = if glued.is_empty() {
                            args.next()?.as_str()
                        } else {
                            glued
                        };
                        let lines: usize = lines.parse().ok()?;
                        extra.push_str(&format!(", \"context\": {lines}"));
                        break;
                    }
                    // Counting, testing, extracting, listing, recursing, a
                    // limit, another pattern: more than selecting lines.
                    _ => return None,
                }
            }
        } else if pattern.is_none() {
            pattern = Some(arg.clone());
        } else {
            // A file to search: not the pipe that is filtered.
            return None;
        }
    }
    let pattern = Value::from(pattern?);
    Some(format!("\"grep\": {pattern}{extra}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atelier-test-filter-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn filter(args: Value) -> Filter {
        Filter::from_args(&args).unwrap().expect("a filter")
    }

    fn numbered(lines: usize) -> String {
        (1..=lines).map(|i| format!("line {i}\n")).collect()
    }

    /// What `args` selects of `text`, as the lines' text.
    fn selected(args: Value, text: &str) -> Vec<String> {
        let lines: Vec<&str> = text.lines().collect();
        let keep = filter(args).select(&lines).keep;
        keep.into_iter().map(|i| lines[i].to_string()).collect()
    }

    #[test]
    fn no_parameter_is_no_filter_and_a_bad_pattern_is_an_error() {
        assert!(
            Filter::from_args(&json!({"command": "ls"}))
                .unwrap()
                .is_none()
        );
        // Zero lines, an empty pattern, options with nothing to apply to.
        let nothing = json!({"tail": 0, "grep": "", "invert": true, "context": 3});
        assert!(Filter::from_args(&nothing).unwrap().is_none());
        let err = Filter::from_args(&json!({"grep": "("})).err().unwrap();
        assert!(err.to_string().contains("invalid `grep` regex"), "{err}");
        // A count written as a string is still a count.
        assert_eq!(selected(json!({"tail": "2"}), "a\nb\nc\n"), ["b", "c"]);
    }

    #[test]
    fn head_tail_and_grep_select_lines() {
        let text = numbered(30);
        assert_eq!(selected(json!({"tail": 2}), &text), ["line 29", "line 30"]);
        assert_eq!(selected(json!({"head": 2}), &text), ["line 1", "line 2"]);
        assert_eq!(
            selected(json!({"head": 1, "tail": 1}), &text),
            ["line 1", "line 30"]
        );
        // More than there is: everything, once.
        assert_eq!(selected(json!({"head": 20, "tail": 20}), &text).len(), 30);
        assert_eq!(
            selected(json!({"grep": "line 1[0-2]$"}), &text),
            ["line 10", "line 11", "line 12"]
        );
        // The pattern first, then the last of what matched.
        assert_eq!(
            selected(json!({"grep": "5", "tail": 1}), &text),
            ["line 25"]
        );
        assert_eq!(
            selected(json!({"grep": "line [12]", "invert": true}), &text).len(),
            30 - 1 - 10 - 1 - 10
        );
        // Context reaches around each match, without repeating a line.
        assert_eq!(
            selected(json!({"grep": "line (7|9)$", "context": 1}), &text),
            ["line 6", "line 7", "line 8", "line 9", "line 10"]
        );
        assert_eq!(
            selected(json!({"grep": "line (1|30)$", "context": 1}), &text),
            ["line 1", "line 2", "line 29", "line 30"]
        );
    }

    #[test]
    fn a_filtered_result_says_what_it_left_out_and_where_everything_is() {
        let root = tempdir("render");
        let text = numbered(300);
        let out = render(
            &root,
            text.clone(),
            Some(&filter(json!({"tail": 3}))),
            false,
            20_000,
        );
        let (header, body) = out.split_once('\n').unwrap();
        assert!(
            header.starts_with("[filtered: 3 of 300 lines shown (last 3) — full output: "),
            "{header}"
        );
        assert_eq!(body, "line 298\nline 299\nline 300\n");
        // The file holds all of it, and the handle is its name.
        let (_, rest) = header.split_once("full output: ").unwrap();
        let path = rest.split_once(' ').unwrap().0;
        assert!(Path::new(path).starts_with(spill::dir(&root).unwrap()));
        assert_eq!(std::fs::read_to_string(path).unwrap(), text);
        let id = path.rsplit('/').next().unwrap().trim_end_matches(".txt");
        assert!(header.contains(&format!("bash_output with id \"out-{id}\"")));
        assert_eq!(spill::kept(&root, id).unwrap(), text);

        // Gaps are marked.
        let both = filter(json!({"grep": "line (5|200)$", "context": 1}));
        let out = render(&root, text.clone(), Some(&both), false, 20_000);
        assert!(
            out.ends_with("line 4\nline 5\nline 6\n--\nline 199\nline 200\nline 201\n"),
            "{out}"
        );
        assert!(out.contains("(grep \"line (5|200)$\" with 1 lines of context)"));

        // A filter that leaves nothing out is no filter: nothing to keep.
        let kept = spill::dir(&root).unwrap();
        let files = std::fs::read_dir(&kept).unwrap().count();
        let out = render(
            &root,
            "a\nb\n".into(),
            Some(&filter(json!({"tail": 5}))),
            false,
            20_000,
        );
        assert_eq!(out, "a\nb\n");
        assert_eq!(std::fs::read_dir(&kept).unwrap().count(), files);
        // Nor is there anything to say about no output at all.
        let out = render(
            &root,
            String::new(),
            Some(&filter(json!({"tail": 5}))),
            true,
            20_000,
        );
        assert_eq!(out, "[no output: nothing to filter]\n");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_pattern_that_matches_nothing_says_so() {
        let root = tempdir("nomatch");
        let out = render(
            &root,
            numbered(40),
            Some(&filter(json!({"grep": "error"}))),
            false,
            20_000,
        );
        assert!(
            out.starts_with(
                "[filtered: no line of the 40 lines of output matches \"error\", so nothing \
                 is shown — full output: /"
            ),
            "{out}"
        );
        assert_eq!(out.lines().count(), 1);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_filter_does_not_hide_how_a_failed_command_ended() {
        let root = tempdir("failed");
        let mut text = String::from("error: the cause, far above\n");
        text.push_str(&numbered(100));
        text.push_str("FAILED: 1 test\n");
        // The pattern finds the progress lines and not the failure.
        let progress = filter(json!({"grep": "line 5$"}));
        let out = render(&root, text.clone(), Some(&progress), true, 20_000);
        assert!(out.contains("\nline 5\n[the command failed, and the filter left out"));
        assert!(
            out.contains("Its last 20 lines, unfiltered:]\nline 82\n"),
            "{out}"
        );
        assert!(out.ends_with("line 100\nFAILED: 1 test\n"), "{out}");
        // The same filter on a command that succeeded adds nothing.
        let out = render(&root, text.clone(), Some(&progress), false, 20_000);
        assert!(out.ends_with("]\nline 5\n"), "{out}");
        // A tail that already shows the end is enough.
        let out = render(
            &root,
            text.clone(),
            Some(&filter(json!({"tail": 20}))),
            true,
            20_000,
        );
        assert!(!out.contains("the command failed"), "{out}");
        // A shorter one is completed.
        let out = render(&root, text, Some(&filter(json!({"tail": 2}))), true, 20_000);
        assert!(out.contains("the command failed"), "{out}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn long_output_without_a_filter_is_cut_and_names_its_handle() {
        let root = tempdir("long");
        let out = render(&root, numbered(5000), None, false, 2000);
        assert!(out.starts_with("line 1\n") && out.ends_with("line 5000\n"));
        assert!(out.contains(" lines omitted (lines "), "{out}");
        assert!(out.contains("bash_output with id \"out-"), "{out}");
        // Short output is returned as it is, and nothing is written.
        let short = tempdir("short");
        assert_eq!(render(&short, "hi\n".into(), None, true, 2000), "hi\n");
        assert!(!spill::dir(&short).unwrap().exists());
        assert_eq!(std::fs::read_dir(&short).unwrap().count(), 0);
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&short).unwrap();
    }

    #[test]
    fn a_kept_output_is_named_by_its_id_or_its_path() {
        assert_eq!(kept_id("out-ab12cd"), Some("ab12cd"));
        assert_eq!(kept_id("tool-output/ab12cd.txt"), Some("ab12cd"));
        assert_eq!(
            kept_id("/state/atelier/projects/-p/tool-output/AB12CD.txt"),
            Some("AB12CD")
        );
        // As earlier versions named it, in a conversation that was resumed.
        assert_eq!(kept_id(".atelier/tool-output/ab12cd.txt"), Some("ab12cd"));
        for not in [
            "job-1",
            "ab12cd",
            "out-xyz",
            "out-ab12cd0",
            "src/ab12cd.txt",
            "",
        ] {
            assert_eq!(kept_id(not), None, "{not}");
        }
    }

    #[test]
    fn a_pipe_into_a_plain_filter_gets_a_note_naming_the_parameter() {
        let note = pipe_note("cargo test 2>&1 | tail -n 20").unwrap();
        assert!(note.contains("pipes its output into `tail`"), "{note}");
        assert!(
            note.contains("The exit status reported is tail's"),
            "{note}"
        );
        assert!(note.contains("pass \"tail\": 20 to bash"), "{note}");
        for (command, instead) in [
            ("make | tail", "\"tail\": 10"),
            ("make | tail -5", "\"tail\": 5"),
            ("make | tail -n5", "\"tail\": 5"),
            ("make | tail --lines=7", "\"tail\": 7"),
            ("make 2>&1 | head -n 30", "\"head\": 30"),
            ("cd sub && make |& head -3", "\"head\": 3"),
            ("make | /usr/bin/grep -i warn", "\"grep\": \"warn\""),
            (
                "make | grep -v 'ok$'",
                "\"grep\": \"ok$\", \"invert\": true",
            ),
            (
                "make | grep -E -A3 'error|fail'",
                "\"grep\": \"error|fail\", \"context\": 3",
            ),
            (
                "make | grep -C 2 -e error",
                "\"context\": 2, \"grep\": \"error\"",
            ),
            ("a; b | egrep x;", "\"grep\": \"x\""),
        ] {
            let note = pipe_note(command).unwrap_or_else(|| panic!("no note for {command}"));
            for part in instead.split(", ") {
                assert!(note.contains(part), "{command}: {note}");
            }
        }
        // With pipefail the status is the command's; the rest still holds.
        let note = pipe_note("set -o pipefail; make | tail -3").unwrap();
        assert!(!note.contains("exit status reported is"), "{note}");
        assert!(note.contains("The pipe hides errors"), "{note}");
    }

    #[test]
    fn pipes_that_transform_and_filters_that_are_not_last_get_no_note() {
        for command in [
            "make",
            "sort names | uniq -c",
            "make | wc -l",
            "curl -s x | jq .name",
            // Counting, testing, extracting, listing: not selecting lines.
            "make | grep -c warning",
            "make | grep -q warning",
            "make | grep -o 'v[0-9.]*'",
            "ls | grep -m1 x",
            // Following, bytes, "all but": no parameter says that.
            "tail -f log | tail -f",
            "make | tail -c 100",
            "make | tail -n +2",
            "make | head -n -2",
            // Not at the end of the line, not fed by a pipe, not the output.
            "make | tail -3 | sort",
            "make | tail -3; echo done",
            "make && tail -n 20 build.log",
            "make || tail -n 20 build.log",
            "grep -n TODO src/main.rs",
            "make | tail -3 > last.txt",
            "make | grep error build.log",
            "echo $(make | tail -1)",
            // What cannot be followed gets no advice.
            "make | tail -3 # the end",
        ] {
            assert_eq!(pipe_note(command), None, "{command}");
        }
    }
}
