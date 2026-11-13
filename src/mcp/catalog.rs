//! What the session knows about its connected MCP servers beyond their
//! tools, shared with the parts of atelier that are not handed the tool
//! registry.
//!
//! Servers are connected by the session, at startup and on `/mcp add` — long
//! after the context providers were built, and behind a registry that only
//! knows `dyn Tool`. A [`Catalog`] is the one thing both sides hold: the
//! session writes each [`Connection`] into it (and takes a removed server
//! out), and the MCP context provider (`context::mcp`) reads it on every
//! turn. It is a cheap handle — clones share one list — kept in connection
//! order, which at startup is the order of the configuration file.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use super::{Annotations, Connection};

/// Most bytes of one server's `instructions` kept for the model. They are
/// sent with every request, and a server's author decides their length: one
/// verbose server must not cost every turn a page.
const MAX_INSTRUCTIONS_BYTES: usize = 2048;

/// The connected servers (see the module docs).
#[derive(Clone, Default)]
pub struct Catalog(Arc<Mutex<Vec<Entry>>>);

struct Entry {
    server: String,
    /// The server's `instructions`, already capped.
    instructions: Option<String>,
    /// What the server said about its tools, by registered name.
    annotations: HashMap<String, Annotations>,
}

impl Catalog {
    /// Record a server that just connected.
    pub fn insert(&self, server: &str, connection: &Connection) {
        let instructions = connection
            .instructions
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| cap(text, MAX_INSTRUCTIONS_BYTES));
        self.entries().push(Entry {
            server: server.to_string(),
            instructions,
            annotations: connection.annotations.clone(),
        });
    }

    /// Forget a server that was removed.
    pub fn remove(&self, server: &str) {
        self.entries().retain(|e| e.server != server);
    }

    /// `(server, instructions)` for every server that gave any.
    pub fn instructions(&self) -> Vec<(String, String)> {
        self.entries()
            .iter()
            .filter_map(|e| Some((e.server.clone(), e.instructions.clone()?)))
            .collect()
    }

    /// What its server said about the tool registered as `tool`, for a
    /// listing: `[read-only]` or `[destructive]`, then its title — whichever
    /// of them there is. `None` for a tool with neither (or not from MCP).
    pub fn describe(&self, tool: &str) -> Option<String> {
        let entries = self.entries();
        let annotations = entries.iter().find_map(|e| e.annotations.get(tool))?;
        let parts: Vec<&str> = annotations
            .marker()
            .into_iter()
            .chain(annotations.title.as_deref())
            .collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    }

    fn entries(&self) -> MutexGuard<'_, Vec<Entry>> {
        // Nothing done under the lock can leave the list half-written, so a
        // panic elsewhere while it was held doesn't make it unusable.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// `text`, cut to at most `max` bytes at a character boundary, saying so.
fn cap(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[… truncated at {max} bytes]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(instructions: Option<&str>) -> Connection {
        Connection {
            tools: Vec::new(),
            instructions: instructions.map(str::to_string),
            annotations: HashMap::new(),
        }
    }

    #[test]
    fn a_tool_is_described_by_its_marker_and_title() {
        let annotated = |title: Option<&str>, read_only, destructive| Annotations {
            title: title.map(str::to_string),
            read_only,
            destructive,
        };
        let mut connection = connection(None);
        connection.annotations = HashMap::from([
            (
                "mcp__s__list".to_string(),
                annotated(Some("List things"), Some(true), None),
            ),
            (
                "mcp__s__wipe".to_string(),
                annotated(None, None, Some(true)),
            ),
            (
                "mcp__s__named".to_string(),
                annotated(Some("Just a title"), None, None),
            ),
            (
                "mcp__s__plain".to_string(),
                annotated(None, Some(false), Some(false)),
            ),
        ]);
        let catalog = Catalog::default();
        catalog.insert("s", &connection);

        let describe = |tool: &str| catalog.describe(tool);
        assert_eq!(
            describe("mcp__s__list").as_deref(),
            Some("[read-only] List things")
        );
        assert_eq!(describe("mcp__s__wipe").as_deref(), Some("[destructive]"));
        assert_eq!(describe("mcp__s__named").as_deref(), Some("Just a title"));
        assert_eq!(describe("mcp__s__plain"), None);
        assert_eq!(describe("bash"), None);

        catalog.remove("s");
        assert_eq!(describe("mcp__s__list"), None);
    }

    #[test]
    fn instructions_are_kept_per_server_in_connection_order() {
        let catalog = Catalog::default();
        catalog.insert("b", &connection(Some("  use b carefully \n")));
        catalog.insert("silent", &connection(None));
        catalog.insert("blank", &connection(Some("  \n")));
        catalog.insert("a", &connection(Some("a's rules")));
        let pairs = |c: &Catalog| c.instructions();
        assert_eq!(
            pairs(&catalog),
            [
                ("b".to_string(), "use b carefully".to_string()),
                ("a".to_string(), "a's rules".to_string()),
            ]
        );

        // A clone is the same catalog; a removed server's text goes with it.
        catalog.clone().remove("b");
        assert_eq!(
            pairs(&catalog),
            [("a".to_string(), "a's rules".to_string())]
        );
    }

    #[test]
    fn long_instructions_are_cut_at_a_character_boundary() {
        let catalog = Catalog::default();
        // 'é' is two bytes: an odd cap would land inside one.
        let long = format!("x{}", "é".repeat(MAX_INSTRUCTIONS_BYTES));
        catalog.insert("s", &connection(Some(&long)));
        let (_, text) = catalog.instructions().remove(0);
        let (kept, note) = text.split_once('\n').unwrap();
        assert!(kept.len() <= MAX_INSTRUCTIONS_BYTES);
        assert!(kept.len() >= MAX_INSTRUCTIONS_BYTES - 1);
        assert!(long.starts_with(kept));
        assert_eq!(note, "[… truncated at 2048 bytes]");
    }
}
