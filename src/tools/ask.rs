//! The `ask` tool: a structured question to the user.
//!
//! The model gives a question and a few options; the interface prints them
//! as numbered lines in the scrollback and the user answers on the ordinary
//! input line — a number, several, a label, or (unless the model forbade it)
//! an answer of their own. No picker, nothing redrawn.
//!
//! # How the question reaches the user
//!
//! A [`Tool`] has no interface to talk through: [`Tool::call`] gets a
//! [`ToolCtx`] and nothing else. So the agent's tool runner recognizes this
//! tool by [`NAME`] and runs [`ask`] with a closure over its `Ui`
//! (`Ui::ask_user`), which is where the interfaces differ. The registered
//! [`AskTool`] supplies the spec, and its `call` is the same [`ask`] with no
//! one to ask — what a caller without an interface gets.
//!
//! Everything an interface needs is here and pure, so they all behave
//! alike: [`Question::lines`] is what to print, [`Question::parse`] turns
//! the typed line into an [`Answer`] or says why it can't.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec};

/// The tool's name: what the model calls, and what the runner intercepts.
pub const NAME: &str = "ask";

/// The fewest and most options a question may offer. Two, or it isn't a
/// choice; six, or it stops being answerable at a glance.
const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 6;

/// One option of a [`Question`].
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub label: String,
    pub description: Option<String>,
}

/// A question as the model asked it, validated.
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub question: String,
    pub options: Vec<Choice>,
    /// Several options may be chosen.
    pub multi_select: bool,
    /// The user may type an answer that is none of the options.
    pub allow_other: bool,
}

/// What the user said.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// The labels of the chosen options, in the order they were offered.
    Labels(Vec<String>),
    /// An answer of the user's own.
    Text(String),
    /// The user dismissed the question (Ctrl-C).
    Declined,
}

impl Question {
    /// Read and check the tool's arguments. Options may also be bare
    /// strings (taken as labels): a model that skips the object wrapper
    /// still asked a clear question.
    pub fn from_args(args: &Value) -> Result<Self> {
        let question = args
            .get("question")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing required argument 'question'"))?
            .to_string();
        let Some(items) = args.get("options").and_then(Value::as_array) else {
            bail!("missing required argument 'options' (an array)");
        };
        if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&items.len()) {
            bail!(
                "'options' must have {MIN_OPTIONS} to {MAX_OPTIONS} items, got {}",
                items.len()
            );
        }
        let mut options: Vec<Choice> = Vec::with_capacity(items.len());
        for item in items {
            let label = item
                .as_str()
                .or_else(|| item.get("label").and_then(Value::as_str))
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .ok_or_else(|| anyhow::anyhow!("every option needs a non-empty 'label'"))?;
            // The answer is reported by label, so two options that read the
            // same would make it ambiguous.
            if options.iter().any(|o| o.label.eq_ignore_ascii_case(label)) {
                bail!("option labels must be distinct; {label:?} appears twice");
            }
            let description = item
                .get("description")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(str::to_string);
            options.push(Choice {
                label: label.to_string(),
                description,
            });
        }
        Ok(Question {
            question,
            options,
            multi_select: flag(args, "multi_select", false),
            allow_other: flag(args, "allow_other", true),
        })
    }

    /// The question as scrollback lines: `? question`, one numbered line
    /// per option, and the [`hint`](Self::hint).
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![format!("? {}", self.question)];
        for (i, o) in self.options.iter().enumerate() {
            out.push(match &o.description {
                Some(d) => format!("  {}. {} — {d}", i + 1, o.label),
                None => format!("  {}. {}", i + 1, o.label),
            });
        }
        out.push(self.hint());
        out
    }

    /// The line saying how to answer; printed under the options, and again
    /// after an answer that couldn't be understood.
    pub fn hint(&self) -> String {
        let how = if self.multi_select {
            "answer with numbers separated by spaces"
        } else {
            "answer with a number"
        };
        if self.allow_other {
            format!("  ({how}, or type your own answer)")
        } else {
            format!("  ({how})")
        }
    }

    /// Understand a typed answer, or say why not (a line fit to print before
    /// the hint). In order:
    ///
    /// 1. a label typed in full (any case) is that option — before numbers,
    ///    so options that *are* numbers ("2", "4", "8") can be typed;
    /// 2. numbers and ranges (`2`, `1 3`, `1,3`, `2-4`) are options by
    ///    position;
    /// 3. comma-separated parts that are each a number, a range, a label or
    ///    an unambiguous start of one (`post`, `red, blue`);
    /// 4. anything else is the user's own answer where that is allowed, and
    ///    an error where it isn't. That includes a number past the last
    ///    option: with free text allowed it is more likely an answer ("16")
    ///    than a slip.
    ///
    /// Picking several options of a single-choice question is always an
    /// error rather than free text: it was meant as a selection.
    pub fn parse(&self, input: &str) -> Result<Answer, String> {
        let input = input.trim();
        if input.is_empty() {
            return Err("no answer given".to_string());
        }
        if let Some(i) = self.exact(input) {
            return Ok(self.labels(vec![i]));
        }
        let picked: Option<Vec<usize>> = if input
            .split([',', ' ', '\t'])
            .filter(|t| !t.is_empty())
            .all(is_numeric)
        {
            // All numbers: spaces separate as well as commas.
            input
                .split([',', ' ', '\t'])
                .filter(|t| !t.is_empty())
                .map(|t| self.by_number(t))
                .collect::<Option<Vec<_>>>()
                .map(|v| v.concat())
        } else {
            // Labels may contain spaces, so only commas separate here.
            input
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(|p| {
                    if is_numeric(p) {
                        self.by_number(p)
                    } else {
                        self.by_label(p).map(|i| vec![i])
                    }
                })
                .collect::<Option<Vec<_>>>()
                .map(|v| v.concat())
        };
        match picked {
            Some(mut picked) if !picked.is_empty() => {
                picked.sort_unstable();
                picked.dedup();
                if picked.len() > 1 && !self.multi_select {
                    return Err("choose one option".to_string());
                }
                Ok(self.labels(picked))
            }
            _ if self.allow_other => Ok(Answer::Text(input.to_string())),
            _ => Err(format!(
                "{input:?} is not one of the {} options",
                self.options.len()
            )),
        }
    }

    fn labels(&self, indices: Vec<usize>) -> Answer {
        Answer::Labels(
            indices
                .into_iter()
                .map(|i| self.options[i].label.clone())
                .collect(),
        )
    }

    /// The option whose label is exactly `text`, ignoring case.
    fn exact(&self, text: &str) -> Option<usize> {
        let text = text.to_lowercase();
        self.options
            .iter()
            .position(|o| o.label.to_lowercase() == text)
    }

    /// The option `text` names: its label, or the start of exactly one.
    fn by_label(&self, text: &str) -> Option<usize> {
        if let Some(i) = self.exact(text) {
            return Some(i);
        }
        let text = text.to_lowercase();
        let mut matches = self
            .options
            .iter()
            .enumerate()
            .filter(|(_, o)| o.label.to_lowercase().starts_with(&text));
        match (matches.next(), matches.next()) {
            (Some((i, _)), None) => Some(i),
            _ => None,
        }
    }

    /// The option indices a number (`2`) or range (`2-4`, either way round)
    /// stands for; `None` if any of it is past the options.
    fn by_number(&self, token: &str) -> Option<Vec<usize>> {
        let index = |s: &str| {
            let n: usize = s.parse().ok()?;
            (1..=self.options.len()).contains(&n).then(|| n - 1)
        };
        match token.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (index(a)?, index(b)?);
                Some((a.min(b)..=a.max(b)).collect())
            }
            None => Some(vec![index(token)?]),
        }
    }
}

/// Whether `token` has the shape of an option number or a range of them.
fn is_numeric(token: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match token.split_once('-') {
        Some((a, b)) => digits(a) && digits(b),
        None => digits(token),
    }
}

fn flag(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

/// The tool result for an answer. Labels and text are quoted as JSON
/// strings and introduced differently, so the model can tell an option it
/// offered from words the user wrote, however those words read.
pub fn result_text(answer: &Answer) -> String {
    let quote = |s: &String| Value::from(s.as_str()).to_string();
    match answer {
        Answer::Labels(labels) if labels.len() == 1 => {
            format!("The user chose the option {}.", quote(&labels[0]))
        }
        Answer::Labels(labels) => format!(
            "The user chose {} options: {}.",
            labels.len(),
            labels.iter().map(quote).collect::<Vec<_>>().join(", ")
        ),
        Answer::Text(text) => format!(
            "The user chose none of the options and answered in their own words: {}",
            quote(text)
        ),
        Answer::Declined => "The user declined to answer. Don't ask again; proceed on your \
             best judgment and state the assumption you made, or stop if you can't."
            .to_string(),
    }
}

/// Run the tool: check the arguments, put the question through `put` (an
/// interface's `ask_user`), and word the answer for the model. `put`
/// returning `None` means there is no one to ask.
pub fn ask(args: &Value, put: &mut dyn FnMut(&Question) -> Option<Answer>) -> Result<String> {
    let question = Question::from_args(args)?;
    match put(&question) {
        Some(answer) => Ok(result_text(&answer)),
        None => bail!(
            "cannot ask: this is a non-interactive session with no one to answer. \
             Decide yourself and state your assumption."
        ),
    }
}

pub struct AskTool;

impl Tool for AskTool {
    fn name(&self) -> &str {
        NAME
    }

    /// It only talks to the user, who answers (or doesn't) themselves.
    fn requires_approval(&self, _args: &Value) -> bool {
        false
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME.to_string(),
            description: "Ask the user a question with a few options to choose from, and wait \
                for the answer. Use it when you need a decision only the user can make (which \
                approach, which of several things they meant) instead of guessing or ending \
                your turn with a question in prose. Don't use it for what you can find out \
                with your other tools. The result says which option(s) the user chose, or \
                quotes what they typed instead."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question, complete enough to answer without scrolling back."
                    },
                    "options": {
                        "type": "array",
                        "minItems": MIN_OPTIONS,
                        "maxItems": MAX_OPTIONS,
                        "description": "The choices, most recommended first.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": {
                                    "type": "string",
                                    "description": "A few words; this is what the answer is reported as."
                                },
                                "description": {
                                    "type": "string",
                                    "description": "One line on what choosing it means (optional)."
                                }
                            },
                            "required": ["label"]
                        }
                    },
                    "multi_select": {
                        "type": "boolean",
                        "description": "Let the user choose several options (default false)."
                    },
                    "allow_other": {
                        "type": "boolean",
                        "description": "Let the user type their own answer instead of choosing (default true)."
                    }
                },
                "required": ["question", "options"]
            }),
        }
    }

    /// Reached only without an interface (the runner answers through one
    /// when it has it): the arguments are still checked, then the model is
    /// told to decide for itself.
    fn call(&self, _ctx: &mut ToolCtx, args: Value) -> Result<String> {
        ask(&args, &mut |_| None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(labels: &[&str], multi_select: bool, allow_other: bool) -> Question {
        Question {
            question: "Which?".into(),
            options: labels
                .iter()
                .map(|l| Choice {
                    label: (*l).to_string(),
                    description: None,
                })
                .collect(),
            multi_select,
            allow_other,
        }
    }

    fn labels(l: &[&str]) -> Result<Answer, String> {
        Ok(Answer::Labels(l.iter().map(|s| s.to_string()).collect()))
    }

    fn text(t: &str) -> Result<Answer, String> {
        Ok(Answer::Text(t.to_string()))
    }

    const DBS: &[&str] = &["Postgres", "SQLite", "Plain files"];

    #[test]
    fn a_number_picks_by_position() {
        let q = question(DBS, false, true);
        assert_eq!(q.parse("1"), labels(&["Postgres"]));
        assert_eq!(q.parse("  2 "), labels(&["SQLite"]));
        assert_eq!(q.parse("3"), labels(&["Plain files"]));
        assert_eq!(q.parse("03"), labels(&["Plain files"]));
    }

    #[test]
    fn several_numbers_need_multi_select() {
        let multi = question(DBS, true, true);
        assert_eq!(multi.parse("1 3"), labels(&["Postgres", "Plain files"]));
        assert_eq!(multi.parse("1,3"), labels(&["Postgres", "Plain files"]));
        assert_eq!(multi.parse("1, 3"), labels(&["Postgres", "Plain files"]));
        assert_eq!(multi.parse(" 3 ,1 "), labels(&["Postgres", "Plain files"]));
        // One is still a valid answer to a multi-select question.
        assert_eq!(multi.parse("2"), labels(&["SQLite"]));

        // A selection of several is never taken for free text.
        let single = question(DBS, false, true);
        assert_eq!(single.parse("1 3"), Err("choose one option".into()));
        assert_eq!(single.parse("1-2"), Err("choose one option".into()));
        assert_eq!(
            single.parse("postgres, sqlite"),
            Err("choose one option".into())
        );
        // Naming the same option twice is one option.
        assert_eq!(single.parse("2 2"), labels(&["SQLite"]));
    }

    #[test]
    fn ranges_expand_in_offer_order_without_duplicates() {
        let q = question(&["a", "b", "c", "d"], true, false);
        assert_eq!(q.parse("2-4"), labels(&["b", "c", "d"]));
        assert_eq!(q.parse("4-2"), labels(&["b", "c", "d"]));
        assert_eq!(q.parse("3 1-2 1"), labels(&["a", "b", "c"]));
        assert_eq!(q.parse("2-2"), labels(&["b"]));
        assert!(q.parse("2-9").is_err());
        assert!(q.parse("0-2").is_err());
    }

    #[test]
    fn labels_and_unambiguous_prefixes_pick_ignoring_case() {
        let q = question(DBS, false, false);
        assert_eq!(q.parse("sqlite"), labels(&["SQLite"]));
        assert_eq!(q.parse("PLAIN FILES"), labels(&["Plain files"]));
        assert_eq!(q.parse("po"), labels(&["Postgres"]));
        assert_eq!(q.parse("s"), labels(&["SQLite"]));
        // "p" starts two labels.
        assert!(q.parse("p").is_err());

        let multi = question(DBS, true, false);
        assert_eq!(
            multi.parse("plain, post"),
            labels(&["Postgres", "Plain files"])
        );
        assert_eq!(multi.parse("sqlite, 1"), labels(&["Postgres", "SQLite"]));
        assert!(multi.parse("sqlite, nope").is_err());
    }

    #[test]
    fn a_full_label_wins_over_a_longer_one_and_over_numbers() {
        // "Yes" is exact even though it also starts "Yes, all of them".
        let q = question(&["Yes", "Yes, all of them", "No"], false, false);
        assert_eq!(q.parse("yes"), labels(&["Yes"]));
        assert_eq!(q.parse("yes, all of them"), labels(&["Yes, all of them"]));
        assert!(q.parse("ye").is_err());

        // Options that are numbers are typed as themselves.
        let n = question(&["2", "4", "8"], false, true);
        assert_eq!(n.parse("4"), labels(&["4"]));
        assert_eq!(n.parse("8"), labels(&["8"]));
        // "1" is no label, so it is the first option.
        assert_eq!(n.parse("1"), labels(&["2"]));
        assert_eq!(n.parse("16"), text("16"));
    }

    #[test]
    fn anything_else_is_free_text_only_where_allowed() {
        let open = question(DBS, false, true);
        assert_eq!(
            open.parse("  MySQL, since ops already runs it "),
            text("MySQL, since ops already runs it")
        );
        assert_eq!(open.parse("9"), text("9"));
        assert_eq!(open.parse("0"), text("0"));
        // Longer than a label is not a prefix of it.
        assert_eq!(
            open.parse("postgres but pooled"),
            text("postgres but pooled")
        );
        assert_eq!(open.parse("p"), text("p"));

        let closed = question(DBS, false, false);
        assert_eq!(
            closed.parse("9"),
            Err("\"9\" is not one of the 3 options".into())
        );
        assert!(closed.parse("0").is_err());
        assert!(closed.parse("MySQL").is_err());
        assert!(closed.parse("-1").is_err());
    }

    #[test]
    fn an_empty_answer_is_never_accepted() {
        for allow_other in [true, false] {
            let q = question(DBS, true, allow_other);
            assert_eq!(q.parse(""), Err("no answer given".into()));
            assert_eq!(q.parse("  \t"), Err("no answer given".into()));
        }
        // Only separators: nothing was picked.
        assert!(question(DBS, true, false).parse(" , ").is_err());
        assert_eq!(question(DBS, true, true).parse(","), text(","));
    }

    #[test]
    fn lines_number_the_options_and_end_with_the_hint() {
        let mut q = question(DBS, false, true);
        q.options[0].description = Some("needs a server".into());
        assert_eq!(
            q.lines(),
            vec![
                "? Which?",
                "  1. Postgres — needs a server",
                "  2. SQLite",
                "  3. Plain files",
                "  (answer with a number, or type your own answer)",
            ]
        );
        assert_eq!(
            question(DBS, true, false).hint(),
            "  (answer with numbers separated by spaces)"
        );
        assert_eq!(
            question(DBS, false, false).hint(),
            "  (answer with a number)"
        );
    }

    #[test]
    fn arguments_are_checked_and_normalized() {
        let q = Question::from_args(&json!({
            "question": " Which database? ",
            "options": [
                {"label": " Postgres ", "description": "needs a server"},
                {"label": "SQLite", "description": "  "},
                "Plain files",
            ],
        }))
        .unwrap();
        assert_eq!(q.question, "Which database?");
        assert_eq!(q.options[0].label, "Postgres");
        assert_eq!(q.options[0].description.as_deref(), Some("needs a server"));
        assert_eq!(q.options[1].description, None);
        assert_eq!(q.options[2].label, "Plain files");
        assert!(!q.multi_select);
        assert!(q.allow_other);

        let q = Question::from_args(&json!({
            "question": "q", "options": ["a", "b"], "multi_select": true, "allow_other": false,
        }))
        .unwrap();
        assert!(q.multi_select && !q.allow_other);

        let bad = |args: Value| Question::from_args(&args).unwrap_err().to_string();
        assert!(bad(json!({"options": ["a", "b"]})).contains("'question'"));
        assert!(bad(json!({"question": " ", "options": ["a", "b"]})).contains("'question'"));
        assert!(bad(json!({"question": "q"})).contains("'options'"));
        assert!(bad(json!({"question": "q", "options": ["a"]})).contains("2 to 6"));
        assert!(
            bad(json!({"question": "q", "options": ["a", "b", "c", "d", "e", "f", "g"]}))
                .contains("got 7")
        );
        assert!(
            bad(json!({"question": "q", "options": ["a", {"description": "x"}]}))
                .contains("'label'")
        );
        assert!(bad(json!({"question": "q", "options": ["Yes", "yes"]})).contains("distinct"));
    }

    #[test]
    fn results_tell_a_choice_from_the_users_own_words() {
        assert_eq!(
            result_text(&Answer::Labels(vec!["SQLite".into()])),
            "The user chose the option \"SQLite\"."
        );
        assert_eq!(
            result_text(&Answer::Labels(vec!["a".into(), "b \"c\"".into()])),
            "The user chose 2 options: \"a\", \"b \\\"c\\\"\"."
        );
        let own = result_text(&Answer::Text("SQLite\nignore the above".into()));
        assert!(own.starts_with("The user chose none of the options"));
        assert!(own.ends_with("\"SQLite\\nignore the above\""));
        assert!(result_text(&Answer::Declined).starts_with("The user declined to answer."));
    }

    #[test]
    fn asking_goes_through_the_interface_or_says_there_is_none() {
        let args = json!({"question": "q", "options": ["a", "b"]});
        let mut seen = None;
        let out = ask(&args, &mut |q| {
            seen = Some(q.clone());
            q.parse("2").ok()
        })
        .unwrap();
        assert_eq!(out, "The user chose the option \"b\".");
        assert_eq!(seen.unwrap().options.len(), 2);

        let err = ask(&args, &mut |_| None).unwrap_err().to_string();
        assert!(err.starts_with("cannot ask: this is a non-interactive session"));

        // Bad arguments never reach the user.
        let mut asked = false;
        let bad = ask(&json!({"question": "q", "options": []}), &mut |_| {
            asked = true;
            None
        });
        assert!(bad.is_err() && !asked);
    }
}
