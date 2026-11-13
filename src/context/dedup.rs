//! Cross-turn deduplication of gathered [`ContextItem`]s.
//!
//! Providers re-inspect the project every turn, so a lot of what they hand
//! back is identical to what was already sent to the model (e.g. git status
//! hasn't changed since the previous message). Re-sending that every turn
//! burns tokens for no benefit, so [`dedup_items`] keeps only the items whose
//! body differs from what was last sent under the same title.
//!
//! That is sound because what was sent stays in front of the model: with
//! either backend, a turn's context travels inside that turn's user message,
//! which remains in the transcript. It is also what keeps a request a stable
//! prefix of the next one — an old turn's context is never rewritten, only
//! superseded by a later block that carries what changed. Since the model
//! then has several blocks of different ages before it, the result also names
//! what is [unchanged](Delta::unchanged) (the older copy still holds) and
//! what is [cleared](Delta::cleared) (the older copy no longer applies — the
//! build errors are fixed, the diff is committed), for the caller to say so.
//!
//! This module is pure: it doesn't decide when it runs or how the snapshot is
//! kept across turns — the agent loop owns that. When the transcript loses
//! what was sent (compaction summarizes the turns away), the caller drops its
//! snapshot and everything is sent afresh.

use super::ContextItem;
use std::collections::HashMap;

/// How this turn's items compare with what the model was last sent.
pub struct Delta {
    /// New or changed items, in the order they were gathered: the ones to
    /// send.
    pub changed: Vec<ContextItem>,
    /// Titles of items identical to what was last sent, in gathering order.
    pub unchanged: Vec<String>,
    /// Titles that were sent before and have nothing to say now, sorted.
    pub cleared: Vec<String>,
    /// Title → body of every item gathered this turn, sent or not: what to
    /// pass as `previous` next turn.
    pub snapshot: HashMap<String, String>,
}

/// Compares `items` with `previous`, which maps a [`ContextItem::title`] to
/// the body last sent for it (the [`Delta::snapshot`] of the turn before).
///
/// An item is [changed](Delta::changed) when its title isn't in `previous`
/// or its body differs; otherwise it is [unchanged](Delta::unchanged). A
/// title in `previous` that no item carries any more is
/// [cleared](Delta::cleared). If `items` holds duplicate titles the later
/// one wins in the snapshot (the providers each contribute at most one item
/// under a fixed title, so this shouldn't happen).
///
/// No changed item is a valid, expected result — nothing new to tell the
/// model this turn.
pub fn dedup_items(previous: &HashMap<String, String>, items: Vec<ContextItem>) -> Delta {
    let mut snapshot = HashMap::with_capacity(items.len());
    let mut changed = Vec::with_capacity(items.len());
    let mut unchanged = Vec::new();

    for item in items {
        let same = previous.get(&item.title) == Some(&item.body);
        snapshot.insert(item.title.clone(), item.body.clone());
        if same {
            unchanged.push(item.title);
        } else {
            changed.push(item);
        }
    }

    // Sorted: a map's iteration order must not leak into what is sent.
    let mut cleared: Vec<String> = previous
        .keys()
        .filter(|title| !snapshot.contains_key(*title))
        .cloned()
        .collect();
    cleared.sort();

    Delta {
        changed,
        unchanged,
        cleared,
        snapshot,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, body: &str, priority: u8) -> ContextItem {
        ContextItem::new(title, body, priority)
    }

    fn titles(items: &[ContextItem]) -> Vec<&str> {
        items.iter().map(|it| it.title.as_str()).collect()
    }

    fn previous(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(t, b)| (t.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn first_turn_keeps_everything_and_snapshots_all() {
        let items = vec![item("git status", "clean", 50), item("layout", "src/", 10)];

        let d = dedup_items(&HashMap::new(), items);

        assert_eq!(titles(&d.changed), vec!["git status", "layout"]);
        assert!(d.unchanged.is_empty() && d.cleared.is_empty());
        assert_eq!(
            d.snapshot,
            previous(&[("git status", "clean"), ("layout", "src/")])
        );
    }

    #[test]
    fn second_turn_keeps_only_the_changed_item() {
        let prev = previous(&[("git status", "clean"), ("layout", "src/")]);
        let items = vec![
            item("git status", "1 file changed", 50),
            item("layout", "src/", 10),
        ];

        let d = dedup_items(&prev, items);

        assert_eq!(titles(&d.changed), vec!["git status"]);
        assert_eq!(d.changed[0].body, "1 file changed");
        assert_eq!(d.unchanged, ["layout"]);
        assert!(d.cleared.is_empty());
        assert_eq!(
            d.snapshot,
            previous(&[("git status", "1 file changed"), ("layout", "src/")])
        );
    }

    #[test]
    fn unchanged_turn_keeps_nothing_but_still_snapshots_all() {
        let prev = previous(&[("git status", "clean"), ("layout", "src/")]);
        let items = vec![item("git status", "clean", 50), item("layout", "src/", 10)];

        let d = dedup_items(&prev, items);

        assert!(d.changed.is_empty());
        assert_eq!(d.unchanged, ["git status", "layout"]);
        assert_eq!(d.snapshot, prev);
    }

    #[test]
    fn an_item_with_nothing_left_to_say_is_cleared() {
        let prev = previous(&[
            ("git status", "clean"),
            ("layout", "src/"),
            ("diagnostics", "error[E0308]"),
        ]);

        // "layout" and "diagnostics" are absent entirely this turn.
        let d = dedup_items(&prev, vec![item("git status", "clean", 50)]);

        assert!(d.changed.is_empty());
        assert_eq!(d.cleared, ["diagnostics", "layout"], "sorted by title");
        assert_eq!(d.snapshot, previous(&[("git status", "clean")]));

        // Said once: next turn it is no longer in the snapshot.
        let d = dedup_items(&d.snapshot, vec![item("git status", "clean", 50)]);
        assert!(d.cleared.is_empty());
    }

    #[test]
    fn order_is_preserved_among_kept_items() {
        let items = vec![
            item("c", "body c", 1),
            item("a", "body a", 2),
            item("b", "body b", 3),
        ];

        let d = dedup_items(&HashMap::new(), items);

        assert_eq!(titles(&d.changed), vec!["c", "a", "b"]);
    }

    #[test]
    fn standing_items_are_deduplicated_like_any_other() {
        let prev = previous(&[("rules", "be kind"), ("git status", "clean")]);
        let items = vec![
            item("rules", "be kind", 250).standing(),
            item("git status", "clean", 50),
        ];

        let d = dedup_items(&prev, items);
        assert!(d.changed.is_empty());
        assert_eq!(d.unchanged, ["rules", "git status"]);
    }

    #[test]
    fn empty_items_produces_empty_output_and_empty_snapshot() {
        let d = dedup_items(&HashMap::new(), Vec::new());
        assert!(d.changed.is_empty() && d.unchanged.is_empty() && d.cleared.is_empty());
        assert!(d.snapshot.is_empty());
    }
}
