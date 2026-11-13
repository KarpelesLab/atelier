//! `/context`: where the context window goes.
//!
//! A plain-line account of what the next request is made of — the system
//! prompt, each standing context item, the summary, the tool list, the
//! context blocks attached to turns (by item), and the conversation by role —
//! so that both the user and the agent can see what to trim. Sizes are
//! estimates (the same ones [`compact`](super::compact) works with), shown
//! with their share of the compaction threshold; the endpoint's real count
//! for the last request is given next to them when it reported one.

use super::compact::{message_tokens, specs_tokens, thousands};
use super::{SYSTEM_PROMPT, Session};
use crate::context::{ContextItem, estimate_tokens};
use crate::provider::{Message, ToolSpec};

/// One line of the account: a label, its estimated tokens, and its share of
/// `limit`.
fn row(label: &str, tokens: usize, limit: usize) -> String {
    let share = tokens as f64 * 100.0 / limit.max(1) as f64;
    format!("  {label:<36} {:>9}  {share:>5.1}%", thousands(tokens))
}

/// The items of the turns' context blocks, by title: how many blocks carry
/// each and what they cost together, in order of first appearance. A block
/// is made of `## title` sections (see `context::render_budgeted`).
fn context_items(history: &[Message]) -> Vec<(String, usize, usize)> {
    let mut items: Vec<(String, usize, usize)> = Vec::new();
    for block in history.iter().filter_map(|m| m.context.as_deref()) {
        let mut section: Option<(&str, String)> = None;
        let mut close = |section: Option<(&str, String)>| {
            let Some((title, text)) = section else { return };
            let tokens = estimate_tokens(&text);
            match items.iter_mut().find(|(t, _, _)| t == title) {
                Some(item) => {
                    item.1 += 1;
                    item.2 += tokens;
                }
                None => items.push((title.to_string(), 1, tokens)),
            }
        };
        for line in block.lines() {
            if let Some(title) = line.strip_prefix("## ") {
                close(section.take());
                section = Some((title, String::new()));
            }
            if let Some((_, text)) = &mut section {
                text.push_str(line);
                text.push('\n');
            }
        }
        close(section);
    }
    items
}

/// The account of a request made of these parts (see the module docs).
fn report(
    standing: &[ContextItem],
    summary: Option<&str>,
    specs: &[ToolSpec],
    history: &[Message],
    limit: usize,
    last_usage: u32,
) -> Vec<String> {
    let mut lines = vec![format!(
        "context window — estimated tokens, and their share of the compaction threshold ({})",
        thousands(limit)
    )];
    let mut total = 0;
    let mut add = |lines: &mut Vec<String>, label: &str, tokens: usize| {
        total += tokens;
        lines.push(row(label, tokens, limit));
    };

    add(&mut lines, "system prompt", estimate_tokens(SYSTEM_PROMPT));
    for item in standing {
        let tokens = estimate_tokens(&item.title) + estimate_tokens(&item.body);
        add(&mut lines, &format!("standing: {}", item.title), tokens);
    }
    if let Some(summary) = summary {
        add(
            &mut lines,
            "summary of earlier turns",
            estimate_tokens(summary),
        );
    }
    add(
        &mut lines,
        &format!("tool list ({} tool(s))", specs.len()),
        specs_tokens(specs),
    );

    // A turn's context block is part of its user message; count it apart.
    let context_tokens = |m: &Message| m.context.as_deref().map_or(0, estimate_tokens);
    let blocks = history.iter().filter(|m| m.context.is_some()).count();
    let in_blocks: usize = history.iter().map(context_tokens).sum();
    add(
        &mut lines,
        &format!("turn context ({blocks} block(s))"),
        in_blocks,
    );
    for (title, count, tokens) in context_items(history) {
        let label = match count {
            1 => format!("  {title}"),
            n => format!("  {title} (×{n})"),
        };
        lines.push(row(&label, tokens, limit));
    }

    for (role, label) in [
        ("user", "history: user"),
        ("assistant", "history: assistant"),
        ("tool", "history: tool results"),
    ] {
        let of_role = || history.iter().filter(|m| m.role == role);
        let tokens: usize = of_role()
            .map(|m| message_tokens(m).saturating_sub(context_tokens(m)))
            .sum();
        add(
            &mut lines,
            &format!("{label} ({})", of_role().count()),
            tokens,
        );
    }
    lines.push(row("total", total, limit));

    lines.push(match last_usage {
        0 => "last request: no usage reported by the endpoint (or none since a compaction)".into(),
        n => format!(
            "last request: {} tokens as counted by the endpoint (prompt + completion)",
            thousands(n as usize)
        ),
    });
    if history.is_empty() {
        lines.push("(project context is gathered when you send a message)".into());
    }
    lines
}

impl Session {
    /// The lines `/context` prints.
    pub fn context_report(&self) -> Vec<String> {
        if self.is_claude() {
            let mut lines =
                vec!["Claude backend: claude manages its own context window.".to_string()];
            if self.usage_ctx > 0 {
                lines.push(format!(
                    "last request: {} tokens as reported by claude",
                    thousands(self.usage_ctx as usize)
                ));
            }
            return lines;
        }
        report(
            &self.standing,
            self.summary.as_deref(),
            &self.tools.specs(),
            &self.history,
            self.compact_threshold as usize,
            self.usage_ctx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolCall;

    fn tokens_of(lines: &[String], label: &str) -> usize {
        let line = lines
            .iter()
            .find(|l| l.trim_start().starts_with(label))
            .unwrap_or_else(|| panic!("no line for {label:?} in {lines:#?}"));
        let mut fields = line.split_whitespace().rev();
        assert!(fields.next().unwrap().ends_with('%'), "{line}");
        fields.next().unwrap().replace(',', "").parse().unwrap()
    }

    #[test]
    fn every_part_of_the_request_is_accounted_for() {
        let standing = vec![
            ContextItem::new("project instructions", "r".repeat(400), 250).standing(),
            ContextItem::new("memory", "m".repeat(40), 240).standing(),
        ];
        let specs = vec![ToolSpec {
            name: "read".into(),
            description: "d".repeat(36),
            parameters: serde_json::json!({}),
        }];
        let block_1 = format!(
            "Current project context:\n\n## git status\n{}\n\n## layout\n{}",
            "s".repeat(80),
            "l".repeat(800)
        );
        let block_2 = format!(
            "Current project context:\n\n## git status\n{}\n(unchanged since last shown: layout)",
            "s".repeat(40)
        );
        let history = vec![
            Message::user("u".repeat(40)).with_context(Some(block_1.clone())),
            Message::assistant_tool_calls(
                "",
                vec![ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"a.rs"}"#.into(),
                }],
            ),
            Message::tool_result("c1", "t".repeat(4_000)),
            Message::assistant("a".repeat(400)),
            Message::user("again").with_context(Some(block_2.clone())),
        ];

        let lines = report(
            &standing,
            Some(&"y".repeat(200)),
            &specs,
            &history,
            8_000,
            0,
        );

        assert!(lines[0].contains("compaction threshold (8,000)"));
        assert_eq!(
            tokens_of(&lines, "system prompt"),
            estimate_tokens(SYSTEM_PROMPT)
        );
        assert_eq!(tokens_of(&lines, "standing: project instructions"), 105);
        assert_eq!(tokens_of(&lines, "standing: memory"), 12);
        assert_eq!(tokens_of(&lines, "summary of earlier turns"), 50);
        assert_eq!(tokens_of(&lines, "tool list (1 tool(s))"), 11);
        assert_eq!(
            tokens_of(&lines, "turn context (2 block(s))"),
            estimate_tokens(&block_1) + estimate_tokens(&block_2)
        );
        assert!(lines.iter().any(|l| l.contains("  git status (×2)")));
        assert_eq!(tokens_of(&lines, "layout"), 203);
        // What was typed, plus the block's tags and the per-message framing.
        assert!((28..=33).contains(&tokens_of(&lines, "history: user (2)")));
        assert!((100..=115).contains(&tokens_of(&lines, "history: assistant (2)")));
        assert_eq!(tokens_of(&lines, "history: tool results (1)"), 1_004);

        // The total is the sum of the top-level rows, and has its share.
        let parts: usize = [
            "system prompt",
            "standing: project instructions",
            "standing: memory",
            "summary of earlier turns",
            "tool list",
            "turn context",
            "history: user",
            "history: assistant",
            "history: tool results",
        ]
        .iter()
        .map(|label| tokens_of(&lines, label))
        .sum();
        assert_eq!(tokens_of(&lines, "total"), parts);
        let share = parts as f64 * 100.0 / 8_000.0;
        let total_line = lines.iter().find(|l| l.contains("total")).unwrap();
        assert!(
            total_line.ends_with(&format!("{share:.1}%")),
            "{total_line}"
        );

        assert!(lines.last().unwrap().starts_with("last request: no usage"));
    }

    #[test]
    fn an_empty_session_and_reported_usage() {
        let lines = report(&[], None, &[], &[], 8_000, 7_012);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("turn context (0 block(s))"))
        );
        assert!(lines.iter().any(|l| l.contains("history: user (0)")));
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("last request: 7,012 tokens"))
        );
        assert!(lines.last().unwrap().contains("gathered when you send"));
        assert!(!lines.iter().any(|l| l.contains("summary")));
    }
}
