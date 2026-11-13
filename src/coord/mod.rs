//! The coordinator: one small daemon that connects every atelier running
//! on the machine.
//!
//! Each atelier process (an *instance*) keeps a connection to the
//! coordinator over a Unix socket in a private directory and speaks
//! newline-delimited JSON-RPC 2.0 with it. The coordinator only routes; it
//! holds no durable state, so it can go away and come back at any time:
//!
//! - **Launch.** There is no service to install. An instance that finds no
//!   coordinator starts one (`atelier coordinator`, detached) and connects.
//!   A lock file held for the daemon's lifetime guarantees a single
//!   coordinator however many instances race to start it.
//! - **Restart.** When the coordinator ends — a crash, an update, a `kill` —
//!   every instance notices the closed connection, and the first to retry
//!   starts a new one; all of them reconnect under the same identity and
//!   re-subscribe to their topics ([`client`]). The coordinator also ends
//!   itself when its executable is replaced on disk (a self-update, a
//!   rebuild), when a newer atelier says hello, and when no instance has
//!   been connected for a while ([`daemon`]).
//!
//! What it routes between instances:
//!
//! | scheme     | request                       | arrives as                    |
//! |------------|-------------------------------|-------------------------------|
//! | presence   | `list`, `update`              | —                             |
//! | direct     | `send {to: name|id, body}`    | `message` notification        |
//! | broadcast  | `send {to: "*", body}`        | `message` (`broadcast: true`) |
//! | topics     | `subscribe`/`publish {topic}` | `message` (`topic: …`)        |
//! | RPC        | `call {to, method, params}`   | `call` request, answered      |
//!
//! Inside atelier these surface as the `peers` and `peer_send` tools
//! ([`tools`]), the `/peers`, `/send`, `/subscribe` commands, and messages
//! from other instances delivered into the conversation (`agent`).
//!
//! Unix only (`cfg(unixlike)`, see `build.rs`): elsewhere [`start`] returns
//! `None` and atelier runs alone.

#[cfg(unixlike)]
pub mod client;
#[cfg(not(unixlike))]
#[path = "stub.rs"]
pub mod client;
#[cfg(unixlike)]
pub mod daemon;
#[cfg(all(test, unixlike))]
mod tests;
pub mod tools;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

pub use client::Client;

/// Wire protocol revision. A coordinator only serves instances that speak
/// its revision; one that meets a newer atelier steps down for it.
pub const PROTO: u64 = 1;
/// Set (to anything but `0`) to run without the coordinator.
pub const ENV_DISABLE: &str = "ATELIER_NO_COORDINATOR";
/// Overrides the directory holding the socket, lock and log.
pub const ENV_DIR: &str = "ATELIER_COORDINATOR_DIR";

/// JSON-RPC error: the coordinator is older than the caller and is
/// restarting to let a newer one take over.
pub const ERR_SUPERSEDED: i64 = -32001;
/// JSON-RPC error: the caller speaks a protocol this coordinator doesn't.
pub const ERR_INCOMPATIBLE: i64 = -32002;

/// A message from another instance.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// The sender's name (unique among connected instances).
    pub from: String,
    /// The directory the sender works in.
    pub cwd: String,
    /// Set when it was published to a topic this instance subscribes to.
    pub topic: Option<String>,
    /// Sent to every instance rather than to this one.
    pub broadcast: bool,
    pub body: String,
}

impl Message {
    /// How it was addressed, for display: ``, ` (to all)` or ` (#topic)`.
    pub fn scope(&self) -> String {
        match (&self.topic, self.broadcast) {
            (Some(t), _) => format!(" (#{t})"),
            (None, true) => " (to all)".to_string(),
            (None, false) => String::new(),
        }
    }
}

/// Where the coordinator's socket, lock and log live:
/// `$ATELIER_COORDINATOR_DIR`, else `$XDG_RUNTIME_DIR/atelier`, else
/// `~/.local/state/atelier`. `None` without any of those.
pub fn runtime_dir() -> Option<PathBuf> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    if let Some(d) = var(ENV_DIR) {
        return Some(PathBuf::from(d));
    }
    if let Some(d) = var("XDG_RUNTIME_DIR") {
        return Some(PathBuf::from(d).join("atelier"));
    }
    var("HOME").map(|h| PathBuf::from(h).join(".local/state/atelier"))
}

pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join("coordinator.sock")
}

/// The socket in `dir`, as a path short enough to bind or connect to.
///
/// A socket address holds about a hundred bytes of path. When the real path
/// is longer (a deep home directory), Linux lets us name the socket through
/// an open handle on its directory instead; the handle is returned and must
/// outlive the bind/connect call.
#[cfg(unixlike)]
fn reachable_socket(dir: &Path) -> Result<(PathBuf, Option<std::fs::File>)> {
    use std::os::fd::AsRawFd;
    const MAX: usize = 100;
    let socket = socket_path(dir);
    if socket.as_os_str().len() <= MAX {
        return Ok((socket, None));
    }
    // (fullrust is Linux under another `target_os`; rustc knows the name
    // only in that toolchain.)
    #[allow(unexpected_cfgs)]
    let procfs = cfg!(any(target_os = "linux", target_os = "fullrust"));
    if procfs && let Ok(handle) = std::fs::File::open(dir) {
        let short = PathBuf::from(format!("/proc/self/fd/{}", handle.as_raw_fd()))
            .join(socket.file_name().expect("has a file name"));
        return Ok((short, Some(handle)));
    }
    anyhow::bail!(
        "{} is too long for a socket path; point {ENV_DIR} at a shorter directory",
        socket.display()
    )
}

/// Connect to the coordinator serving `dir`.
#[cfg(unixlike)]
fn connect(dir: &Path) -> Result<std::os::unix::net::UnixStream> {
    let (path, _handle) = reachable_socket(dir)?;
    Ok(std::os::unix::net::UnixStream::connect(path)?)
}

/// Whether the user turned the coordinator off.
fn disabled() -> bool {
    std::env::var_os(ENV_DISABLE).is_some_and(|v| !v.is_empty() && v != "0")
}

/// Join the coordinator (starting one if needed) as an instance working in
/// `root`. Returns at once: connecting happens in the background, and keeps
/// being retried for as long as the process lives. `None` when disabled or
/// unsupported here.
pub fn start(root: &Path, model: &str) -> Option<Arc<Client>> {
    if disabled() || cfg!(not(unixlike)) {
        return None;
    }
    let dir = runtime_dir()?;
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "atelier".to_string());
    Some(Client::start(client::Options {
        dir,
        // Resolved now: after a self-update replaces the binary, the running
        // process's own path no longer names a file.
        exe: std::env::current_exe().ok(),
        name,
        cwd: root.display().to_string(),
        model: model.to_string(),
    }))
}

/// `atelier coordinator [status|stop]`. With no argument, be the
/// coordinator (what instances launch); `status` and `stop` talk to a
/// running one and never start it.
pub fn run_command(args: &[String]) -> Result<()> {
    #[cfg(unixlike)]
    {
        let dir = runtime_dir()
            .ok_or_else(|| anyhow::anyhow!("no runtime directory (set {ENV_DIR} or HOME)"))?;
        match args.first().map(String::as_str) {
            None => daemon::run(daemon::Config::new(dir)),
            Some("status") => {
                match client::control(&dir, "status") {
                    Ok(s) => {
                        println!(
                            "coordinator {} (pid {}), up {}s, {}",
                            s["version"].as_str().unwrap_or("?"),
                            s["pid"],
                            s["uptime_secs"],
                            socket_path(&dir).display()
                        );
                        let list = client::control(&dir, "list")?;
                        for line in tools::render_instances(&list) {
                            println!("{line}");
                        }
                    }
                    Err(_) => println!("coordinator not running ({})", dir.display()),
                }
                Ok(())
            }
            Some("stop") => {
                match client::control(&dir, "shutdown") {
                    Ok(_) => println!("coordinator stopped"),
                    Err(_) => println!("coordinator not running"),
                }
                Ok(())
            }
            Some(other) => anyhow::bail!(
                "unknown coordinator command '{other}' (usage: atelier coordinator [status|stop])"
            ),
        }
    }
    #[cfg(not(unixlike))]
    {
        let _ = args;
        anyhow::bail!("the coordinator is not supported on this platform")
    }
}
