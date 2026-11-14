//! `session/history`: the conversation so far, a page at a time.
//!
//! The items are the session's to say ([`Session::transcript`], which also
//! documents what they hold and what they leave out); this is the request
//! around them: its parameters, and which part of the list answers it.
//!
//! A client showing a conversation wants its end first and the rest as the
//! user scrolls up, so a page is the last `limit` items before `before` —
//! an item's `index`, by default the end. Indexes count from the oldest
//! item the session still has: they hold while the session is idle, and
//! shift when compaction replaces the oldest items with a summary.
//!
//! The session thread answers, since the session is its: the request waits
//! its turn as a job, and is refused while a turn or a command runs —
//! half a turn is not a history, and the client is being sent that turn's
//! events anyway.

use serde_json::{Value, json};

use crate::agent::Session;
use crate::rpc::Failure;

/// Items in a page when the request does not say.
const DEFAULT_LIMIT: usize = 200;

/// The most items a page may hold. An item is at most a tool result's cap
/// (16 KiB of output, see [`crate::events`]) plus its arguments.
const MAX_LIMIT: usize = 1000;

/// Which page was asked for.
pub(super) struct Page {
    limit: usize,
    before: Option<usize>,
}

impl Page {
    /// Read `limit` and `before` out of the request's parameters.
    pub(super) fn parse(params: &Value) -> Result<Page, Failure> {
        let number = |name: &str| match &params[name] {
            Value::Null => Ok(None),
            v => v
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .map(Some)
                .ok_or_else(|| Failure::params(format!("\"{name}\" must be a number"))),
        };
        let limit = match number("limit")? {
            None => DEFAULT_LIMIT,
            Some(0) => return Err(Failure::params("\"limit\" must be at least 1")),
            Some(limit) => limit.min(MAX_LIMIT),
        };
        Ok(Page {
            limit,
            before: number("before")?,
        })
    }

    /// This page of `session`'s conversation.
    pub(super) fn of(&self, session: &Session) -> Value {
        let mut result = self.cut(session.transcript());
        // The Claude backend's conversation is in Claude Code's store.
        result["stored"] = Value::Bool(!session.is_claude());
        if let Some(id) = session.claude_session() {
            result["claude_session"] = Value::from(id);
        }
        result
    }

    /// The last `limit` of `items` before `before`, each with its index.
    fn cut(&self, items: Vec<Value>) -> Value {
        let total = items.len();
        let end = self.before.map_or(total, |before| before.min(total));
        let first = end.saturating_sub(self.limit);
        let page: Vec<Value> = items
            .into_iter()
            .enumerate()
            .skip(first)
            .take(end - first)
            .map(|(index, mut item)| {
                item["index"] = Value::from(index);
                item
            })
            .collect();
        json!({
            "items": page,
            "first": first,
            "total": total,
            "more": first > 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(params: Value) -> Value {
        let items = (0..5)
            .map(|n| json!({ "type": "user", "text": n }))
            .collect();
        Page::parse(&params).ok().unwrap().cut(items)
    }

    fn indexes(result: &Value) -> Vec<u64> {
        let items = result["items"].as_array().unwrap();
        items.iter().map(|i| i["index"].as_u64().unwrap()).collect()
    }

    #[test]
    fn a_page_is_the_end_of_the_list_and_before_walks_back() {
        let all = page(json!({}));
        assert_eq!(indexes(&all), [0, 1, 2, 3, 4]);
        assert_eq!(all["first"], 0);
        assert_eq!(all["total"], 5);
        assert_eq!(all["more"], false);

        let last = page(json!({ "limit": 2 }));
        assert_eq!(indexes(&last), [3, 4]);
        assert_eq!(last["more"], true);
        let earlier = page(json!({ "limit": 2, "before": last["first"] }));
        assert_eq!(indexes(&earlier), [1, 2]);
        let first = page(json!({ "limit": 2, "before": earlier["first"] }));
        assert_eq!(indexes(&first), [0]);
        assert_eq!(first["more"], false);

        // Past either end: nothing, or everything there is.
        assert!(indexes(&page(json!({ "before": 0 }))).is_empty());
        assert_eq!(indexes(&page(json!({ "before": 99 }))).len(), 5);
    }

    #[test]
    fn parameters_are_checked() {
        assert!(Page::parse(&json!({ "limit": 0 })).is_err());
        assert!(Page::parse(&json!({ "limit": "ten" })).is_err());
        assert!(Page::parse(&json!({ "before": -1 })).is_err());
        let huge = Page::parse(&json!({ "limit": 1_000_000 })).ok().unwrap();
        assert_eq!(huge.limit, MAX_LIMIT);
        assert_eq!(Page::parse(&Value::Null).ok().unwrap().limit, DEFAULT_LIMIT);
    }
}
