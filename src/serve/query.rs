//! The queries of the protocol: methods that answer with data where a slash
//! command prints lines — the models, the MCP servers, the other instances,
//! usage, memory, the commands, the background jobs.
//!
//! An interface that wants a model picker or a list of servers could run
//! `/models` or `/mcp` through `command/run` and read the `event/info`
//! lines; these give it the same facts as JSON instead. What they answer is
//! gathered by the session ([`crate::agent::Session`]'s `*_data` methods),
//! from what the slash commands themselves print from.
//!
//! # When they can be asked
//!
//! A query reads the session, which its thread owns and a running turn is
//! using: so a query is a job like `command/run`, carried out between
//! turns, and refused with "turn running" (`-32001`) while a turn or a
//! command runs. They are not published as a snapshot the way
//! `session/status` is: most read the disk and one asks the providers over
//! the network, which is no work to repeat after every turn in case someone
//! asks. A query changes nothing in the session.

use serde_json::Value;

use crate::agent::Session;
use crate::rpc::Failure;

/// The query methods, as `initialize` advertises them
/// (`capabilities.queries`).
pub(super) const METHODS: [&str; 7] = [
    "models/list",
    "mcp/list",
    "peers/list",
    "usage/get",
    "memory/list",
    "commands/list",
    "jobs/list",
];

/// A query, with its parameters checked.
pub(super) enum Query {
    /// `fetch`: ask each provider which models it offers.
    Models {
        fetch: bool,
    },
    Mcp,
    Peers,
    Usage,
    Memory,
    Commands,
    Jobs,
}

impl Query {
    /// The query `method` names, or `None` when it is not one of them.
    /// Fails on parameters of the wrong type.
    pub(super) fn parse(method: &str, params: &Value) -> Result<Option<Self>, Failure> {
        Ok(Some(match method {
            "models/list" => Query::Models {
                fetch: match &params["fetch"] {
                    Value::Null => true,
                    Value::Bool(fetch) => *fetch,
                    _ => return Err(Failure::params("\"fetch\" must be true or false")),
                },
            },
            "mcp/list" => Query::Mcp,
            "peers/list" => Query::Peers,
            "usage/get" => Query::Usage,
            "memory/list" => Query::Memory,
            "commands/list" => Query::Commands,
            "jobs/list" => Query::Jobs,
            _ => return Ok(None),
        }))
    }

    /// The result, read off `session`.
    pub(super) fn answer(&self, session: &Session) -> Value {
        match self {
            Query::Models { fetch } => session.models_data(*fetch),
            Query::Mcp => session.mcp_data(),
            Query::Peers => session.peers_data(),
            Query::Usage => session.usage_data(),
            Query::Memory => session.memory_data(),
            Query::Commands => session.commands_data(),
            Query::Jobs => session.jobs_data(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every advertised method is one `parse` knows, and nothing else is.
    #[test]
    fn the_advertised_methods_are_the_ones_parsed() {
        for method in METHODS {
            assert!(
                matches!(Query::parse(method, &Value::Null), Ok(Some(_))),
                "{method}"
            );
        }
        assert!(matches!(Query::parse("models/get", &Value::Null), Ok(None)));
        let bad = Query::parse("models/list", &json!({"fetch": "no"}));
        assert!(bad.is_err());
    }
}
