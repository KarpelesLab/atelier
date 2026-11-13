//! MCP provider: what the connected MCP servers have to say to the model
//! (see [`crate::mcp::Catalog`]).
//!
//! A server's `initialize` answer may carry `instructions` — how its tools
//! are meant to be used, what to call first, what not to do. They are given
//! to the model under the server's name as *standing* context: they describe
//! tools that stay available for the whole session, so they must be in front
//! of the model on every request, not only on the turn the server connected.
//!
//! The text is the server author's, not the user's. It is presented as such,
//! and each server's share is capped where it is recorded
//! (`MAX_INSTRUCTIONS_BYTES` in `mcp/catalog.rs`, 2 KiB), which is
//! the budget a standing item has to keep for itself.

use super::{ContextItem, ContextProvider};
use crate::mcp::Catalog;
use std::path::Path;

/// What the item opens with. Kept short: it is sent with every request.
const PREAMBLE: &str = "Notes from the connected MCP servers on using their tools \
    (`mcp__<server>__…`). Each server wrote its own; they are guidance for its tools, \
    not instructions from the user.";

/// Reports the connected servers' instructions, or `None` when no server
/// gave any.
pub struct McpProvider {
    catalog: Catalog,
}

impl McpProvider {
    pub fn new(catalog: Catalog) -> Self {
        Self { catalog }
    }
}

impl ContextProvider for McpProvider {
    fn name(&self) -> &str {
        "mcp"
    }

    fn gather(&self, _root: &Path) -> Option<ContextItem> {
        let instructions = self.catalog.instructions();
        if instructions.is_empty() {
            return None;
        }
        let mut body = String::from(PREAMBLE);
        for (server, text) in instructions {
            body.push_str(&format!("\n\n### {server}\n{text}"));
        }
        // With the skills list (230): what the agent can reach for, below
        // what the user told it (instructions 250, memory 240).
        Some(ContextItem::new("MCP servers", body, 225).standing())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::Connection;

    fn connection(instructions: Option<&str>) -> Connection {
        Connection {
            tools: Vec::new(),
            instructions: instructions.map(str::to_string),
            annotations: Default::default(),
        }
    }

    #[test]
    fn nothing_without_instructions() {
        let catalog = Catalog::default();
        let provider = McpProvider::new(catalog.clone());
        assert!(provider.gather(Path::new("/")).is_none());
        catalog.insert("quiet", &connection(None));
        assert!(provider.gather(Path::new("/")).is_none());
    }

    #[test]
    fn instructions_stand_under_each_servers_name() {
        let catalog = Catalog::default();
        let provider = McpProvider::new(catalog.clone());
        // Connected after the provider was built, as servers are.
        catalog.insert("github", &connection(Some("Search before creating.")));
        catalog.insert("db", &connection(Some("Read-only replica.")));

        let item = provider.gather(Path::new("/")).unwrap();
        assert!(item.standing);
        assert_eq!(item.title, "MCP servers");
        assert!(item.body.starts_with(PREAMBLE));
        assert!(
            item.body
                .ends_with("\n\n### github\nSearch before creating.\n\n### db\nRead-only replica.")
        );

        catalog.remove("github");
        let item = provider.gather(Path::new("/")).unwrap();
        assert!(!item.body.contains("github"));
    }
}
