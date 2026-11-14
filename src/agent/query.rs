//! What the slash commands list, as data.
//!
//! `/models`, `/mcp`, `/peers`, `/usage`, `/memory`, `/help`, `/skills` and
//! `/agents` print lines for a person. A program driving the session (see
//! [`crate::serve`]) wants the same facts with their structure intact — to
//! fill a model picker, a list of servers, a panel of running jobs. Each
//! method here answers one of the protocol's queries with JSON, built from
//! what the command of the same subject prints from: where a command had its
//! facts and its wording in one piece, the facts were lifted out (here, or
//! next to what they describe) and the command now words them.
//!
//! Field names are `snake_case`. Nothing here returns a credential: a
//! provider is described by where its key comes from, an MCP server without
//! its headers.

use std::path::Path;

use serde_json::{Map, Value, json};

use super::{HELP, Session, is_builtin, needs_login};
use crate::config::Role;
use crate::coord::{self, locks::Lock};
use crate::mcp::Annotations;
use crate::settings::McpOptions;

/// Why a configured MCP server is not connected.
pub(super) enum McpProblem {
    /// It answered that it wants a sign-in (`/mcp login`).
    NeedsLogin,
    /// Connecting it failed, and how.
    Failed(String),
}

/// How an MCP server is reached.
enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
    },
    Http {
        url: String,
    },
    /// Supplied by the program driving the session, for this session: how
    /// is that program's to know.
    Client,
}

/// Where an MCP server stands.
enum McpStatus {
    Connected,
    /// `enabled = false`: kept in the file, not connected.
    Disabled,
    /// The Claude backend: the server is claude's to connect, not atelier's.
    Delegated,
    /// The project is not trusted, so the server was not started.
    Untrusted,
    NeedsLogin,
    Failed(String),
    /// Not connected, for no reason on record (before the servers are
    /// connected).
    Disconnected,
}

impl McpStatus {
    fn name(&self) -> &'static str {
        match self {
            McpStatus::Connected => "connected",
            McpStatus::Disabled => "disabled",
            McpStatus::Delegated => "delegated",
            McpStatus::Untrusted => "untrusted",
            McpStatus::NeedsLogin => "needs_login",
            McpStatus::Failed(_) => "failed",
            McpStatus::Disconnected => "disconnected",
        }
    }
}

/// One of a connected server's tools.
struct McpTool {
    /// As registered: `mcp__<server>__<tool>`.
    name: String,
    description: String,
    annotations: Annotations,
    /// Reached through `mcp_search` / `mcp_call` rather than advertised.
    deferred: bool,
}

/// An MCP server of the session: one of `atelier.toml`'s, or one the client
/// attached.
pub(super) struct McpServer {
    name: String,
    transport: Transport,
    enabled: bool,
    /// In `atelier.toml` (rather than attached by the client).
    pub(super) configured: bool,
    status: McpStatus,
    tools: Vec<McpTool>,
}

impl McpServer {
    /// The server's line in `/mcp`.
    pub(super) fn line(&self) -> String {
        let off = if self.enabled { "" } else { " (disabled)" };
        let name = &self.name;
        match &self.transport {
            Transport::Stdio { command, args } if args.is_empty() => {
                format!("{name} → {command}{off}")
            }
            Transport::Stdio { command, args } => {
                format!("{name} → {command} {}{off}", args.join(" "))
            }
            Transport::Http { url } => format!("{name} → {url} (http){off}"),
            Transport::Client => format!("{name} → (from the client){off}"),
        }
    }

    fn to_json(&self) -> Value {
        let mut server = json!({
            "name": self.name,
            "enabled": self.enabled,
            "configured": self.configured,
            "status": self.status.name(),
            "error": match &self.status {
                McpStatus::Failed(why) => Some(why),
                _ => None,
            },
        });
        match &self.transport {
            Transport::Stdio { command, args } => {
                server["transport"] = "stdio".into();
                server["command"] = json!(command);
                server["args"] = json!(args);
            }
            Transport::Http { url } => {
                server["transport"] = "http".into();
                server["url"] = json!(url);
            }
            Transport::Client => server["transport"] = Value::Null,
        }
        let prefix = format!("mcp__{}__", self.name);
        let tool = |t: &McpTool| {
            json!({
                "name": t.name,
                "tool": t.name.strip_prefix(&prefix).unwrap_or(&t.name),
                "description": t.description,
                "title": t.annotations.title,
                "read_only": t.annotations.read_only,
                "destructive": t.annotations.destructive,
                "deferred": t.deferred,
            })
        };
        server["tools"] = self.tools.iter().map(tool).collect();
        server
    }
}

/// `/peers`: the link to the coordinator and what it knows.
pub(super) struct Peers {
    /// One line on the state of the link.
    pub(super) link: String,
    /// The connected instances as the coordinator lists them, or why it
    /// could not be asked.
    pub(super) instances: anyhow::Result<Value>,
    /// The advisory locks, from this instance's copy of the table.
    pub(super) locks: Vec<Lock>,
}

impl Peers {
    pub(super) fn of(coord: &coord::Client) -> Self {
        Peers {
            link: coord.link_status(),
            instances: coord.list(),
            locks: coord.locks(),
        }
    }
}

/// `path` as text.
fn shown(path: &Path) -> String {
    path.display().to_string()
}

impl Session {
    /// Note why the server `name` is not connected, from the error
    /// connecting it gave.
    pub(super) fn mcp_problem(&mut self, name: &str, error: &anyhow::Error) {
        let problem = match needs_login(error) {
            true => McpProblem::NeedsLogin,
            false => McpProblem::Failed(format!("{error:#}")),
        };
        self.mcp_problems.insert(name.to_string(), problem);
    }

    /// The MCP servers: the configured ones (stdio, then HTTP, as `/mcp`
    /// lists them), then those the client attached.
    pub(super) fn mcp_servers(&self) -> Vec<McpServer> {
        let connected = self.mcp_catalog.servers();
        let status = |name: &str, options: &McpOptions| {
            if connected.iter().any(|c| c == name) {
                McpStatus::Connected
            } else if !options.enabled {
                McpStatus::Disabled
            } else if !self.trust.active() {
                McpStatus::Untrusted
            } else if self.is_claude() {
                McpStatus::Delegated
            } else {
                match self.mcp_problems.get(name) {
                    Some(McpProblem::NeedsLogin) => McpStatus::NeedsLogin,
                    Some(McpProblem::Failed(why)) => McpStatus::Failed(why.clone()),
                    None => McpStatus::Disconnected,
                }
            }
        };
        let server = |name: &str, transport, options: Option<&McpOptions>| McpServer {
            name: name.to_string(),
            transport,
            enabled: options.is_none_or(|o| o.enabled),
            configured: options.is_some(),
            status: options.map_or(McpStatus::Connected, |o| status(name, o)),
            tools: self.mcp_tools(name),
        };
        let stdio = self.settings.mcp.iter().map(|m| {
            let transport = Transport::Stdio {
                command: m.command.clone(),
                args: m.args.clone(),
            };
            server(&m.name, transport, Some(&m.options))
        });
        let http = self.settings.mcp_http.iter().map(|m| {
            let transport = Transport::Http { url: m.url.clone() };
            server(&m.name, transport, Some(&m.options))
        });
        let attached = connected
            .iter()
            .filter(|name| !self.mcp_name_taken(name))
            .map(|name| server(name, Transport::Client, None));
        stdio.chain(http).chain(attached).collect()
    }

    /// The tools of the connected server `name`; none when it is not.
    fn mcp_tools(&self, name: &str) -> Vec<McpTool> {
        let tools = self.mcp_catalog.tools_of(name).unwrap_or_default();
        let tool = |(name, annotations): (String, Annotations)| McpTool {
            description: (self.tools.get(&name)).map_or_else(String::new, |t| t.spec().description),
            deferred: self.tools.is_hidden(&name),
            annotations,
            name,
        };
        tools.into_iter().map(tool).collect()
    }

    /// `mcp/list`: the MCP servers, where each stands, and its tools.
    pub fn mcp_data(&self) -> Value {
        let servers: Vec<Value> = self.mcp_servers().iter().map(McpServer::to_json).collect();
        json!({ "servers": servers })
    }

    /// The backend as the protocol names it.
    fn backend_name(&self) -> &'static str {
        if self.is_claude() {
            "claude"
        } else if self.chatgpt.is_some() {
            "chatgpt"
        } else {
            "openai"
        }
    }

    /// `models/list`: the model in use, the providers and what they offer
    /// (asked of each now, unless `fetch` is off), the model per role and
    /// the variants — what `/model`, `/models`, `/providers` and `/effort`
    /// show. With the Claude backend only the model in use: claude picks
    /// and calls its own.
    pub fn models_data(&self, fetch: bool) -> Value {
        let cfg = self.config();
        let model = self.main_model();
        let mut current = json!({
            "label": self.backend_label(),
            "model": model,
            "provider": null,
            "id": null,
            "variant": null,
            "options": {},
        });
        let mut data = json!({
            "backend": self.backend_name(),
            "providers": [],
            "roles": [],
            "variants": [],
        });
        if self.is_claude() {
            data["current"] = current;
            return data;
        }
        current["variant"] = json!(cfg.parse(model).variant);
        if self.chatgpt.is_some() {
            // Its own endpoint: no provider of the configuration serves it.
            let (id, options) = cfg.model_and_options(model);
            current["id"] = json!(id);
            current["options"] = Value::Object(options);
        } else {
            let target = cfg.resolve(model);
            current["provider"] = json!(target.provider.name);
            current["id"] = json!(target.model);
            current["options"] = Value::Object(target.options);
        }
        data["current"] = current;

        let mut offered = match fetch {
            true => crate::provider::models_offered(cfg),
            false => Vec::new(),
        }
        .into_iter();
        let provider = |p: &std::sync::Arc<crate::provider::Provider>| {
            let (models, error) = match offered.next() {
                Some((_, Ok(models))) => (Some(models), None),
                Some((_, Err(e))) => (None, Some(format!("{e:#}"))),
                None => (None, None),
            };
            json!({
                "name": p.name,
                "base_url": p.base_url,
                "key": p.key_description(),
                "default": p.name == cfg.default_provider().name,
                "models": models,
                "error": error,
            })
        };
        data["providers"] = cfg.providers().iter().map(provider).collect();
        let role = |role: Role| {
            let target = cfg.target(role);
            json!({
                "role": role.name(),
                "model": cfg.role_model(role),
                "provider": target.provider.name,
                "id": target.model,
                "options": target.options,
                "own": role == Role::Main || cfg.has_own_model(role),
            })
        };
        data["roles"] = Role::ALL.into_iter().map(role).collect();
        let variant = |(name, options): (&String, &Map<String, Value>)| json!({ "name": name, "options": options });
        data["variants"] = cfg.variants().iter().map(variant).collect();
        data
    }

    /// `peers/list`: the other atelier instances and the advisory locks, as
    /// `/peers` shows them. Without a coordinator (switched off, or a
    /// platform that has none) the lists are empty.
    pub fn peers_data(&self) -> Value {
        let Some(coord) = &self.coord else {
            return json!({
                "enabled": false,
                "name": null,
                "link": null,
                "error": null,
                "instances": [],
                "locks": [],
            });
        };
        let peers = Peers::of(coord);
        let (list, error) = match peers.instances {
            Ok(list) => (list, None),
            Err(e) => (Value::Null, Some(format!("{e:#}"))),
        };
        // What an instance says of itself, by name: nothing else of the
        // coordinator's entry is passed on.
        let instance = |i: &Value| {
            json!({
                "name": i["name"],
                "you": i["you"] == true,
                "status": i["status"],
                "model": i["model"],
                "cwd": i["cwd"],
                "pid": i["pid"],
                "version": i["version"],
                "since": i["since"],
                "topics": i["topics"].as_array().cloned().unwrap_or_default(),
            })
        };
        let instances: Vec<Value> = (list["instances"].as_array())
            .map(|all| all.iter().map(instance).collect())
            .unwrap_or_default();
        let lock = |l: &Lock| {
            json!({
                "path": shown(&l.path),
                "holder": l.holder,
                "yours": l.yours,
                "note": l.note,
                "since": l.since,
            })
        };
        json!({
            "enabled": true,
            "name": coord.name(),
            "link": peers.link,
            "error": error,
            "instances": instances,
            "locks": peers.locks.iter().map(lock).collect::<Vec<Value>>(),
        })
    }

    /// `usage/get`: this session's model requests, tokens and cost — what
    /// `/usage` shows.
    pub fn usage_data(&self) -> Value {
        let (context, output) = self.usage();
        let prices = self.config().prices();
        let mut data = crate::usage::session_data(prices, self.is_claude(), context, output);
        data["backend"] = self.backend_name().into();
        data
    }

    /// `memory/list`: the notes of both scopes and where they are kept, as
    /// `/memory` lists them, and the instruction files the session loads up
    /// front.
    pub fn memory_data(&self) -> Value {
        let store = crate::memory::Store::new(&self.root, crate::memory::global_dir());
        let scope =
            |(scope, dir, memories): (_, std::path::PathBuf, Vec<crate::memory::Memory>)| {
                let memory = |m: &crate::memory::Memory| {
                    json!({
                        "name": m.name,
                        "type": m.kind.as_str(),
                        "description": m.description,
                        "path": shown(&dir.join(format!("{}.md", m.name))),
                    })
                };
                json!({
                    "scope": crate::memory::Scope::to_string(&scope),
                    "dir": shown(&dir),
                    "memories": memories.iter().map(memory).collect::<Vec<Value>>(),
                })
            };
        let scopes: Vec<Value> = store.scopes().into_iter().map(scope).collect();
        let files = crate::context::instruction_files(&self.root);
        let file = |path: &std::path::PathBuf| json!({ "path": shown(path) });
        json!({
            "scopes": scopes,
            "instruction_files": files.iter().map(file).collect::<Vec<Value>>(),
        })
    }

    /// `commands/list`: what can be typed after a slash — the built-in
    /// commands, the command files, the connected MCP servers' prompts (all
    /// as `/help` lists them) — and the skills the agent can load (as
    /// `/skills` does).
    pub fn commands_data(&self) -> Value {
        let builtin = |(usage, description): &(&str, &str)| {
            let name = usage.split_whitespace().next().unwrap_or(usage);
            json!({
                "name": name.trim_start_matches('/'),
                "usage": usage,
                "description": description.lines().collect::<Vec<_>>().join(" "),
            })
        };
        let global = crate::commands::global_dir();
        let shadowed = |name: &str| is_builtin(&format!("/{name}"));
        let files = crate::commands::reachable(&self.root, global.as_deref(), &shadowed);
        let custom = |c: &crate::commands::Command| {
            json!({
                "name": c.name,
                "description": c.description,
                "argument_hint": c.argument_hint,
                "model": c.model,
                "path": shown(&c.path),
            })
        };
        let prompts = self.mcp_catalog.prompt_commands();
        let prompts = prompts
            .iter()
            // A command file of the same name wins, as when it is typed.
            .filter(|(name, _)| !files.iter().any(|f| &f.name == name))
            .map(|(name, prompt)| {
                let server = name.strip_prefix("mcp:").and_then(|n| n.split(':').next());
                json!({
                    "name": name,
                    "server": server,
                    "prompt": prompt.name,
                    "description": prompt.description,
                    "argument_hint": Some(prompt.usage()).filter(|usage| !usage.is_empty()),
                })
            });
        let found = crate::skills::discover(&self.root, &crate::skills::global_dirs());
        let skill = |s: &crate::skills::Skill| {
            json!({
                "name": s.name,
                "description": s.description,
                "path": shown(&s.dir),
                "global": s.global,
            })
        };
        json!({
            "builtin": HELP.iter().map(builtin).collect::<Vec<Value>>(),
            "custom": files.iter().map(custom).collect::<Vec<Value>>(),
            "prompts": prompts.collect::<Vec<Value>>(),
            "skills": found.skills.iter().map(skill).collect::<Vec<Value>>(),
            "skipped_skills": found.invalid,
        })
    }

    /// `jobs/list`: the work running in the background — `bash` jobs (what
    /// `bash_output` lists for the model), the subagents `/agents` shows,
    /// and how many tasks claude has going.
    pub fn jobs_data(&self) -> Value {
        let millis = |d: std::time::Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        let jobs = crate::tools::bash::job_list();
        let job = |j: &crate::tools::bash::JobInfo| {
            json!({
                "id": j.id,
                "command": j.command,
                "state": if j.running { "running" } else { "exited" },
                "status": j.status,
                "exit_code": j.exit_code,
                "runtime_ms": millis(j.runtime),
            })
        };
        let agents = self.background.roster().agents();
        let agent = |a: &super::subagent::AgentInfo| {
            json!({
                "id": a.id,
                "agent": a.agent,
                "description": a.description,
                "state": a.ended.unwrap_or("running"),
                "activity": a.activity,
                "steps": a.steps,
                "runtime_ms": millis(a.runtime),
                "worktree": a.worktree.as_deref().map(shown),
            })
        };
        json!({
            "jobs": jobs.iter().map(job).collect::<Vec<Value>>(),
            "agents": agents.iter().map(agent).collect::<Vec<Value>>(),
            "claude_background": self.background_tasks(),
        })
    }
}
