//! Command-line flags.
//!
//! atelier has few flags and no parser dependency; this is a single pass
//! over the arguments. Options may come in any order and before or after the
//! prompt words (`atelier -p --output-format json "prompt"`), value options
//! take either `--flag value` or `--flag=value`, and a bare `--` ends option
//! parsing so a prompt can itself start with dashes. An option atelier
//! doesn't know is an error rather than part of the prompt: a script that
//! misspells `--max-turns` should find out, not run unbounded.
//!
//! Subcommands that are programs of their own (`update`, `coordinator`, `doctor`,
//! `trust`, `mcp-serve`) are picked off in `main` before this runs. Those that are
//! runs on a session (`review`, `debug prompt`) are recognized by [`subcommand`]; their
//! arguments are their own, parsed by the module that implements them.

use std::path::Path;

use serde_json::Value;

use crate::headless::{InputFormat, Options, OutputFormat, review};
use crate::permissions::Mode;

/// What a usage error prints after its message, and `--help` on its own.
pub const USAGE: &str = "\
usage: atelier [--claude] [--continue] [--mode MODE]
       atelier --print [options] [--] [prompt...]
       atelier review [--uncommitted | --base BRANCH | --commit SHA] ...
       atelier debug prompt [prompt...]
       atelier doctor [--json] [--offline] [--mcp]
       atelier update | coordinator [status|stop]
       atelier permissions check [--mode MODE] -- <command...>
       atelier stats [--days N] [--project] [--json]
       atelier trust [status|accept|revoke] [path]

  -c, --continue             resume the saved session for this directory
  -p, --print                run one prompt non-interactively and exit
                             (the prompt is read from stdin when not given)
      --claude               use the Claude Code backend
      --mode MODE            how much runs without asking: plan (read-only),
                             ask-edits, auto-edit (default), or yolo
  -V, --version              print the version
  -h, --help                 print this help

with --print:
      --output-format FMT    text (default), json, or stream-json
      --input-format FMT     text (default), or stream-json: one
                             {\"type\":\"user\",\"text\":\"...\"} per line on stdin
      --max-turns N          stop a prompt after N model requests
      --output-schema FILE   JSON Schema the final answer must satisfy
      --goal CONDITION       keep working until CONDITION holds (see /goal)

exit status: 0 success, 1 error, 2 usage, 3 --max-turns reached,
             4 answer failed --output-schema, 6 --goal not met in its bounds

`atelier review --help`, `atelier debug --help` and `atelier doctor --help`
describe those commands.";

/// The parsed command line.
#[derive(Debug)]
pub struct Cli {
    /// `--claude`: the Claude Code backend.
    pub claude: bool,
    /// `--continue`/`-c`: resume the saved session first.
    pub resume: bool,
    /// `--help`/`-h`.
    pub help: bool,
    /// `--mode`: the permission mode to start in.
    pub mode: Option<Mode>,
    /// `--print`/`-p`: run headlessly with these options, instead of the
    /// interactive interface.
    pub print: Option<Options>,
}

/// Parse the arguments after the program name. The error is the message for
/// the user (exit status 2).
pub fn parse(args: &[String]) -> Result<Cli, String> {
    let mut claude = false;
    let mut resume = false;
    let mut help = false;
    let mut mode = None;
    let mut print = false;
    let mut output = None;
    let mut input = None;
    let mut max_turns = None;
    let mut schema = None;
    let mut goal = None;
    let mut words: Vec<&str> = Vec::new();

    let mut it = args.iter().map(String::as_str);
    while let Some(arg) = it.next() {
        if arg == "--" {
            // Everything after is the prompt, dashes and all.
            words.extend(it.by_ref());
            break;
        }
        // A lone `-` is a word, as are arguments that don't start with one.
        if !arg.starts_with('-') || arg == "-" {
            words.push(arg);
            continue;
        }
        // `--flag=value` carries its value; `--flag value` takes the next
        // argument, whatever it looks like.
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v)),
            _ => (arg, None),
        };
        let mut value = || {
            inline
                .or_else(|| it.next())
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag {
            "--mode" => mode = Some(Mode::parse(value()?).map_err(|e| format!("--mode: {e}"))?),
            "--output-format" => output = Some(output_format(value()?)?),
            "--input-format" => input = Some(input_format(value()?)?),
            "--max-turns" => max_turns = Some(positive(flag, value()?)?),
            "--output-schema" => schema = Some(load_schema(Path::new(value()?))?),
            "--goal" => goal = Some(condition(value()?)?),
            _ if inline.is_some() => return Err(format!("{flag} takes no value")),
            "--claude" => claude = true,
            "--continue" | "-c" => resume = true,
            "--print" | "-p" => print = true,
            "--help" | "-h" => help = true,
            _ => return Err(format!("unknown option {arg}")),
        }
    }

    let headless_only = [
        ("--output-format", output.is_some()),
        ("--input-format", input.is_some()),
        ("--max-turns", max_turns.is_some()),
        ("--output-schema", schema.is_some()),
        ("--goal", goal.is_some()),
    ];
    if !print && let Some((flag, _)) = headless_only.iter().find(|(_, set)| *set) {
        return Err(format!("{flag} only applies with --print"));
    }
    let input = input.unwrap_or(InputFormat::Text);
    if print && input == InputFormat::StreamJson && !words.is_empty() {
        return Err("--input-format stream-json reads its prompts from stdin; \
             drop the prompt argument"
            .to_string());
    }

    Ok(Cli {
        claude,
        resume,
        help,
        mode,
        // Without --print, stray words are ignored, as they always were.
        print: print.then(|| Options {
            output: output.unwrap_or(OutputFormat::Text),
            input,
            max_turns,
            schema,
            goal,
            prompt: (!words.is_empty()).then(|| words.join(" ")),
        }),
    })
}

/// A subcommand that runs on a session, set up by `main` as for any run.
#[derive(Debug)]
pub enum Sub {
    /// `atelier review …` (see [`review`]).
    Review(review::Options),
    /// `atelier debug prompt [prompt…]`: print the first request the prompt
    /// (stdin when `None`) would make.
    DebugPrompt(Option<String>),
}

/// `atelier debug --help`, and what follows a usage error there.
const DEBUG_USAGE: &str = "\
usage: atelier debug prompt [--] [prompt...]

Print, as JSON, the first request the prompt would make (the system message,
the messages, the tools) without sending it. The prompt is read from stdin
when not given.";

/// Why a subcommand's arguments end the program before anything runs.
#[derive(Debug)]
pub enum Early {
    /// `--help`: print this and exit 0.
    Help(&'static str),
    /// A usage error: the message, with the subcommand's usage (exit 2).
    Usage(String),
}

/// The subcommand `args` (after the program name) ask for, if their first
/// word names one; `None` means they are flags for [`parse`].
pub fn subcommand(args: &[String]) -> Result<Option<Sub>, Early> {
    let Some((name, rest)) = args.split_first() else {
        return Ok(None);
    };
    // As for the main flags: what follows a bare `--` is not an option.
    let wants_help = || {
        let options = rest.iter().take_while(|a| *a != "--");
        options.into_iter().any(|a| a == "--help" || a == "-h")
    };
    match name.as_str() {
        "review" if wants_help() => Err(Early::Help(review::USAGE)),
        "review" => match review::parse(rest) {
            Ok(opts) => Ok(Some(Sub::Review(opts))),
            Err(msg) => Err(Early::Usage(format!("review: {msg}\n\n{}", review::USAGE))),
        },
        "debug" if wants_help() => Err(Early::Help(DEBUG_USAGE)),
        "debug" => {
            let usage = |msg: String| Early::Usage(format!("debug: {msg}\n\n{DEBUG_USAGE}"));
            let words = match rest.split_first() {
                Some((what, words)) if what == "prompt" => words,
                Some((what, _)) => return Err(usage(format!("unknown subcommand '{what}'"))),
                None => return Err(usage("what to print is missing".to_string())),
            };
            // No options of its own: a leading `--` lets the prompt start
            // with a dash, as with `--print`.
            let words = match words.split_first() {
                Some((dashes, words)) if dashes == "--" => words,
                Some((option, _)) if option.starts_with('-') && option != "-" => {
                    return Err(usage(format!("unknown option {option}")));
                }
                _ => words,
            };
            Ok(Some(Sub::DebugPrompt(
                (!words.is_empty()).then(|| words.join(" ")),
            )))
        }
        _ => Ok(None),
    }
}

fn output_format(v: &str) -> Result<OutputFormat, String> {
    match v {
        "text" => Ok(OutputFormat::Text),
        "json" => Ok(OutputFormat::Json),
        "stream-json" => Ok(OutputFormat::StreamJson),
        _ => Err(format!(
            "--output-format: unknown format '{v}' (text, json, stream-json)"
        )),
    }
}

fn input_format(v: &str) -> Result<InputFormat, String> {
    match v {
        "text" => Ok(InputFormat::Text),
        "stream-json" => Ok(InputFormat::StreamJson),
        _ => Err(format!(
            "--input-format: unknown format '{v}' (text, stream-json)"
        )),
    }
}

/// A `--goal` condition: some text.
fn condition(v: &str) -> Result<String, String> {
    match v.trim() {
        "" => Err("--goal: the condition is empty".to_string()),
        text => Ok(text.to_string()),
    }
}

fn positive(flag: &str, v: &str) -> Result<u32, String> {
    match v.parse::<u32>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("{flag}: expected a positive number, got '{v}'")),
    }
}

/// Read the schema now, so a bad path or file fails before anything runs.
fn load_schema(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("--output-schema: cannot read {}: {e}", path.display()))?;
    let schema: Value = serde_json::from_str(&text)
        .map_err(|e| format!("--output-schema: {} is not JSON: {e}", path.display()))?;
    if !schema.is_object() {
        return Err(format!(
            "--output-schema: {} must hold a JSON object",
            path.display()
        ));
    }
    Ok(schema)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(args: &[&str]) -> Cli {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse(&args).expect("parses")
    }

    fn parse_err(args: &[&str]) -> String {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse(&args).expect_err("is a usage error")
    }

    #[test]
    fn no_arguments_is_interactive() {
        let cli = parse_ok(&[]);
        assert!(!cli.claude && !cli.resume && !cli.help && cli.print.is_none());
    }

    #[test]
    fn mode_flag() {
        assert_eq!(parse_ok(&[]).mode, None);
        assert_eq!(parse_ok(&["--mode", "plan"]).mode, Some(Mode::Plan));
        assert_eq!(
            parse_ok(&["-p", "--mode=ask-edits", "hi"]).mode,
            Some(Mode::AskEdits)
        );
        // A misspelt mode stops the run rather than falling back to another.
        assert!(parse_err(&["--mode", "pln"]).contains("unknown mode 'pln'"));
        assert_eq!(parse_err(&["--mode"]), "--mode needs a value");
    }

    #[test]
    fn print_joins_prompt_words() {
        let opts = parse_ok(&["-p", "hello", "there", "world"]).print.unwrap();
        assert_eq!(opts.prompt.as_deref(), Some("hello there world"));
        assert_eq!(opts.output, OutputFormat::Text);
        assert_eq!(opts.input, InputFormat::Text);
        assert_eq!(opts.max_turns, None);
    }

    #[test]
    fn print_without_words_reads_stdin() {
        assert_eq!(parse_ok(&["--print"]).print.unwrap().prompt, None);
    }

    #[test]
    fn options_may_surround_the_prompt() {
        let cli = parse_ok(&[
            "--continue",
            "-p",
            "--output-format",
            "json",
            "fix it",
            "--max-turns=3",
        ]);
        assert!(cli.resume);
        let opts = cli.print.unwrap();
        assert_eq!(opts.output, OutputFormat::Json);
        assert_eq!(opts.max_turns, Some(3));
        assert_eq!(opts.prompt.as_deref(), Some("fix it"));
        assert_eq!(opts.goal, None);
    }

    #[test]
    fn goal_is_a_headless_option_with_a_condition() {
        let opts = parse_ok(&["-p", "--goal", " the tests pass ", "fix them"])
            .print
            .unwrap();
        assert_eq!(opts.goal.as_deref(), Some("the tests pass"));
        assert_eq!(opts.prompt.as_deref(), Some("fix them"));
        assert_eq!(
            parse_err(&["-p", "--goal="]),
            "--goal: the condition is empty"
        );
        assert_eq!(
            parse_err(&["--goal", "x"]),
            "--goal only applies with --print"
        );
    }

    #[test]
    fn double_dash_ends_options() {
        let opts = parse_ok(&["-p", "--", "--continue", "is", "-a", "flag"])
            .print
            .unwrap();
        assert_eq!(opts.prompt.as_deref(), Some("--continue is -a flag"));
        assert!(!parse_ok(&["-p", "--", "--continue"]).resume);
    }

    #[test]
    fn subcommands_take_their_own_arguments() {
        let sub = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            subcommand(&args)
        };
        assert!(matches!(sub(&[]), Ok(None)));
        assert!(matches!(sub(&["-p", "review", "this"]), Ok(None)));
        assert!(matches!(
            sub(&["review", "--uncommitted"]),
            Ok(Some(Sub::Review(_)))
        ));
        assert!(matches!(sub(&["review", "-h"]), Err(Early::Help(_))));
        // After `--` it is an instruction for the reviewer.
        assert!(matches!(
            sub(&["review", "--", "--help"]),
            Ok(Some(Sub::Review(_)))
        ));
        let Err(Early::Usage(msg)) = sub(&["review", "--frob"]) else {
            panic!("expected a usage error");
        };
        assert!(msg.starts_with("review: unknown option --frob\n\nusage: atelier review"));

        let prompt = |args: &[&str]| match sub(args) {
            Ok(Some(Sub::DebugPrompt(prompt))) => prompt,
            other => panic!("expected a debug prompt, got {other:?}"),
        };
        assert_eq!(prompt(&["debug", "prompt"]), None);
        assert_eq!(
            prompt(&["debug", "prompt", "fix", "it"]).as_deref(),
            Some("fix it")
        );
        assert_eq!(
            prompt(&["debug", "prompt", "--", "--help", "me"]).as_deref(),
            Some("--help me")
        );
        assert!(matches!(sub(&["debug", "--help"]), Err(Early::Help(_))));
        for bad in [
            &["debug"][..],
            &["debug", "tools"],
            &["debug", "prompt", "-x"],
        ] {
            assert!(matches!(sub(bad), Err(Early::Usage(_))), "{bad:?}");
        }
    }

    #[test]
    fn usage_errors() {
        assert_eq!(parse_err(&["-p", "--frob"]), "unknown option --frob");
        assert_eq!(
            parse_err(&["-p", "--max-turns"]),
            "--max-turns needs a value"
        );
        assert!(parse_err(&["-p", "--max-turns", "0"]).contains("positive number"));
        assert!(parse_err(&["-p", "--output-format", "xml"]).contains("unknown format 'xml'"));
        assert_eq!(parse_err(&["--print=yes"]), "--print takes no value");
        assert_eq!(
            parse_err(&["--output-format", "json"]),
            "--output-format only applies with --print"
        );
        assert!(parse_err(&["-p", "--input-format", "stream-json", "hi"]).contains("stdin"));
        assert!(
            parse_err(&["-p", "--output-schema", "/nonexistent/schema.json"])
                .contains("cannot read")
        );
    }
}
