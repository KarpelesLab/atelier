//! Cross-turn deduplication of gathered [`ContextItem`]s.
//!
//! Providers re-inspect the project every turn, so a lot of what they hand
//! back is identical to what was already sent to the model last turn (e.g.
//! git status hasn't changed since the previous message). Re-sending that
//! unchanged context every turn burns tokens for no benefit, so
//! [`dedup_items`] drops items whose body is byte-identical to what was sent
//! for the same title last turn.
//!
//! This module is pure: it doesn't decide when it runs or how the snapshot is
//! persisted across turns — the agent loop owns that (see the module docs in
//! `mod.rs` for the intended call shape).

use super::ContextItem;
use std::collections::HashMap;

/// Splits `items` into what's new-or-changed since `previous`, and a fresh
/// snapshot to pass as `previous` next turn.
///
/// `previous` maps a [`ContextItem::title`] to the body that was rendered for
/// it last turn. An item is kept in `to_render` when its title isn't in
/// `previous` at all, or when its body differs from `previous[title]`; an
/// item whose title and body both match `previous` is considered unchanged
/// and dropped. The relative order of kept items matches their order in
/// `items`.
///
/// `snapshot` always contains an entry for every item in `items` (title ->
/// body), regardless of whether that item was kept or dropped, so the caller
/// can pass it as `previous` on the next call. If `items` contains duplicate
/// titles, the later item wins in `snapshot` (this shouldn't happen with the
/// current providers, which each contribute at most one item with a fixed
/// title).
///
/// An empty `to_render` is a valid, expected result (every item unchanged) —
/// callers should treat it the same as "no context to inject this turn".
pub fn dedup_items(
    previous: &HashMap<String, String>,
    items: Vec<ContextItem>,
) -> (Vec<ContextItem>, HashMap<String, String>) {
    let mut snapshot = HashMap::with_capacity(items.len());
    let mut to_render = Vec::with_capacity(items.len());

    for item in items {
        let changed = match previous.get(&item.title) {
            Some(prev_body) => prev_body != &item.body,
            None => true,
        };
        snapshot.insert(item.title.clone(), item.body.clone());
        if changed {
            to_render.push(item);
        }
    }

    (to_render, snapshot)
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

    #[test]
    fn first_turn_keeps_everything_and_snapshots_all() {
        let previous = HashMap::new();
        let items = vec![item("git status", "clean", 50), item("layout", "src/", 10)];

        let (kept, snapshot) = dedup_items(&previous, items);

        assert_eq!(titles(&kept), vec!["git status", "layout"]);
        assert_eq!(
            snapshot.get("git status").map(String::as_str),
            Some("clean")
        );
        assert_eq!(snapshot.get("layout").map(String::as_str), Some("src/"));
        assert_eq!(snapshot.len(), 2);
    }

    #[test]
    fn second_turn_keeps_only_the_changed_item() {
        let mut previous = HashMap::new();
        previous.insert("git status".to_string(), "clean".to_string());
        previous.insert("layout".to_string(), "src/".to_string());

        let items = vec![
            item("git status", "1 file changed", 50),
            item("layout", "src/", 10),
        ];

        let (kept, snapshot) = dedup_items(&previous, items);

        assert_eq!(titles(&kept), vec!["git status"]);
        assert_eq!(kept[0].body, "1 file changed");
        assert_eq!(
            snapshot.get("git status").map(String::as_str),
            Some("1 file changed")
        );
        assert_eq!(snapshot.get("layout").map(String::as_str), Some("src/"));
    }

    #[test]
    fn unchanged_turn_keeps_nothing_but_still_snapshots_all() {
        let mut previous = HashMap::new();
        previous.insert("git status".to_string(), "clean".to_string());
        previous.insert("layout".to_string(), "src/".to_string());

        let items = vec![item("git status", "clean", 50), item("layout", "src/", 10)];

        let (kept, snapshot) = dedup_items(&previous, items);

        assert!(kept.is_empty());
        assert_eq!(snapshot.len(), 2);
        assert_eq!(
            snapshot.get("git status").map(String::as_str),
            Some("clean")
        );
        assert_eq!(snapshot.get("layout").map(String::as_str), Some("src/"));
    }

    #[test]
    fn removed_item_disappears_from_the_snapshot() {
        let mut previous = HashMap::new();
        previous.insert("git status".to_string(), "clean".to_string());
        previous.insert("layout".to_string(), "src/".to_string());

        // "layout" had nothing to say this turn, so it's absent entirely.
        let items = vec![item("git status", "clean", 50)];

        let (kept, snapshot) = dedup_items(&previous, items);

        assert!(kept.is_empty());
        assert_eq!(snapshot.len(), 1);
        assert!(!snapshot.contains_key("layout"));
    }

    #[test]
    fn order_is_preserved_among_kept_items() {
        let previous = HashMap::new();
        let items = vec![
            item("c", "body c", 1),
            item("a", "body a", 2),
            item("b", "body b", 3),
        ];

        let (kept, _snapshot) = dedup_items(&previous, items);

        assert_eq!(titles(&kept), vec!["c", "a", "b"]);
    }

    #[test]
    fn empty_items_produces_empty_output_and_empty_snapshot() {
        let previous = HashMap::new();
        let (kept, snapshot) = dedup_items(&previous, Vec::new());
        assert!(kept.is_empty());
        assert!(snapshot.is_empty());
    }
}
