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

use std::sync::{Arc, Mutex, MutexGuard};

use super::Connection;

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
        }
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
