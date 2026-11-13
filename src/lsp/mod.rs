//! Language servers, for the `symbols` tool: which one serves a file, and
//! the ones this process has running.
//!
//! This is the *navigation* half of LSP only — definition, references,
//! symbols, hover. Diagnostics deliberately do not come from here: they come
//! from the project's own lint and typecheck commands (see
//! `context::diagnostics` for why), and this client does not even declare
//! that it can receive them.
//!
//! # Which server
//!
//! For a file, by its extension ([`choose`]):
//!
//! 1. a `[[lsp]]` entry of the project's `atelier.toml` listing it;
//! 2. else a server [`KNOWN`] for the language, when the project has one of
//!    the language's marker files in its root *and* the server's program is
//!    on `PATH`. Nothing is ever installed.
//!
//! When there is none, the caller is told why in a sentence it can pass on
//! to the model, and falls back to textual search.
//!
//! # Trust
//!
//! A language server runs the project's code: rust-analyzer runs build
//! scripts and proc macros, typescript-language-server loads the
//! `typescript` in `node_modules` and the plugins `tsconfig.json` names.
//! Nothing here checks trust — [`Servers::with`] starts what it is asked to
//! — so the caller must: `symbols` starts a server, configured or detected,
//! only in a project the user has trusted, exactly as the diagnostics
//! checkers are run (see [`crate::trust`], which lists both).
//!
//! # Lifetime
//!
//! A server is started the first time it is needed and kept: indexing a
//! project takes seconds to minutes, and answers are only fast afterwards.
//! [`Servers::global`] is the process's set, shared by every session in it
//! (a subagent asks the server its parent started), keyed by project root
//! and server name. A server that dies is started again, once; after a
//! second death, or when it cannot be started at all, it is given up on for
//! the rest of the process and the reason is repeated to every later call.
//! Each server is its own process group, killed as atelier exits
//! ([`kill_all`]) — the same care the `bash` tool takes of its jobs.

mod client;
mod frame;
pub(crate) mod position;
#[cfg(test)]
pub(crate) mod testutil;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub(crate) use client::{Client, Failure, Transport};

use crate::settings::LspServerConfig;

/// How long a detected server is given to answer a call, readiness
/// included (a `[[lsp]]` entry sets its own with `timeout_secs`).
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// How many times a server is started: the first time, and once more if it
/// dies.
const MAX_STARTS: u32 = 2;

/// A language whose server is found without configuration.
struct Known {
    language: &'static str,
    /// Files in the project root that say the project is in this language.
    markers: &'static [&'static str],
    /// Command lines to try, in order; the first whose program is on
    /// `PATH` is used.
    commands: &'static [&'static str],
    extensions: &'static [&'static str],
}

const KNOWN: &[Known] = &[
    Known {
        language: "Rust",
        markers: &["Cargo.toml"],
        commands: &["rust-analyzer"],
        extensions: &["rs"],
    },
    Known {
        language: "Go",
        markers: &["go.mod"],
        commands: &["gopls"],
        extensions: &["go"],
    },
    Known {
        language: "TypeScript/JavaScript",
        markers: &["tsconfig.json", "package.json"],
        commands: &["typescript-language-server --stdio"],
        extensions: &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"],
    },
    Known {
        language: "Python",
        markers: &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
        ],
        commands: &["pyright-langserver --stdio", "pylsp"],
        extensions: &["py", "pyi"],
    },
    Known {
        language: "C/C++",
        markers: &["compile_commands.json", "build/compile_commands.json"],
        commands: &["clangd"],
        extensions: &["c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx"],
    },
];

impl Known {
    fn applies(&self, root: &Path) -> bool {
        self.markers.iter().any(|m| root.join(m).is_file())
    }

    /// Whether `[[lsp]]` entries cover every extension of this language,
    /// leaving nothing for a detected server to serve.
    fn replaced_by(&self, configured: &[LspServerConfig]) -> bool {
        (self.extensions.iter()).all(|ext| configured.iter().any(|c| c.serves(ext)))
    }
}

/// The program of a command line: its first word.
fn program(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or(command)
}

/// A server to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerSpec {
    /// What it is called in results and messages.
    pub(crate) name: String,
    /// Split like a shell would ([`crate::shlex::split`]) and run without
    /// one, in the project root.
    pub(crate) command: String,
    /// How long one call may wait for it.
    pub(crate) timeout: Duration,
}

impl ServerSpec {
    fn configured(config: &LspServerConfig) -> Self {
        Self {
            name: config.name.clone(),
            command: config.command.clone(),
            timeout: Duration::from_secs(config.timeout_secs.max(1)),
        }
    }

    fn detected(command: &str) -> Self {
        Self {
            name: program(command).to_string(),
            command: command.to_string(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// The `initializationOptions` the server is started with, by its
    /// program's name.
    fn options(&self) -> Value {
        let program = program(&self.command);
        match Path::new(program).file_name().and_then(|n| n.to_str()) {
            // rust-analyzer would otherwise run `cargo check` as it starts
            // and hold the build directory's lock against the agent's own
            // builds — to produce diagnostics nobody here reads.
            Some("rust-analyzer") => json!({ "checkOnSave": false }),
            _ => Value::Null,
        }
    }
}

/// The server for files with extension `ext` in the project at `root`, or
/// why there is none (a clause that completes "no language server for X:
/// …"). `has` says whether a program is on `PATH`.
pub(crate) fn choose(
    root: &Path,
    configured: &[LspServerConfig],
    ext: &str,
    has: &dyn Fn(&str) -> bool,
) -> Result<ServerSpec, String> {
    if let Some(config) = configured.iter().find(|c| c.serves(ext)) {
        return Ok(ServerSpec::configured(config));
    }
    let Some(known) = KNOWN.iter().find(|k| k.extensions.contains(&ext)) else {
        return Err(format!(
            "none is known for .{ext} files, and no [[lsp]] entry of atelier.toml lists them"
        ));
    };
    if !known.applies(root) {
        return Err(format!(
            "the project root has no {}",
            known.markers.join(" or ")
        ));
    }
    match known.commands.iter().find(|c| has(program(c))) {
        Some(command) => Ok(ServerSpec::detected(command)),
        None => {
            let programs: Vec<&str> = known.commands.iter().map(|c| program(c)).collect();
            Err(format!("{} is not on PATH", programs.join(" or ")))
        }
    }
}

/// Every server of the project at `root`: the configured ones, then the
/// detected ones whose language is not configured. For a query that is
/// about the whole project rather than one file.
pub(crate) fn all(
    root: &Path,
    configured: &[LspServerConfig],
    has: &dyn Fn(&str) -> bool,
) -> Vec<ServerSpec> {
    let mut found: Vec<ServerSpec> = configured.iter().map(ServerSpec::configured).collect();
    for known in KNOWN {
        if known.replaced_by(configured) || !known.applies(root) {
            continue;
        }
        if let Some(command) = known.commands.iter().find(|c| has(program(c))) {
            found.push(ServerSpec::detected(command));
        }
    }
    found
}

/// The programs detection could start for `root`, going by its marker files
/// alone: what is installed is not looked at, so the answer is the same on
/// every machine. For the trust summary.
pub(crate) fn detectable(root: &Path, configured: &[LspServerConfig]) -> Vec<String> {
    let known = KNOWN
        .iter()
        .filter(|k| !k.replaced_by(configured) && k.applies(root));
    known
        .map(|k| {
            let programs: Vec<&str> = k.commands.iter().map(|c| program(c)).collect();
            programs.join(" or ")
        })
        .collect()
}

/// The language a detected server would serve `ext` as, for messages.
pub(crate) fn language_of(ext: &str) -> Option<&'static str> {
    let known = KNOWN.iter().find(|k| k.extensions.contains(&ext));
    known.map(|k| k.language)
}

/// The `languageId` a file with extension `ext` is opened as.
pub(crate) fn language_id(ext: &str) -> &str {
    match ext {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        // `go`, and whatever a configured server is given: most ids are
        // the extension.
        other => other,
    }
}

/// `path` (absolute) as a `file://` URI.
pub(crate) fn uri_of_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    // `C:\dir\file` becomes `/C:/dir/file`.
    let text = match cfg!(windows) {
        true => format!("/{}", text.replace('\\', "/")),
        false => text.into_owned(),
    };
    let mut uri = String::from("file://");
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(byte as char)
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

/// The path a `file://` URI names; `None` for any other scheme (a server's
/// virtual documents) or an encoding that is not UTF-8.
pub(crate) fn path_of_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // An authority, if any, ends at the first slash; it is empty or
    // `localhost` for a local file.
    let path = &rest[rest.find('/')?..];
    let mut bytes = Vec::with_capacity(path.len());
    let mut input = path.bytes();
    while let Some(byte) = input.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let hex = [input.next()?, input.next()?];
        let hex = std::str::from_utf8(&hex).ok()?;
        bytes.push(u8::from_str_radix(hex, 16).ok()?);
    }
    let text = String::from_utf8(bytes).ok()?;
    Some(PathBuf::from(match cfg!(windows) {
        true => text.trim_start_matches('/').to_string(),
        false => text,
    }))
}

/// Starts a server: its stdio, and its process.
pub(crate) type Spawn = dyn Fn(&ServerSpec, &Path) -> Result<Transport> + Send + Sync;

/// Run `spec`'s command in `root`, as its own process group — so that
/// killing the group reaches what the server starts — with its stderr
/// discarded (servers log there, at length).
fn spawn(spec: &ServerSpec, root: &Path) -> Result<Transport> {
    let argv = crate::shlex::split(&spec.command)?;
    let Some((program, args)) = argv.split_first() else {
        bail!("empty command");
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unixlike)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn()?;
    let writer = child.stdin.take().context("no stdin")?;
    let reader = child.stdout.take().context("no stdout")?;
    Ok(Transport {
        writer: Box::new(writer),
        reader: Box::new(reader),
        child: Some(child),
    })
}

/// One server of one project, across its restarts.
#[derive(Default)]
struct Slot {
    client: Option<Arc<Client>>,
    /// How many times it was started.
    starts: u32,
    /// Why it is not to be tried again, once it is not.
    given_up: Option<String>,
}

/// The slots, by project root and server name. Each has a lock of its
/// own, so a server that is slow to start holds up only its own callers.
type Slots = HashMap<(PathBuf, String), Arc<Mutex<Slot>>>;

/// The servers a process has running. See the module docs.
pub(crate) struct Servers {
    spawn: Box<Spawn>,
    slots: Mutex<Slots>,
}

impl Servers {
    /// A set whose servers are started by `spawn`.
    pub(crate) fn new(spawn: Box<Spawn>) -> Self {
        Self {
            spawn,
            slots: Mutex::default(),
        }
    }

    /// A set whose servers are real processes.
    pub(crate) fn processes() -> Self {
        Self::new(Box::new(spawn))
    }

    /// The process's own set.
    pub(crate) fn global() -> &'static Arc<Servers> {
        static GLOBAL: OnceLock<Arc<Servers>> = OnceLock::new();
        GLOBAL.get_or_init(|| Arc::new(Servers::processes()))
    }

    /// Run `op` against `spec`'s server for the project at `root`, starting
    /// it if need be. `op` is given the instant by which the whole call
    /// must be done. When the server dies under `op` it is restarted (once
    /// in its life) and `op` run again. `Err` is why there is no answer, as
    /// a clause that follows the server's name.
    pub(crate) fn with<T>(
        &self,
        root: &Path,
        spec: &ServerSpec,
        op: impl Fn(&Client, Instant) -> Result<T, Failure>,
    ) -> Result<T, String> {
        let deadline = Instant::now() + spec.timeout;
        let secs = spec.timeout.as_secs();
        let slot = {
            let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            let key = (root.to_path_buf(), spec.name.clone());
            Arc::clone(slots.entry(key).or_default())
        };
        loop {
            let client = self.client(&slot, root, spec, deadline)?;
            match op(&client, deadline) {
                Ok(value) => return Ok(value),
                // Around again: `client` restarts it, or gives up.
                Err(Failure::Died) => continue,
                Err(Failure::Timeout) => {
                    return Err(format!(
                        "did not answer within {secs}s (it may still be indexing the project; \
                         a later call may get through)"
                    ));
                }
                Err(Failure::Server(message)) => {
                    return Err(format!("answered with an error: {message}"));
                }
            }
        }
    }

    /// The slot's live client, starting the server if it is not running
    /// and may still be started. The slot stays locked for the handshake,
    /// so calls arriving together wait for one start instead of racing to
    /// make several.
    fn client(
        &self,
        slot: &Mutex<Slot>,
        root: &Path,
        spec: &ServerSpec,
        deadline: Instant,
    ) -> Result<Arc<Client>, String> {
        let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(why) = &slot.given_up {
                return Err(why.clone());
            }
            if let Some(client) = slot.client.as_ref().filter(|c| !c.is_dead()) {
                return Ok(Arc::clone(client));
            }
            slot.client = None;
            if slot.starts >= MAX_STARTS {
                let why = "exited again after being restarted; it is not started a third time";
                slot.given_up = Some(why.to_string());
                continue;
            }
            slot.starts += 1;
            let transport = match (self.spawn)(spec, root) {
                Ok(transport) => transport,
                Err(e) => {
                    slot.given_up = Some(format!("could not be started (`{}`: {e})", spec.command));
                    continue;
                }
            };
            match Client::start(transport, root, spec.options(), deadline) {
                Ok(client) => slot.client = Some(Arc::new(client)),
                // Exited during the handshake: counts as a death.
                Err(Failure::Died) => continue,
                Err(Failure::Timeout) => {
                    slot.given_up = Some("did not answer `initialize`".to_string());
                }
                Err(Failure::Server(message)) => {
                    slot.given_up = Some(format!("refused `initialize`: {message}"));
                }
            }
        }
    }

    /// Stop every server and start none again.
    pub(crate) fn kill_all(&self) {
        let slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        for slot in slots.values() {
            let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
            slot.given_up = Some("was stopped: atelier is exiting".to_string());
            if let Some(client) = slot.client.take() {
                // Not left to `Drop`: a call in flight still holds it.
                client.kill();
            }
        }
    }
}

/// Stop every language server this process started. Called as atelier
/// exits: they would otherwise outlive it.
pub fn kill_all() {
    Servers::global().kill_all();
}

#[cfg(test)]
mod tests {
    use super::testutil::{Act, FakeServer};
    use super::*;

    fn dir(name: &str) -> PathBuf {
        crate::trust::testing::dir(&format!("lsp-{name}"))
    }

    fn config(name: &str, command: &str, extensions: &[&str]) -> LspServerConfig {
        LspServerConfig {
            name: name.to_string(),
            command: command.to_string(),
            extensions: extensions.iter().map(|e| e.to_string()).collect(),
            timeout_secs: 7,
        }
    }

    #[test]
    fn a_server_is_detected_from_a_marker_file_when_its_program_is_installed() {
        let root = dir("detect");
        let everything = |_: &str| true;
        let nothing = |_: &str| false;
        // No marker: no server, whatever is installed.
        let why = choose(&root, &[], "rs", &everything).unwrap_err();
        assert_eq!(why, "the project root has no Cargo.toml");

        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        let spec = choose(&root, &[], "rs", &everything).unwrap();
        assert_eq!(
            (spec.name.as_str(), spec.command.as_str(), spec.timeout),
            ("rust-analyzer", "rust-analyzer", DEFAULT_TIMEOUT)
        );
        let why = choose(&root, &[], "rs", &nothing).unwrap_err();
        assert_eq!(why, "rust-analyzer is not on PATH");

        // The first installed of a language's servers.
        std::fs::write(root.join("requirements.txt"), "").unwrap();
        let only_pylsp = |program: &str| program == "pylsp";
        assert_eq!(
            choose(&root, &[], "py", &everything).unwrap().command,
            "pyright-langserver --stdio"
        );
        assert_eq!(
            choose(&root, &[], "py", &only_pylsp).unwrap().command,
            "pylsp"
        );
        assert_eq!(
            choose(&root, &[], "py", &nothing).unwrap_err(),
            "pyright-langserver or pylsp is not on PATH"
        );

        // A language nothing is known for.
        let why = choose(&root, &[], "zig", &everything).unwrap_err();
        assert!(why.starts_with("none is known for .zig files"), "{why}");

        // Every marker, every server.
        for marker in ["go.mod", "package.json", "compile_commands.json"] {
            std::fs::write(root.join(marker), "").unwrap();
        }
        let names: Vec<String> = (all(&root, &[], &everything).into_iter())
            .map(|s| s.name)
            .collect();
        assert_eq!(
            names,
            [
                "rust-analyzer",
                "gopls",
                "typescript-language-server",
                "pyright-langserver",
                "clangd"
            ]
        );
        assert_eq!(
            detectable(&root, &[]),
            [
                "rust-analyzer",
                "gopls",
                "typescript-language-server",
                "pyright-langserver or pylsp",
                "clangd"
            ]
        );
        assert!(all(&root, &[], &nothing).is_empty());
    }

    #[test]
    fn a_configured_server_replaces_the_detected_one_for_its_extensions() {
        let root = dir("configured");
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        std::fs::write(root.join("go.mod"), "").unwrap();
        let configured = [
            config("rust", "ra-multiplex --flag", &["rs"]),
            config("zig", "zls", &[".zig"]),
        ];
        let everything = |_: &str| true;
        let spec = choose(&root, &configured, "rs", &everything).unwrap();
        assert_eq!(
            spec,
            ServerSpec {
                name: "rust".to_string(),
                command: "ra-multiplex --flag".to_string(),
                timeout: Duration::from_secs(7),
            }
        );
        // Written with its dot, as people do.
        assert_eq!(
            choose(&root, &configured, "zig", &everything).unwrap().name,
            "zig"
        );
        // Used even when nothing says it is installed: starting it will
        // tell.
        assert!(choose(&root, &configured, "rs", &|_| false).is_ok());
        // Other languages are still detected.
        assert_eq!(
            choose(&root, &configured, "go", &everything).unwrap().name,
            "gopls"
        );
        let names: Vec<String> = (all(&root, &configured, &everything).into_iter())
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["rust", "zig", "gopls"]);
        assert_eq!(detectable(&root, &configured), ["gopls"]);
    }

    #[test]
    fn rust_analyzer_is_told_not_to_run_cargo_check() {
        let options = |command: &str| ServerSpec::detected(command).options();
        assert_eq!(options("rust-analyzer"), json!({ "checkOnSave": false }));
        assert_eq!(
            options("/opt/bin/rust-analyzer --log-file x"),
            json!({ "checkOnSave": false })
        );
        assert_eq!(options("gopls"), Value::Null);
    }

    #[test]
    fn paths_and_uris_convert_both_ways() {
        if cfg!(windows) {
            return;
        }
        for (path, uri) in [
            ("/p/src/main.rs", "file:///p/src/main.rs"),
            ("/p/my project/a#b.rs", "file:///p/my%20project/a%23b.rs"),
            ("/p/日本/é.rs", "file:///p/%E6%97%A5%E6%9C%AC/%C3%A9.rs"),
        ] {
            assert_eq!(uri_of_path(Path::new(path)), uri);
            assert_eq!(path_of_uri(uri), Some(PathBuf::from(path)));
        }
        // Lower-case escapes and an explicit host, as some servers write.
        assert_eq!(
            path_of_uri("file://localhost/p/a%2fb"),
            Some(PathBuf::from("/p/a/b"))
        );
        // Not files, or not decodable.
        assert_eq!(path_of_uri("untitled:Untitled-1"), None);
        assert_eq!(path_of_uri("jdt://contents/x.class"), None);
        assert_eq!(path_of_uri("file:///p/%zz"), None);
        assert_eq!(path_of_uri("file:///p/%4"), None);
    }

    fn fake_servers(server: &FakeServer) -> Servers {
        let server = server.clone();
        Servers::new(Box::new(move |_, _| Ok(server.transport())))
    }

    fn spec(timeout: Duration) -> ServerSpec {
        ServerSpec {
            name: "fake".to_string(),
            command: "fake-ls".to_string(),
            timeout,
        }
    }

    fn ping(client: &Client, deadline: Instant) -> Result<Value, Failure> {
        client.request("ping", Value::Null, deadline)
    }

    #[test]
    fn a_server_is_started_once_and_kept() {
        let server = FakeServer::new(|_, _| Act::Reply(json!("pong")));
        let servers = fake_servers(&server);
        let spec = spec(Duration::from_secs(10));
        for _ in 0..3 {
            let got = servers.with(Path::new("/p"), &spec, ping);
            assert_eq!(got, Ok(json!("pong")));
        }
        assert_eq!(server.starts(), 1);
        // Another project gets a server of its own.
        servers.with(Path::new("/q"), &spec, ping).unwrap();
        assert_eq!(server.starts(), 2);
    }

    #[test]
    fn a_server_that_dies_is_restarted_once() {
        // Dies on every other `ping`.
        let pings = std::sync::atomic::AtomicUsize::new(0);
        let server = FakeServer::new(move |method, _| match method {
            "ping" => match pings.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 => Act::Exit,
                1 => Act::Reply(json!("pong")),
                _ => Act::Exit,
            },
            _ => Act::Reply(Value::Null),
        });
        let servers = fake_servers(&server);
        let spec = spec(Duration::from_secs(10));
        // The first call survives the death: the server is restarted and
        // asked again.
        assert_eq!(
            servers.with(Path::new("/p"), &spec, ping),
            Ok(json!("pong"))
        );
        assert_eq!(server.starts(), 2);
        // The second death is final, and stays so.
        for _ in 0..2 {
            let why = servers.with(Path::new("/p"), &spec, ping).unwrap_err();
            assert!(why.contains("exited again after being restarted"), "{why}");
        }
        assert_eq!(server.starts(), 2);
    }

    #[test]
    fn a_slow_server_times_out_and_is_kept() {
        let server = FakeServer::new(|method, _| match method {
            "ping" => Act::Ignore,
            _ => Act::Reply(json!("ok")),
        });
        let servers = fake_servers(&server);
        let why = servers
            .with(Path::new("/p"), &spec(Duration::from_millis(200)), ping)
            .unwrap_err();
        assert!(why.starts_with("did not answer within 0s"), "{why}");
        // Still running: a later call goes to the same server.
        let got = servers.with(Path::new("/p"), &spec(Duration::from_secs(10)), |c, d| {
            c.request("other", Value::Null, d)
        });
        assert_eq!(got, Ok(json!("ok")));
        assert_eq!(server.starts(), 1);
    }

    #[test]
    fn a_server_that_cannot_start_is_given_up_on_with_the_reason() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&attempts);
        let servers = Servers::new(Box::new(move |_, _| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            bail!("No such file or directory")
        }));
        let spec = spec(Duration::from_secs(10));
        for _ in 0..2 {
            let why = servers.with(Path::new("/p"), &spec, ping).unwrap_err();
            assert_eq!(
                why,
                "could not be started (`fake-ls`: No such file or directory)"
            );
        }
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_server_error_is_passed_on_and_kill_all_stops_everything() {
        let server = FakeServer::new(|method, _| match method {
            "ping" => Act::Error("not yet".to_string()),
            _ => Act::Reply(Value::Null),
        });
        let servers = fake_servers(&server);
        let spec = spec(Duration::from_secs(10));
        let why = servers.with(Path::new("/p"), &spec, ping).unwrap_err();
        assert_eq!(why, "answered with an error: not yet");
        servers.kill_all();
        let why = servers.with(Path::new("/p"), &spec, ping).unwrap_err();
        assert_eq!(why, "was stopped: atelier is exiting");
        assert_eq!(server.starts(), 1);
    }

    /// The real thing: a process is spawned in its own group, fails to
    /// speak LSP, and is gone — with what it started — after `kill_all`.
    #[cfg(unix)]
    #[test]
    fn a_real_process_is_spawned_in_the_project_and_killed_with_its_group() {
        let root = dir("spawn");
        let servers = Servers::processes();
        // A "server" that starts a helper, records both pids and where it
        // runs, then reads its input forever without answering.
        let spec = ServerSpec {
            name: "mute".to_string(),
            command: "sh -c 'sleep 300 & echo $$ $! > pids; pwd > cwd; cat > /dev/null'"
                .to_string(),
            timeout: Duration::from_millis(1500),
        };
        let why = servers.with(&root, &spec, ping).unwrap_err();
        assert_eq!(why, "did not answer `initialize`");
        let pids = std::fs::read_to_string(root.join("pids")).unwrap();
        let cwd = std::fs::read_to_string(root.join("cwd")).unwrap();
        assert_eq!(
            std::fs::canonicalize(cwd.trim()).unwrap(),
            std::fs::canonicalize(&root).unwrap()
        );
        // Giving up on it killed it, helper included. (Looked up in /proc,
        // where there is one.)
        let pids: Vec<&str> = pids.split_whitespace().collect();
        assert_eq!(pids.len(), 2);
        if !Path::new("/proc/self/stat").exists() {
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while pids.iter().any(|p| running(p)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        for pid in pids {
            assert!(!running(pid), "pid {pid} survived");
        }
    }

    /// Whether `pid` is a running process (a zombie waiting to be reaped is
    /// not).
    #[cfg(unix)]
    fn running(pid: &str) -> bool {
        let Ok(stat) = std::fs::read_to_string(Path::new("/proc").join(pid).join("stat")) else {
            return false;
        };
        // "pid (comm) S ...": the state follows the closing parenthesis.
        !(stat.rsplit(") ").next()).is_some_and(|rest| rest.starts_with('Z'))
    }
}
