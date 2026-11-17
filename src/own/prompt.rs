//! What a newcomer is told about the session it found live, and the
//! question it is asked: numbered lines answered on the input line, in the
//! manner of the `ask` tool (it is a [`Question`], printed and parsed as
//! one).

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::Value;

use super::{Choice, Kind, Live, Picked};
use crate::tools::ask::{Choice as Option_, Question};

/// What a choice reads as at the prompt.
fn option(choice: Choice) -> Option_ {
    let (label, description) = match choice {
        Choice::TakeOver => (
            "Take over",
            "stop that atelier and everything it is running, and continue its session here",
        ),
        Choice::Cancel => ("Cancel", "leave it alone and exit"),
    };
    Option_ {
        label: label.to_string(),
        description: Some(description.to_string()),
    }
}

/// The choices offered about `live`, in the order they are numbered.
fn choices(_live: &Live) -> Vec<Choice> {
    vec![Choice::TakeOver, Choice::Cancel]
}

/// `n` of something, in words.
fn count(n: u64, one: &str, many: &str) -> String {
    match n {
        1 => format!("1 {one}"),
        n => format!("{n} {many}"),
    }
}

/// How long ago `since` (seconds since the epoch) was.
fn ago(since: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    match now.saturating_sub(since) {
        s if s < 90 => format!("{s} s ago"),
        s if s < 90 * 60 => format!("{} min ago", s / 60),
        s if s < 48 * 3600 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86400),
    }
}

/// Who owns the session, since when, and what it is doing: the lines under
/// "a session is already running in this directory".
pub(super) fn describe(live: &Live) -> Vec<String> {
    let record = &live.record;
    let client = live.status.as_ref().and_then(|s| s["client"].as_str());
    let what = match record.kind {
        Kind::Terminal => "the terminal interface".to_string(),
        Kind::Repl => "the line REPL".to_string(),
        Kind::Print => "a --print --continue run".to_string(),
        Kind::Serve => match client {
            Some(client) => format!("atelier serve, driven by {client}"),
            None => "atelier serve, driven by another program".to_string(),
        },
        Kind::Acp => match client {
            Some(client) => format!("atelier acp, driven by {client}"),
            None => "atelier acp, driven by an editor".to_string(),
        },
    };
    let mut who = format!("pid {} · {what}", record.pid);
    if let Some(tty) = &record.tty {
        who.push_str(&format!(" on {tty}"));
    }
    who.push_str(&format!(" · started {}", ago(record.since)));
    let mut lines = vec![who];
    lines.push(match &live.status {
        Some(status) => format!("now: {}", doing(status)),
        None => "now: not known (it does not answer on its socket)".to_string(),
    });
    if record.kind.driven() {
        lines.push("taking it over ends that program's session with it".to_string());
    }
    lines
}

/// What an owner's `owner/status` says it has going.
fn doing(status: &Value) -> String {
    let mut parts = Vec::new();
    if status["busy"] == true {
        parts.push("a turn in progress".to_string());
    }
    let number = |key: &str| status[key].as_u64().unwrap_or(0);
    if number("jobs") > 0 {
        parts.push(count(number("jobs"), "background job", "background jobs"));
    }
    if number("agents") > 0 {
        parts.push(count(number("agents"), "subagent", "subagents"));
    }
    if parts.is_empty() {
        return "idle".to_string();
    }
    parts.join(" · ")
}

/// How the question put to the user ended.
pub(super) enum Asked {
    Choice(Choice),
    /// The owner ended while the question was up (`gone` said so).
    Gone,
}

/// Ask the user what to do about `live`. `gone` is asked now and then
/// whether there is still anything to ask about.
pub(super) fn ask(live: &Live, gone: &mut dyn FnMut() -> bool) -> Result<Asked> {
    let choices = choices(live);
    let mut intro = vec!["A session is already running in this directory:".to_string()];
    intro.extend(describe(live).into_iter().map(|l| format!("  {l}")));
    let question = Question {
        question: "What do you want to do?".to_string(),
        options: choices.iter().map(|c| option(*c)).collect(),
        multi_select: false,
        allow_other: false,
    };
    #[cfg(tui)]
    let picked = crate::tui::choose(&intro, &question, gone)?;
    #[cfg(not(tui))]
    let picked = crate::agent::choose(&intro, &question, gone);
    Ok(match picked {
        Picked::Option(i) => Asked::Choice(choices[i]),
        Picked::Dismissed => Asked::Choice(Choice::Cancel),
        Picked::Gone => Asked::Gone,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::own::Record;
    use serde_json::json;

    fn live(kind: Kind, status: Option<Value>) -> Live {
        Live {
            record: Record {
                pid: 4021,
                started: "77".into(),
                since: 0,
                kind,
                tty: Some("/dev/pts/3".into()),
                socket: None,
                version: "0".into(),
            },
            status,
        }
    }

    #[test]
    fn the_owner_is_described_by_what_it_is_and_what_it_runs() {
        let busy = json!({"busy": true, "jobs": 1, "agents": 2});
        let lines = describe(&live(Kind::Terminal, Some(busy)));
        assert!(
            lines[0].starts_with("pid 4021 · the terminal interface on /dev/pts/3 · started "),
            "{lines:?}"
        );
        assert_eq!(
            lines[1],
            "now: a turn in progress · 1 background job · 2 subagents"
        );
        assert_eq!(lines.len(), 2);

        let idle = json!({"busy": false, "jobs": 0, "agents": 0, "client": "my-editor 1.2"});
        let lines = describe(&live(Kind::Serve, Some(idle)));
        assert!(
            lines[0].contains("atelier serve, driven by my-editor 1.2"),
            "{lines:?}"
        );
        assert_eq!(lines[1], "now: idle");
        assert!(
            lines[2].contains("ends that program's session"),
            "{lines:?}"
        );

        let lines = describe(&live(Kind::Repl, None));
        assert!(lines[1].contains("does not answer"), "{lines:?}");
    }

    #[test]
    fn a_time_is_told_as_how_long_ago() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(ago(now - 40 * 60), "40 min ago");
        assert_eq!(ago(now - 5 * 3600), "5 h ago");
        assert_eq!(ago(now - 3 * 86400), "3 days ago");
    }
}
