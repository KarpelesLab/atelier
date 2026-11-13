//! Deferred MCP tools: with many servers, advertise two tools instead of
//! hundreds.
//!
//! Every advertised tool costs its name, description and JSON Schema on
//! every request. A handful of MCP servers can bring a few hundred tools,
//! most of which a given session never calls. So when the specs of all MCP
//! tools together exceed a budget (`[defaults] mcp_tool_budget`), none of
//! them is advertised. They stay in the registry, *hidden*
//! ([`ToolRegistry::set_hidden`]) — callable, not listed — and the model
//! gets two tools in their place:
//!
//! - [`SEARCH`] (`mcp_search`): terms in, the best-matching tools out, each
//!   with its description and full input schema.
//! - [`CALL`] (`mcp_call`): `{tool, arguments}`, which runs one of them.
//!
//! # `mcp_call` is not a tool that runs tools
//!
//! Were `mcp_call` an ordinary tool that looked the real one up and ran it,
//! every check keyed on the tool's name would see `mcp_call`: a
//! `deny = ["mcp__github__delete_*"]` rule would stop matching, "always
//! allow" would allow everything behind it, a `pre_tool` hook matching
//! `mcp__db__.*` would never fire. So it is resolved *before* any of that:
//! the agent loop passes each call through [`resolve`], which turns
//! `mcp_call {tool: T, arguments: A}` into the call `T(A)`. From there on —
//! hooks, permission rules, the approval prompt, what the user is shown,
//! execution — it *is* a call to `T`.
//!
//! [`resolve`] only does this for a tool that is currently hidden; anything
//! else is left alone and reaches [`McpCallTool`] itself, whose one job is
//! to say what went wrong. Should some path ever run `mcp_call` without
//! resolving it first, that is also all that happens: an error, never an
//! unchecked call.
//!
//! # All or nothing
//!
//! Deferral is decided over the MCP tools as a whole, not per server, and
//! [`apply`] re-decides it whenever the set changes (startup, `/mcp add`,
//! `/mcp remove`). Deferring only the largest servers would advertise less
//! in some cases, but the model would then have two ways to reach MCP tools
//! and no rule for which applies; one mode at a time is easier to use well.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::Catalog;
use crate::tools::{Tool, ToolCtx, ToolRegistry, ToolSpec};

/// The search tool's name.
pub const SEARCH: &str = "mcp_search";
/// The call tool's name.
pub const CALL: &str = "mcp_call";

/// Bytes of MCP tool specs advertised before they are deferred instead,
/// unless `[defaults] mcp_tool_budget` says otherwise: about 2k tokens.
pub const DEFAULT_TOOL_BUDGET: usize = 8 * 1024;

/// Results [`SEARCH`] returns unless asked for another number, and the most
/// it returns: each comes with a full schema.
const DEFAULT_RESULTS: usize = 5;
const MAX_RESULTS: usize = 20;

/// What [`apply`] deferred.
pub struct Deferral {
    /// How many tools are now behind [`SEARCH`] and [`CALL`].
    pub tools: usize,
    /// The size of their specs, which exceeded the budget.
    pub bytes: usize,
}

/// Decide, for the MCP tools `catalog` knows of, whether they are advertised
/// one by one or deferred, and put `registry` and `catalog` in that state.
/// `budget` is the most bytes of specs to advertise; `0` never defers.
/// Returns what was deferred, if anything.
pub fn apply(registry: &mut ToolRegistry, catalog: &Catalog, budget: usize) -> Option<Deferral> {
    // Start from nothing deferred: the previous decision may not hold.
    registry.remove_prefix(SEARCH);
    registry.remove_prefix(CALL);
    let specs: Vec<ToolSpec> = catalog
        .tool_names()
        .iter()
        .filter_map(|name| registry.get(name))
        .map(|tool| tool.spec())
        .collect();
    let bytes: usize = specs.iter().map(spec_size).sum();
    if budget == 0 || bytes <= budget {
        registry.set_hidden(Default::default());
        catalog.set_deferred(false);
        return None;
    }
    registry.set_hidden(specs.iter().map(|s| s.name.clone()).collect());
    catalog.set_deferred(true);
    let tools = specs.len();
    registry.register(Box::new(McpSearchTool { index: specs }));
    registry.register(Box::new(McpCallTool));
    Some(Deferral { tools, bytes })
}

/// What advertising `spec` costs, near enough: the bytes of its three parts.
fn spec_size(spec: &ToolSpec) -> usize {
    spec.name.len() + spec.description.len() + spec.parameters.to_string().len()
}

/// If the call `name(arguments)` is an [`CALL`] of a deferred tool, the call
/// it stands for: that tool's name and the JSON of its arguments. `None` for
/// every other call, including an `mcp_call` that names no deferred tool or
/// is malformed — those run as `mcp_call` and get its error.
pub fn resolve(registry: &ToolRegistry, name: &str, arguments: &str) -> Option<(String, String)> {
    if name != CALL {
        return None;
    }
    let args: Value = serde_json::from_str(arguments).ok()?;
    let tool = args.get("tool")?.as_str()?;
    if !registry.is_hidden(tool) {
        return None;
    }
    let inner = match args.get("arguments") {
        None | Some(Value::Null) => json!({}),
        // Some models send nested arguments as a string of JSON.
        Some(Value::String(text)) => serde_json::from_str(text).ok()?,
        Some(other) => other.clone(),
    };
    Some((tool.to_string(), inner.to_string()))
}

/// `mcp_search`: find deferred tools by what they are called and described
/// as. Holds its own copy of their specs, taken when deferral was decided.
struct McpSearchTool {
    index: Vec<ToolSpec>,
}

impl McpSearchTool {
    /// The tools matching `query`, best first.
    ///
    /// The query is split into terms; a tool matches if any term occurs in
    /// its name or description, ignoring case. A term found in the name
    /// counts for more than one found only in the description (the name is
    /// what the server's author chose to call the thing), and a tool
    /// matching more terms ranks above one matching fewer. Ties keep the
    /// tools' registration order, so results are stable.
    fn search(&self, query: &str) -> Vec<&ToolSpec> {
        let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let score = |spec: &ToolSpec| -> usize {
            let name = spec.name.to_lowercase();
            let description = spec.description.to_lowercase();
            terms
                .iter()
                .map(
                    |term| match (name.contains(term), description.contains(term)) {
                        (true, _) => 3,
                        (false, true) => 1,
                        (false, false) => 0,
                    },
                )
                .sum()
        };
        let mut scored: Vec<(usize, &ToolSpec)> = self
            .index
            .iter()
            .map(|spec| (score(spec), spec))
            .filter(|(score, _)| *score > 0)
            .collect();
        // Stable: equal scores stay in registration order.
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored.into_iter().map(|(_, spec)| spec).collect()
    }
}

impl Tool for McpSearchTool {
    fn name(&self) -> &str {
        SEARCH
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SEARCH.to_string(),
            description: format!(
                "Find tools offered by the connected MCP servers. There are {} of them, too \
                 many to list, so they are looked up here: give a few words for what you want \
                 to do (or part of a tool or server name) and get the best-matching tools, \
                 each with its description and input schema. Then run one with `{CALL}`.",
                self.index.len()
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Words to look for in tool names and descriptions, \
                            e.g. \"create issue\". Any of them may match; more matches rank higher.",
                    },
                    "limit": {
                        "type": "integer",
                        "description": format!(
                            "Most tools to return (default {DEFAULT_RESULTS}, at most {MAX_RESULTS})."
                        ),
                    },
                },
                "required": ["query"],
            }),
        }
    }

    /// Reads an index held in memory: nothing to approve.
    // Searching the catalog changes nothing (plan mode lets it through).
    fn mutates(&self, _args: &Value) -> bool {
        false
    }

    fn requires_approval(&self, _args: &Value) -> bool {
        false
    }

    fn call(&self, _ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("");
        if query.trim().is_empty() {
            bail!("missing required argument 'query': a few words for what you want to do");
        }
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_RESULTS, |n| (n as usize).clamp(1, MAX_RESULTS));

        let found = self.search(query);
        if found.is_empty() {
            return Ok(format!(
                "No MCP tool matches {query:?} (searched {} tools by name and description). \
                 Try other words, or a server's name to see what it offers.",
                self.index.len()
            ));
        }
        let shown = found.len().min(limit);
        let mut out = format!(
            "{shown} of {} matching tools, best first. Run one with `{CALL}` \
             {{\"tool\": <name>, \"arguments\": {{…}}}}.",
            found.len()
        );
        for spec in &found[..shown] {
            out.push_str(&format!(
                "\n\n### {}\n{}\ninput schema: {}",
                spec.name, spec.description, spec.parameters
            ));
        }
        if shown < found.len() {
            out.push_str(&format!(
                "\n\n… {} more match; narrow the query or raise `limit`.",
                found.len() - shown
            ));
        }
        Ok(out)
    }
}

/// `mcp_call`: run a deferred tool. The running is done by resolving the
/// call to the real tool before it gets here (see the module docs); this is
/// what is left when that did not happen.
struct McpCallTool;

impl Tool for McpCallTool {
    fn name(&self) -> &str {
        CALL
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: CALL.to_string(),
            description: format!(
                "Run a tool of a connected MCP server found with `{SEARCH}`. It is run as that \
                 tool: the same approval and permission rules apply as if it were called \
                 directly."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "tool": {
                        "type": "string",
                        "description": format!(
                            "The tool's full name as `{SEARCH}` gave it, e.g. `mcp__github__create_issue`."
                        ),
                    },
                    "arguments": {
                        "type": "object",
                        "description": "The tool's arguments, following its input schema.",
                    },
                },
                "required": ["tool"],
            }),
        }
    }

    /// Only ever reports an error (see [`call`](Tool::call)).
    fn requires_approval(&self, _args: &Value) -> bool {
        false
    }

    fn call(&self, _ctx: &mut ToolCtx, args: Value) -> Result<String> {
        match args.get("tool").and_then(Value::as_str) {
            None => bail!("missing required argument 'tool': the name of a tool from `{SEARCH}`"),
            Some(tool) => bail!(
                "`{tool}` is not a tool `{CALL}` can run (or its `arguments` are not a JSON \
                 object). Use the exact name `{SEARCH}` returned."
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::testutil::{FakeRpc, tools_only};
    use crate::mcp::{ServerOptions, assemble, discover};
    use crate::tools::FileState;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    /// A registry holding the tools of a fake `gh` server, recorded in a
    /// catalog as the session would.
    fn connected(tools: Value) -> (ToolRegistry, Catalog) {
        let mut registry = ToolRegistry::new();
        let mut rpc = FakeRpc::new(tools_only(tools));
        let found = discover(&mut rpc, "gh", "2024-11-05").unwrap();
        let connection = assemble(
            Arc::new(Mutex::new(rpc)),
            "gh",
            &ServerOptions::default(),
            found,
        );
        let catalog = Catalog::default();
        catalog.insert("gh", &connection);
        for tool in connection.tools {
            registry.register(tool);
        }
        (registry, catalog)
    }

    fn gh_tools() -> Value {
        json!([
            { "name": "create_issue", "description": "Open a new issue in a repository",
              "inputSchema": { "type": "object", "properties": { "title": { "type": "string" } } } },
            { "name": "list_issues", "description": "List the issues of a repository" },
            { "name": "create_branch", "description": "Create a branch" },
            { "name": "search_code", "description": "Search code; can find an issue reference" },
        ])
    }

    fn advertised(registry: &ToolRegistry) -> Vec<String> {
        registry.specs().into_iter().map(|s| s.name).collect()
    }

    fn run(registry: &ToolRegistry, name: &str, args: Value) -> Result<String> {
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: Path::new("/"),
            fstate: &mut fstate,
            approved: true,
        };
        registry.get(name).expect(name).call(&mut ctx, args)
    }

    #[test]
    fn under_the_budget_tools_are_advertised_one_by_one() {
        let (mut registry, catalog) = connected(gh_tools());
        assert!(apply(&mut registry, &catalog, DEFAULT_TOOL_BUDGET).is_none());
        assert_eq!(advertised(&registry).len(), 4);
        assert!(registry.get(SEARCH).is_none());
        assert!(catalog.deferred().is_empty());
    }

    #[test]
    fn over_the_budget_they_are_hidden_behind_search_and_call() {
        let (mut registry, catalog) = connected(gh_tools());
        let deferral = apply(&mut registry, &catalog, 100).expect("over 100 bytes");
        assert_eq!(deferral.tools, 4);
        assert!(deferral.bytes > 100);
        assert_eq!(advertised(&registry), [SEARCH, CALL]);
        // Still there to be run.
        assert!(registry.get("mcp__gh__create_issue").is_some());
        assert!(registry.is_hidden("mcp__gh__create_issue"));
        assert_eq!(catalog.deferred(), [("gh".to_string(), 4)]);

        // A budget of 0 never defers, and undoes a deferral.
        assert!(apply(&mut registry, &catalog, 0).is_none());
        assert_eq!(advertised(&registry).len(), 4);
        assert!(!registry.is_hidden("mcp__gh__create_issue"));
        assert!(catalog.deferred().is_empty());

        // Deciding twice leaves one pair of tools, not two.
        apply(&mut registry, &catalog, 100);
        apply(&mut registry, &catalog, 100);
        assert_eq!(advertised(&registry), [SEARCH, CALL]);
    }

    #[test]
    fn search_ranks_names_over_descriptions_and_more_terms_over_fewer() {
        let (mut registry, catalog) = connected(gh_tools());
        apply(&mut registry, &catalog, 100);
        let search = |args: Value| run(&registry, SEARCH, args).unwrap();
        let order = |out: &str| -> Vec<String> {
            out.lines()
                .filter_map(|l| l.strip_prefix("### mcp__gh__"))
                .map(str::to_string)
                .collect()
        };

        // Both terms in the name; then one in the name (in registration
        // order); then one in the description only.
        let out = search(json!({ "query": "Create ISSUE" }));
        assert_eq!(
            order(&out),
            [
                "create_issue",
                "list_issues",
                "create_branch",
                "search_code"
            ]
        );
        assert!(out.starts_with("4 of 4 matching tools"), "{out}");
        // Each result carries its description and its whole schema.
        assert!(out.contains("Open a new issue in a repository"), "{out}");
        assert!(out.contains(r#"input schema: {"properties":{"title":{"type":"string"}}"#));

        let out = search(json!({ "query": "issue", "limit": 1 }));
        assert_eq!(order(&out), ["create_issue"]);
        assert!(out.contains("… 2 more match"), "{out}");

        // The server's name finds its tools.
        assert_eq!(order(&search(json!({ "query": "gh" }))).len(), 4);
        assert!(search(json!({ "query": "kubernetes" })).starts_with("No MCP tool matches"));
        assert!(run(&registry, SEARCH, json!({ "query": "  " })).is_err());
    }

    #[test]
    fn a_call_resolves_to_the_deferred_tool_and_nothing_else() {
        let (mut registry, catalog) = connected(gh_tools());
        apply(&mut registry, &catalog, 100);
        let resolve = |arguments: &str| resolve(&registry, CALL, arguments);
        let real = |args: &str| Some(("mcp__gh__create_issue".to_string(), args.to_string()));

        assert_eq!(
            resolve(r#"{"tool":"mcp__gh__create_issue","arguments":{"title":"x"}}"#),
            real(r#"{"title":"x"}"#)
        );
        // Arguments left out, or sent as a string of JSON.
        assert_eq!(resolve(r#"{"tool":"mcp__gh__create_issue"}"#), real("{}"));
        assert_eq!(
            resolve(r#"{"tool":"mcp__gh__create_issue","arguments":"{\"title\":\"x\"}"}"#),
            real(r#"{"title":"x"}"#)
        );

        // Not a deferred tool: left to `mcp_call`, which only errors.
        for arguments in [
            r#"{"tool":"mcp__gh__nope"}"#,
            r#"{"tool":"mcp_call","arguments":{"tool":"mcp__gh__create_issue"}}"#,
            r#"{"tool":"mcp_search","arguments":{"query":"x"}}"#,
            r#"{"tool":"mcp__gh__create_issue","arguments":"not json"}"#,
            r#"{"arguments":{}}"#,
            "not json",
        ] {
            assert_eq!(resolve(arguments), None, "{arguments}");
        }
        // Another tool's call is never touched.
        let direct = super::resolve(&registry, "mcp__gh__create_issue", r#"{"title":"x"}"#);
        assert_eq!(direct, None);

        let err = run(&registry, CALL, json!({ "tool": "mcp__gh__nope" })).unwrap_err();
        assert!(err.to_string().contains("`mcp__gh__nope` is not a tool"));
        assert!(run(&registry, CALL, json!({})).is_err());

        // Once nothing is deferred there is nothing to resolve to.
        apply(&mut registry, &catalog, 0);
        let after = super::resolve(&registry, CALL, r#"{"tool":"mcp__gh__create_issue"}"#);
        assert_eq!(after, None);
    }
}
