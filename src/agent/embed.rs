//! What a session offers a front end that is itself driven by a program —
//! an editor speaking the Agent Client Protocol ([`crate::acp`]) — beyond
//! what the terminal interfaces need.
//!
//! Two things such a client brings or asks for that a person at a prompt
//! does not: MCP servers of its own, for this session only
//! ([`Session::attach_mcp`]), and the list of slash commands as data, to
//! complete them in its own input box ([`Session::extra_commands`]) where
//! the terminal prints `/help`.

use crate::mcp;

use super::{Session, needs_login};

/// A slash command that is not built in: a command file's, or an MCP
/// server's prompt.
pub struct ExtraCommand {
    /// Without the leading slash: `fix`, `mcp:github:review`.
    pub name: String,
    pub description: String,
    /// What to type after it, when it takes arguments.
    pub hint: Option<String>,
}

impl Session {
    /// Connect MCP servers the client supplied for this session, next to
    /// the configured ones. They are not written to `atelier.toml`, and are
    /// not subject to the project's trust: that guards what a *repository*
    /// may start, and these are the choice of the user's own editor.
    /// Returns a status line per server, as
    /// [`connect_configured_mcp`](Self::connect_configured_mcp) does.
    pub fn attach_mcp(&mut self, servers: Vec<mcp::Server>) -> Vec<String> {
        if servers.is_empty() {
            return Vec::new();
        }
        // claude takes its servers from a config file written at launch
        // (`--mcp-config`), built from the settings; these are not in them.
        if self.is_claude() {
            let unused = |s: &mcp::Server| {
                format!(
                    "mcp: '{}' (from the client) is not passed on to the Claude backend",
                    s.name()
                )
            };
            return servers.iter().map(unused).collect();
        }
        let mut status = Vec::new();
        // A name already in use would have its tools collide with the
        // other server's (`mcp__<name>__<tool>`): the first one keeps it.
        let mut fresh: Vec<mcp::Server> = Vec::new();
        for server in servers {
            let name = server.name();
            if self.mcp_name_taken(name) || fresh.iter().any(|f| f.name() == name) {
                status.push(format!(
                    "mcp: '{name}' (from the client) skipped: a server of that name is \
                     already configured"
                ));
            } else {
                fresh.push(server);
            }
        }
        for (server, outcome) in fresh.iter().zip(mcp::connect_all(&fresh)) {
            let name = server.name();
            status.push(match outcome {
                Ok(connection) => {
                    let n = self.mcp_register(name, connection);
                    format!("mcp: connected '{name}' (from the client, {n} tool(s))")
                }
                Err(e) if needs_login(&e) => {
                    format!(
                        "mcp: '{name}' (from the client) needs a sign-in atelier cannot do for it"
                    )
                }
                Err(e) => format!("mcp: failed to connect '{name}' (from the client): {e:#}"),
            });
        }
        self.mcp_defer();
        status
    }

    /// The slash commands this session has beyond the built-in ones: the
    /// command files of the project and the user (see [`crate::commands`]),
    /// then the prompts of the connected MCP servers — those a command file
    /// does not shadow, as in `/help`.
    pub fn extra_commands(&self) -> Vec<ExtraCommand> {
        let global = crate::commands::global_dir();
        let files = crate::commands::list(&self.root, global.as_deref());
        let prompts = self.mcp_catalog.prompt_commands();
        let prompts = prompts
            .into_iter()
            .filter(|(name, _)| !files.iter().any(|f| &f.name == name))
            .map(|(name, prompt)| ExtraCommand {
                name,
                description: prompt.description.clone().unwrap_or_default(),
                hint: Some(prompt.usage()).filter(|usage| !usage.is_empty()),
            })
            .collect::<Vec<_>>();
        let files = files.into_iter().map(|file| ExtraCommand {
            name: file.name,
            description: file.description,
            hint: file.argument_hint,
        });
        files.chain(prompts).collect()
    }
}
