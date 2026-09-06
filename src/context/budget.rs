//! Token-budgeted rendering of gathered [`ContextItem`]s.
//!
//! Individual providers already keep their own output compact, but nothing
//! stops several of them from being large at once (a big diff *and* a big
//! layout tree, say). [`render_budgeted`] is the safety valve: it sorts items
//! by priority, packs as many as fit under a token budget, and truncates or
//! drops the rest — so a single turn's injected context can never blow past
//! the model's context window no matter what the providers hand back.
//!
//! This module does not decide *when* it runs or *what* budget to use; the
//! agent loop owns that. It only exposes the pure rendering function.

use super::ContextItem;

/// Rough characters-per-token ratio used by [`estimate_tokens`]. This mirrors
/// common tokenizer behavior for English prose/code closely enough for
/// budgeting purposes; it is intentionally not an exact tokenizer.
const CHARS_PER_TOKEN: usize = 4;

/// Below this many characters, a truncated section isn't worth including —
/// it would just be a title and an ellipsis with no useful content.
const MIN_TRUNCATED_BODY_CHARS: usize = 20;

/// The header prefixed to every rendered block, matching the wording the
/// agent loop's own (pre-budgeting) `gather_context` used, so behavior looks
/// the same to the model either way.
const HEADER: &str = "Current project context:\n";

/// The marker appended to a body that was cut short to fit the budget.
const TRUNCATED_MARKER: &str = "\n… [truncated]\n";

/// Estimates how many tokens `text` will cost once sent to the model.
///
/// This is a cheap heuristic, **not** an exact tokenizer: it assumes roughly
/// [`CHARS_PER_TOKEN`] characters per token, which is a reasonable
/// approximation for English text and code but will over- or under-count for
/// other scripts, heavy punctuation, or unusual whitespace. Use it only for
/// budgeting decisions, never for anything that needs an exact count.
pub fn estimate_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    chars.div_ceil(CHARS_PER_TOKEN)
}

/// Renders `items` into a single system-message-ready block, packing as many
/// as fit under `max_tokens` (by [`estimate_tokens`]).
///
/// Items are sorted by `priority` descending (ties keep their relative
/// order) and rendered as titled sections (`## <title>\n<body>`), matching
/// the un-budgeted rendering the agent loop previously used. Items are added
/// while they fit whole; the first item that doesn't fit is either included
/// with a truncated body (if a useful amount of it — at least
/// [`MIN_TRUNCATED_BODY_CHARS`] characters — fits in the remaining budget) or
/// dropped entirely, and every item after it is skipped without inspection
/// (since they are lower priority and the budget is already spent). A
/// trailing line notes how many items were left out.
///
/// Returns `None` if `items` is empty, or if nothing at all could be made to
/// fit (e.g. `max_tokens` is too small even for the header).
pub fn render_budgeted(items: Vec<ContextItem>, max_tokens: usize) -> Option<String> {
    if items.is_empty() {
        return None;
    }

    let mut items = items;
    items.sort_by_key(|it| std::cmp::Reverse(it.priority));

    let header_tokens = estimate_tokens(HEADER);
    if header_tokens >= max_tokens {
        return None;
    }
    let mut remaining = max_tokens - header_tokens;

    let mut body = String::new();
    let mut included = 0usize;

    for item in &items {
        let section = format!("\n## {}\n{}\n", item.title, item.body);
        let section_tokens = estimate_tokens(&section);

        if section_tokens <= remaining {
            body.push_str(&section);
            remaining -= section_tokens;
            included += 1;
            continue;
        }

        // The full item doesn't fit. Try a truncated version of it before
        // giving up — but only if a useful amount of body text fits
        // alongside its own header and the truncation marker.
        let fixed_overhead = format!("\n## {}\n", item.title) + TRUNCATED_MARKER;
        let fixed_overhead_tokens = estimate_tokens(&fixed_overhead);
        if fixed_overhead_tokens < remaining {
            let body_token_budget = remaining - fixed_overhead_tokens;
            let body_char_budget = body_token_budget * CHARS_PER_TOKEN;
            if body_char_budget >= MIN_TRUNCATED_BODY_CHARS {
                let truncated_body: String = item.body.chars().take(body_char_budget).collect();
                let section = format!(
                    "\n## {}\n{}{}",
                    item.title, truncated_body, TRUNCATED_MARKER
                );
                let section_tokens = estimate_tokens(&section);
                if section_tokens <= remaining {
                    body.push_str(&section);
                    included += 1;
                }
            }
        }

        // Whether or not the truncated version made it in, we stop here:
        // every remaining item is lower priority and the budget is spent.
        break;
    }

    let skipped = items.len() - included;
    if skipped > 0 {
        body.push_str(&format!(
            "\n[{skipped} lower-priority context item(s) omitted]\n"
        ));
    }

    if body.is_empty() {
        return None;
    }
    Some(format!("{HEADER}{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, body: &str, priority: u8) -> ContextItem {
        ContextItem::new(title, body, priority)
    }

    #[test]
    fn estimate_tokens_known_string() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        // 100 chars -> 25 tokens exactly.
        let s = "x".repeat(100);
        assert_eq!(estimate_tokens(&s), 25);
    }

    #[test]
    fn empty_items_returns_none() {
        assert!(render_budgeted(Vec::new(), 10_000).is_none());
    }

    #[test]
    fn generous_budget_includes_everything_in_priority_order() {
        let items = vec![
            item("low", "low body", 10),
            item("high", "high body", 200),
            item("mid", "mid body", 100),
        ];
        let out = render_budgeted(items, 10_000).expect("should render");

        assert!(out.starts_with("Current project context:\n"));
        assert!(out.contains("## high\nhigh body"));
        assert!(out.contains("## mid\nmid body"));
        assert!(out.contains("## low\nlow body"));
        assert!(!out.contains("omitted"));
        assert!(!out.contains("truncated"));

        let hi = out.find("high").unwrap();
        let mid = out.find("mid").unwrap();
        let lo = out.find("low").unwrap();
        assert!(hi < mid && mid < lo, "sections should be priority-ordered");
    }

    #[test]
    fn tight_budget_truncates_lowest_priority_item() {
        // A high-priority item that fits whole, plus a low-priority item
        // whose full body doesn't fit but a truncated slice does.
        let long_body = "word ".repeat(200); // ~1000 chars, way over budget alone
        let items = vec![
            item("important", "short", 200),
            item("bulky", &long_body, 10),
        ];

        // Budget: enough for the header + the whole "important" section,
        // plus a little room (but not enough) for the full "bulky" section.
        let header_tokens = estimate_tokens(HEADER);
        let important_tokens = estimate_tokens("\n## important\nshort\n");
        let max_tokens = header_tokens + important_tokens + 30;

        let out = render_budgeted(items, max_tokens).expect("should render something");

        assert!(out.contains("## important\nshort"));
        assert!(out.contains("## bulky"));
        assert!(out.contains("… [truncated]"));
        assert!(
            !out.contains("omitted"),
            "the bulky item was included (truncated), not skipped"
        );

        // The truncated body should be a strict prefix of the original.
        let start = out.find("## bulky\n").unwrap() + "## bulky\n".len();
        let marker = out.find(TRUNCATED_MARKER).unwrap();
        let truncated_slice = &out[start..marker];
        assert!(!truncated_slice.is_empty());
        assert!(long_body.starts_with(truncated_slice));
        assert!(truncated_slice.len() < long_body.len());
    }

    #[test]
    fn tiny_budget_fits_only_top_item_header_and_partial_body() {
        let items = vec![
            item("top", &"y".repeat(500), 200),
            item("second", "second body", 100),
        ];

        // Just enough for the header, "top"'s own header, the truncation
        // marker, and a sliver of body — nowhere near enough for "second".
        let header_tokens = estimate_tokens(HEADER);
        let top_fixed = estimate_tokens("\n## top\n") + estimate_tokens("\n… [truncated]\n");
        let max_tokens = header_tokens + top_fixed + 10;

        let out = render_budgeted(items, max_tokens).expect("should render something");

        assert!(out.contains("## top"));
        assert!(out.contains("… [truncated]"));
        assert!(!out.contains("second body"));
        assert!(out.contains("[1 lower-priority context item(s) omitted]"));
    }

    #[test]
    fn budget_too_small_for_anything_returns_none() {
        let items = vec![item("only", "body", 100)];
        // Smaller than even the outer header.
        assert!(render_budgeted(items, 1).is_none());
    }

    #[test]
    fn item_too_small_to_usefully_truncate_is_dropped_not_truncated() {
        // Budget leaves only a couple of characters of body room for the
        // second item — below MIN_TRUNCATED_BODY_CHARS, so it should be
        // omitted entirely rather than rendered as a near-empty stub.
        let items = vec![
            item("first", "first body here", 200),
            item("second", &"z".repeat(500), 50),
        ];
        let header_tokens = estimate_tokens(HEADER);
        let first_tokens = estimate_tokens("\n## first\nfirst body here\n");
        let second_fixed = estimate_tokens("\n## second\n") + estimate_tokens("\n… [truncated]\n");
        // Room for first item fully, plus the second's fixed overhead plus
        // one token of body budget (4 chars) — under MIN_TRUNCATED_BODY_CHARS.
        let max_tokens = header_tokens + first_tokens + second_fixed + 1;

        let out = render_budgeted(items, max_tokens).expect("should render something");
        assert!(out.contains("## first\nfirst body here"));
        assert!(!out.contains("## second"));
        assert!(out.contains("[1 lower-priority context item(s) omitted]"));
    }
}
