//! MCP provider: what the connected MCP servers have to say to the model
//! (see [`crate::mcp::Catalog`]).
//!
//! A server's `initialize` answer may carry `instructions` — how its tools
//! are meant to be used, what to call first, what not to do. They are given
//! to the model under the server's name as *standing* context: they describe
//! tools that stay available for the whole session, so they must be in front
//! of the model on every request, not only on the turn the server connected.
//!
//! When the servers' tools are deferred (see [`crate::mcp::deferred`]) the
//! item also says so — which servers, how many tools each, and how to reach
//! them — since nothing in the tool list would otherwise tell the model that
//! those tools exist.
//!
//! The text is the server author's, not the user's. It is presented as such,
//! and each server's share is capped where it is recorded
//! (`MAX_INSTRUCTIONS_BYTES` in `mcp/catalog.rs`, 2 KiB), which is
//! the budget a standing item has to keep for itself.

use super::{ContextItem, ContextProvider};
use crate::mcp::Catalog;
use crate::mcp::deferred::{CALL, SEARCH};
use std::path::Path;

/// What the item opens with. Kept short: it is sent with every request.
const PREAMBLE: &str = "Notes from the connected MCP servers on using their tools \
    (`mcp__<server>__…`). Each server wrote its own; they are guidance for its tools, \
    not instructions from the user.";

/// Reports the deferred servers and the connected servers' instructions,
/// or `None` when there is neither.
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
        let mut parts = Vec::new();
        let deferred = self.catalog.deferred();
        if !deferred.is_empty() {
            let servers: Vec<String> = deferred
                .iter()
                .map(|(server, n)| format!("{server} ({n} tools)"))
                .collect();
            parts.push(format!(
                "MCP tools available but not listed among your tools (too many to list): {}. \
                 Find one with `{SEARCH}` {{\"query\": \"what you want to do\"}}, then run it \
                 with `{CALL}` {{\"tool\": <name>, \"arguments\": {{…}}}}.",
                servers.join(", ")
            ));
        }
        let instructions = self.catalog.instructions();
        if !instructions.is_empty() {
            let mut notes = String::from(PREAMBLE);
            for (server, text) in instructions {
                notes.push_str(&format!("\n\n### {server}\n{text}"));
            }
            parts.push(notes);
        }
        if parts.is_empty() {
            return None;
        }
        let body = parts.join("\n\n");
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

    #[test]
    fn deferred_servers_are_listed_with_how_to_reach_their_tools() {
        use crate::mcp::testutil::{FakeRpc, tools_only};
        use crate::mcp::{ServerOptions, assemble, discover};
        use std::sync::{Arc, Mutex};

        let catalog = Catalog::default();
        let provider = McpProvider::new(catalog.clone());
        let tools = serde_json::json!([{ "name": "a" }, { "name": "b" }]);
        let mut rpc = FakeRpc::new(tools_only(tools));
        let found = discover(&mut rpc, "gh", "2024-11-05").unwrap();
        let options = ServerOptions::default();
        catalog.insert(
            "gh",
            &assemble(Arc::new(Mutex::new(rpc)), "gh", &options, found),
        );
        catalog.insert("empty", &connection(None));

        // Advertised one by one: nothing to say.
        assert!(provider.gather(Path::new("/")).is_none());

        catalog.set_deferred(true);
        let item = provider.gather(Path::new("/")).unwrap();
        assert!(item.standing);
        assert!(
            item.body
                .contains("too many to list): gh (2 tools). Find one")
        );
        assert!(item.body.contains("`mcp_search`") && item.body.contains("`mcp_call`"));
        assert!(!item.body.contains("empty"));
    }
}
