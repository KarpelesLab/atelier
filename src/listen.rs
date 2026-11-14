//! `atelier serve --listen`: the embedding protocol ([`crate::serve`]) on a
//! socket, for a program that did not launch atelier — a web page, a second
//! process — to connect to. The reference for a client is the "Listening"
//! section of `docs/protocol.md`.
//!
//! # One process per connection
//!
//! A session owns its process: the working directory is the project, stdout
//! is the protocol's, and leaving is ending the process. So the listener
//! runs no session itself. For each connection it lets in, it starts
//! `atelier serve` again — the same executable, the same flags less the
//! listener's own, the same directory — and carries messages between the
//! connection and that child's stdin and stdout. `serve` over stdio stays
//! the only implementation of the protocol, sessions cannot reach each
//! other's state, and a disconnect is what the end of stdin already was:
//! the child interrupts what runs and exits (see "Leaving" in
//! [`crate::serve`]). The child is started at the connection's first
//! message, so a connection that says nothing costs nothing.
//!
//! # Who gets in
//!
//! - A **Unix socket** is a file: mode 0600, in a directory no other user
//!   can enter, which the listener checks (or creates so) before binding.
//!   Whoever can open it is the user.
//! - **TCP** is loopback only, and loopback is shared by every user and
//!   every program on the machine — and reachable by any web page the
//!   user's browser shows. So a connection presents a **token** before
//!   anything else: 256 random bits written to a 0600 file, or given by
//!   the caller in [`ENV_TOKEN`]; never on a command line, which other
//!   users can read. A WebSocket handshake that carries an `Origin` (a
//!   browser's always does) must also name one from `--allow-origin`.
//!
//! Until a connection is let in it has [`HANDSHAKE`] to say who it is, can
//! make the listener read a few kilobytes at most, and there can be
//! [`MAX_PENDING`] of them; once in, it holds one of `--max-sessions`.
//!
//! # Leaving
//!
//! The listener serves until it is killed (atelier handles no signals,
//! here or elsewhere). Its children then see their stdin end and leave on
//! their own. What stays behind is harmless and cleaned up by the next
//! listener: a socket file nobody answers on is removed, and a token file
//! holds a token nothing accepts any more.

mod ws;

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::rpc::{self, lock};

/// The environment variable a caller gives the TCP token in, instead of
/// having one made and written to a file.
pub const ENV_TOKEN: &str = "ATELIER_SERVE_TOKEN";

/// `--max-sessions` when not given.
pub const DEFAULT_MAX_SESSIONS: usize = 8;

/// How long a TCP connection has to present its token (and, for a
/// WebSocket, finish its handshake).
const HANDSHAKE: Duration = Duration::from_secs(10);

/// How many TCP connections may be in their handshake at once.
const MAX_PENDING: usize = 32;

/// The longest line read from a connection that has not been let in: the
/// token line, an HTTP request line, a header.
const MAX_HEAD_LINE: usize = 8 << 10;

/// The most header lines read from an HTTP request.
const MAX_HEADERS: usize = 64;

/// How long a session's process has to leave once its client is gone
/// (`serve` gives a running turn five seconds), before it is killed.
const REAP: Duration = Duration::from_secs(10);

/// How long a closed WebSocket is read for the peer's closing frame.
const CLOSING: Duration = Duration::from_secs(5);

/// The listener's own JSON-RPC error codes, next to `serve`'s. Part of the
/// contract (`docs/protocol.md`): never renumber.
mod code {
    /// The token is missing or wrong.
    pub const UNAUTHORIZED: i64 = -32020;
    /// `--max-sessions` connections are being served already.
    pub const TOO_MANY_SESSIONS: i64 = -32021;
}

/// `atelier serve --listen …`, as asked for on the command line.
#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub at: Where,
    /// `--allow-origin`, normalized (see [`origin`]).
    pub allow_origins: Vec<String>,
    pub max_sessions: usize,
}

/// Where to listen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Where {
    /// A Unix domain socket at this path.
    Unix(PathBuf),
    /// A loopback address; port 0 lets the system pick.
    Tcp(SocketAddr),
}

impl Where {
    /// Parse `--listen`'s value: `unix:PATH`, a path (something with a `/`
    /// in it), or a loopback `ADDRESS:PORT`. The error is for the user.
    pub fn parse(s: &str) -> Result<Where, String> {
        let unix = |path: &str| {
            if path.is_empty() {
                Err("--listen: unix: needs a path".to_string())
            } else if cfg!(not(unixlike)) {
                Err(
                    "--listen: Unix sockets are not available on this platform; \
                     listen on 127.0.0.1:PORT"
                        .to_string(),
                )
            } else {
                Ok(Where::Unix(PathBuf::from(path)))
            }
        };
        if let Some(path) = s.strip_prefix("unix:") {
            return unix(path);
        }
        if let Some(port) = s.strip_prefix("localhost:") {
            let port = port
                .parse::<u16>()
                .map_err(|_| format!("--listen: '{port}' is not a port"))?;
            return Ok(Where::Tcp(SocketAddr::from((Ipv4Addr::LOCALHOST, port))));
        }
        if let Ok(addr) = s.parse::<SocketAddr>() {
            if !addr.ip().is_loopback() {
                return Err(format!(
                    "--listen: {} is not a loopback address. atelier serve listens on this \
                     machine only (127.0.0.1, [::1], localhost, or a Unix socket): whoever \
                     connects drives an agent with your files and your shell",
                    addr.ip()
                ));
            }
            return Ok(Where::Tcp(addr));
        }
        if s.contains('/') {
            return unix(s);
        }
        Err(format!(
            "--listen: '{s}' is neither a loopback ADDRESS:PORT (127.0.0.1:PORT, [::1]:PORT, \
             localhost:PORT) nor a socket path (unix:PATH)"
        ))
    }
}

/// An `--allow-origin` value, or an `Origin` header, in the one form they
/// are compared in: lower case, no trailing slash. `Err` for what cannot be
/// an origin (`scheme://host[:port]`, or `null`).
pub fn origin(s: &str) -> Result<String, String> {
    let o = s.trim().trim_end_matches('/').to_ascii_lowercase();
    let well_formed = o == "null"
        || o.split_once("://").is_some_and(|(scheme, host)| {
            !scheme.is_empty() && !host.is_empty() && !host.contains(['/', '?', '#', '@', ' ', '*'])
        });
    if well_formed {
        Ok(o)
    } else {
        Err(format!(
            "--allow-origin: '{s}' is not an origin (like http://localhost:5173)"
        ))
    }
}

/// The arguments each session's `atelier serve` is started with: this
/// process's own (`serve` first), less the listener's flags. Whatever else
/// `serve` takes — now or later — passes through.
pub fn child_args(args: &[String]) -> Vec<String> {
    const OWN: [&str; 3] = ["--listen", "--allow-origin", "--max-sessions"];
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if OWN.contains(&arg.as_str()) {
            it.next(); // its value
        } else if !OWN
            .iter()
            .any(|own| arg.strip_prefix(own).is_some_and(|v| v.starts_with('=')))
        {
            out.push(arg.clone());
        }
    }
    out
}

/// Listen as `opts` says and serve a session to every connection let in,
/// each in a process of its own started with `child_args`. Returns only on
/// a failure to start: once listening, it runs until killed.
pub fn run(opts: Options, child_args: Vec<String>) -> Result<u8> {
    let exe = crate::app::helper_exe().context("finding atelier's own executable")?;
    let listener = |token| {
        Arc::new(Listener {
            exe,
            child_args,
            token,
            origins: opts.allow_origins,
            max_sessions: opts.max_sessions,
            sessions: AtomicUsize::new(0),
            pending: AtomicUsize::new(0),
            connections: AtomicU64::new(0),
        })
    };
    match opts.at {
        Where::Tcp(addr) => {
            // Before binding: a token that cannot be used stops the start.
            let given = token_from_env()?;
            let socket = TcpListener::bind(addr).with_context(|| format!("listening on {addr}"))?;
            let addr = socket.local_addr()?;
            let (token, file) = match given {
                Some(token) => (token, None),
                None => {
                    let token = crate::oauth::random_b64url(32);
                    let file = write_token_file(&token, addr.port())?;
                    (token, Some(file))
                }
            };
            announce(&json!({
                "transport": "tcp",
                "address": addr.to_string(),
                "ws": format!("ws://{addr}/"),
                "token_file": file.as_ref().map(|f| f.display().to_string()),
            }));
            eprintln!(
                "atelier serve: listening on {addr} — JSON-RPC lines, or WebSocket at ws://{addr}/"
            );
            match &file {
                Some(file) => eprintln!("atelier serve: the token is in {}", file.display()),
                None => eprintln!("atelier serve: the token is {ENV_TOKEN}'s"),
            }
            eprintln!(
                "atelier serve: a program sends {{\"token\":\"…\"}} as its first line; a page \
                 opens new WebSocket(\"ws://{addr}/\", [\"atelier\", \"token.\" + token]) and \
                 needs --allow-origin"
            );
            let listener = listener(Some(token));
            listener.accept(|| {
                let (stream, _) = socket.accept()?;
                let _ = stream.set_nodelay(true);
                Ok(Stream::Tcp(stream))
            })
        }
        #[cfg(unixlike)]
        Where::Unix(path) => {
            let (socket, path) = bind_unix(&path)?;
            announce(&json!({ "transport": "unix", "path": path.display().to_string() }));
            eprintln!(
                "atelier serve: listening on {} — JSON-RPC lines (try: socat - UNIX-CONNECT:{})",
                path.display(),
                path.display()
            );
            let listener = listener(None);
            listener.accept(|| Ok(Stream::Unix(socket.accept()?.0)))
        }
        #[cfg(not(unixlike))]
        Where::Unix(_) => bail!("Unix sockets are not available on this platform"),
    }
}

/// The one line on stdout: where a program that started the listener can
/// reach it.
fn announce(listening: &Value) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{}", json!({ "listening": listening }));
    let _ = out.flush();
}

// ------------------------------------------------------------------ token

/// The token the caller chose, if it chose one. It travels in a URL or a
/// WebSocket subprotocol, so it is held to what both carry unescaped; and
/// it is the whole of the protection, so it cannot be short.
fn token_from_env() -> Result<Option<String>> {
    let Some(token) = std::env::var_os(ENV_TOKEN).filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    let Some(token) = token.to_str().filter(|t| {
        t.len() >= 16
            && t.len() <= 512
            && t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
    }) else {
        bail!("{ENV_TOKEN} must be 16 to 512 characters of A-Z a-z 0-9 - . _ ~");
    };
    Ok(Some(token.to_string()))
}

/// Write `token` where only the user can read it, and return the path:
/// `serve-PORT.token` in atelier's runtime directory, so a client that
/// knows the port can find it.
fn write_token_file(token: &str, port: u16) -> Result<PathBuf> {
    let dir = match crate::coord::runtime_dir() {
        Some(dir) => dir,
        // Per user on Windows; elsewhere HOME is all but always set.
        None if cfg!(windows) => std::env::temp_dir().join("atelier"),
        None => bail!("nowhere to write the token (no XDG_RUNTIME_DIR or HOME): set {ENV_TOKEN}"),
    };
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unixlike)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("serve-{port}.token"));
    // Never written through what is there (a link, a file with other
    // permissions): removed, then created anew.
    let _ = std::fs::remove_file(&path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unixlike)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&path)
        .with_context(|| format!("writing {}", path.display()))?;
    writeln!(file, "{token}").with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Whether `presented` is the token, in time that says nothing of where
/// they differ, or of how long the token is.
fn same_token(presented: &str, token: &str) -> bool {
    use purecrypto::ct::ConstantTimeEq;
    let a = purecrypto::hash::sha256(presented.as_bytes());
    let b = purecrypto::hash::sha256(token.as_bytes());
    a.ct_eq(&b).unwrap_u8() == 1
}

// ------------------------------------------------------------ unix socket

/// Bind the socket at `path`, mode 0600, in a directory only this user can
/// enter (created so when missing). A socket file left by a listener that
/// is gone is replaced; one that answers stops the start.
#[cfg(unixlike)]
fn bind_unix(path: &std::path::Path) -> Result<(std::os::unix::net::UnixListener, PathBuf)> {
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};

    let path = std::path::absolute(path).with_context(|| format!("{}", path.display()))?;
    let dir = path
        .parent()
        .with_context(|| format!("{} has no directory", path.display()))?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let dir_meta = std::fs::metadata(dir).with_context(|| format!("{}", dir.display()))?;
    if dir_meta.mode() & 0o077 != 0 {
        bail!(
            "{} is open to other users (mode {:o}): the socket goes in a directory only you \
             can enter. chmod 700 it, or name a socket in a directory that does not exist \
             yet and it is created so",
            dir.display(),
            dir_meta.mode() & 0o777
        );
    }
    let Some((short, _handle)) = crate::coord::short_socket(&path) else {
        bail!("{} is too long for a socket path", path.display());
    };
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_socket() => {
            if UnixStream::connect(&short).is_ok() {
                bail!("another listener is serving {}", path.display());
            }
            std::fs::remove_file(&path)
                .with_context(|| format!("removing the stale {}", path.display()))?;
        }
        Ok(_) => bail!("{} exists and is not a socket", path.display()),
        Err(_) => {}
    }
    let socket =
        UnixListener::bind(&short).with_context(|| format!("binding {}", path.display()))?;
    let secured = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .and_then(|()| std::fs::symlink_metadata(&path))
        .map_err(anyhow::Error::from)
        .and_then(|meta| {
            // The socket is ours by construction; the directory must be too,
            // or its owner can swap the socket for another.
            if meta.uid() != dir_meta.uid() {
                bail!("{} belongs to another user", dir.display());
            }
            Ok(())
        });
    if let Err(e) = secured {
        let _ = std::fs::remove_file(&path);
        return Err(e.context(format!("securing {}", path.display())));
    }
    Ok((socket, path))
}

// ------------------------------------------------------------- connection

/// A connection, whichever kind of socket it came in on.
enum Stream {
    Tcp(TcpStream),
    #[cfg(unixlike)]
    Unix(std::os::unix::net::UnixStream),
}

/// Apply `$call` to the socket inside a [`Stream`].
macro_rules! each {
    ($stream:expr, $s:ident => $call:expr) => {
        match $stream {
            Stream::Tcp($s) => $call,
            #[cfg(unixlike)]
            Stream::Unix($s) => $call,
        }
    };
}

impl Stream {
    fn try_clone(&self) -> io::Result<Stream> {
        Ok(match self {
            Stream::Tcp(s) => Stream::Tcp(s.try_clone()?),
            #[cfg(unixlike)]
            Stream::Unix(s) => Stream::Unix(s.try_clone()?),
        })
    }

    /// Bound reads and writes alike (`None`: wait for as long as it takes).
    /// The setting is the socket's, so it holds for every clone.
    fn set_timeouts(&self, read: Option<Duration>, write: Option<Duration>) {
        let _ = each!(self, s => s.set_read_timeout(read));
        let _ = each!(self, s => s.set_write_timeout(write));
    }

    /// Write `bytes`, all of them. (Whoever writes holds the connection's
    /// one [`Out`], or is alone with it during the handshake.)
    fn put(&self, bytes: &[u8]) -> io::Result<()> {
        each!(self, s => {
            let mut s = s;
            s.write_all(bytes)
        })
    }

    fn shutdown(&self, how: std::net::Shutdown) {
        let _ = each!(self, s => s.shutdown(how));
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        each!(self, s => s.read(buf))
    }
}

/// How messages are delimited on a connection.
enum Framing {
    /// One per line, as on stdio.
    Lines,
    /// One per WebSocket text message.
    WebSocket(ws::Assembler),
}

/// A JSON-RPC error that answers no request in particular, as a message.
fn error(code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": { "code": code, "message": message },
    })
    .to_string()
}

/// The way out of a connection, shared by the thread that reads it (which
/// answers pings and refusals) and the one that forwards the session's
/// output.
struct Out {
    stream: Mutex<Stream>,
    websocket: bool,
}

impl Out {
    /// One protocol message, whole.
    fn send(&self, text: &str) -> io::Result<()> {
        if self.websocket {
            return self.frame(ws::TEXT, text.as_bytes());
        }
        lock(&self.stream).put(format!("{text}\n").as_bytes())
    }

    /// Tell the peer why it is being turned away.
    fn refuse(&self, code: i64, message: &str) {
        let _ = self.send(&error(code, message));
    }

    fn frame(&self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        lock(&self.stream).put(&ws::encode_frame(opcode, payload, None))
    }

    /// Nothing more will be sent. A WebSocket is told so with a close frame
    /// and given a moment to answer it; a line connection is just closed.
    fn finish(&self, close: u16) {
        if self.websocket {
            let _ = self.frame(ws::CLOSE, &ws::close_payload(close));
            let stream = lock(&self.stream);
            stream.shutdown(std::net::Shutdown::Write);
            stream.set_timeouts(Some(CLOSING), Some(CLOSING));
        } else {
            lock(&self.stream).shutdown(std::net::Shutdown::Both);
        }
    }
}

/// What reading a connection came to.
enum Next {
    /// One message for the session, as a line without its end.
    Message(Vec<u8>),
    /// The peer left, or closed.
    End,
    /// The peer was cut off, for this reason (it was told, where it can be).
    Failed(String),
}

/// A line could not be read.
enum LineError {
    TooLong,
    Io,
}

/// The next line of `r`, without its end, reading at most `limit` bytes of
/// it; `None` at the end of the stream.
fn read_line(r: &mut impl BufRead, limit: usize) -> Result<Option<Vec<u8>>, LineError> {
    let mut line = Vec::new();
    // Two more than the line: its end, which is not counted.
    match r.take(limit as u64 + 2).read_until(b'\n', &mut line) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(_) => return Err(LineError::Io),
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
    // Without its end, a line this long was cut by the limit.
    if line.len() > limit {
        return Err(LineError::TooLong);
    }
    Ok(Some(line))
}

/// The reading side of a connection that was let in.
struct Inbound {
    reader: BufReader<Stream>,
    framing: Framing,
}

impl Inbound {
    /// The next message for the session. What the transport handles itself
    /// (blank lines, pings) is dealt with on the way.
    fn next(&mut self, out: &Out) -> Next {
        loop {
            match &mut self.framing {
                Framing::Lines => match read_line(&mut self.reader, ws::MAX_MESSAGE) {
                    Ok(Some(line)) if line.iter().all(u8::is_ascii_whitespace) => {}
                    Ok(Some(line)) => return Next::Message(line),
                    Ok(None) | Err(LineError::Io) => return Next::End,
                    Err(LineError::TooLong) => {
                        out.refuse(rpc::code::INVALID_REQUEST, "the line is too long");
                        return Next::Failed("a line over the size limit".into());
                    }
                },
                Framing::WebSocket(assembler) => match assembler.read(&mut self.reader) {
                    // A message is a line to the session, and JSON may be
                    // written over several: a line break is only ever
                    // whitespace between its tokens, so a space does as well.
                    Ok(ws::Message::Text(text)) => {
                        let line = text.replace(['\n', '\r'], " ").into_bytes();
                        if !line.iter().all(u8::is_ascii_whitespace) {
                            return Next::Message(line);
                        }
                    }
                    Ok(ws::Message::Ping(payload)) => {
                        let _ = out.frame(ws::PONG, &payload);
                    }
                    Ok(ws::Message::Pong) => {}
                    Ok(ws::Message::Close(_)) => {
                        let _ = out.frame(ws::CLOSE, &ws::close_payload(ws::close::NORMAL));
                        return Next::End;
                    }
                    Err(e) => {
                        let Some(code) = e.close_code() else {
                            return Next::End;
                        };
                        let _ = out.frame(ws::CLOSE, &ws::close_payload(code));
                        return Next::Failed(e.to_string());
                    }
                },
            }
        }
    }
}

/// An HTTP request's first line, when that is what `line` is: the method
/// and the target. A JSON line is never mistaken for one.
fn request_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    let http = parts.next().is_none()
        && version.starts_with("HTTP/")
        && !method.is_empty()
        && method.bytes().all(|b| b.is_ascii_uppercase());
    http.then_some((method, target))
}

/// The value of `name` in a request target's query string, as written (the
/// token needs no escaping).
fn query_param<'a>(target: &'a str, name: &str) -> Option<&'a str> {
    let (_, query) = target.split_once('?')?;
    let query = query.split('#').next().unwrap_or(query);
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find_map(|(k, v)| (k == name).then_some(v))
}

/// An HTTP response that ends the connection.
fn http_refusal(status: &str, extra_headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{body}",
        body.len()
    )
}

/// One count of something limited, given back when dropped.
struct Held<'a>(&'a AtomicUsize);

impl<'a> Held<'a> {
    /// Take one of `counter`'s `max`, if one is left.
    fn take(counter: &'a AtomicUsize, max: usize) -> Option<Self> {
        let mut n = counter.load(Ordering::SeqCst);
        while n < max {
            match counter.compare_exchange(n, n + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(Held(counter)),
                Err(now) => n = now,
            }
        }
        None
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The listener: what every connection's thread needs.
struct Listener {
    exe: PathBuf,
    child_args: Vec<String>,
    /// What a connection must present; `None` on a Unix socket, where
    /// being able to connect is the proof.
    token: Option<String>,
    /// The origins a browser's WebSocket may come from.
    origins: Vec<String>,
    max_sessions: usize,
    /// Connections let in and not yet gone.
    sessions: AtomicUsize,
    /// Connections in their handshake.
    pending: AtomicUsize,
    /// Connections so far, to tell them apart in the log.
    connections: AtomicU64,
}

impl Listener {
    /// Take connections from `next` for as long as the process lives, each
    /// on a thread of its own.
    fn accept(self: Arc<Self>, mut next: impl FnMut() -> io::Result<Stream>) -> ! {
        loop {
            let stream = match next() {
                Ok(stream) => stream,
                Err(e) => {
                    // Out of descriptors, a connection reset before it was
                    // taken: nothing to stop for.
                    eprintln!("atelier serve: accepting a connection: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            let listener = Arc::clone(&self);
            std::thread::spawn(move || {
                let id = listener.connections.fetch_add(1, Ordering::Relaxed) + 1;
                listener.connection(id, stream);
            });
        }
    }

    /// One connection, from its first byte to its last.
    fn connection(&self, id: u64, stream: Stream) {
        let Ok(read_half) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(read_half);
        let framing = match &self.token {
            None => Framing::Lines,
            Some(token) => {
                // Dropped without a word when too many are at it already.
                let Some(_pending) = Held::take(&self.pending, MAX_PENDING) else {
                    return;
                };
                stream.set_timeouts(Some(HANDSHAKE), Some(HANDSHAKE));
                match self.handshake(&mut reader, &stream, token) {
                    Ok(framing) => framing,
                    Err(why) => {
                        eprintln!("atelier serve: connection {id} refused: {why}");
                        stream.shutdown(std::net::Shutdown::Both);
                        return;
                    }
                }
            }
        };
        stream.set_timeouts(None, None);
        let websocket = matches!(framing, Framing::WebSocket(_));
        let out = Arc::new(Out {
            stream: Mutex::new(stream),
            websocket,
        });
        let Some(_session) = Held::take(&self.sessions, self.max_sessions) else {
            let why = format!(
                "too many sessions: {} are open, the most this listener serves \
                 (--max-sessions)",
                self.max_sessions
            );
            out.refuse(code::TOO_MANY_SESSIONS, &why);
            out.finish(ws::close::TRY_AGAIN_LATER);
            eprintln!("atelier serve: connection {id} refused: too many sessions");
            return;
        };
        let kind = if websocket { "websocket" } else { "lines" };
        eprintln!("atelier serve: connection {id} open ({kind})");
        self.pump(id, Inbound { reader, framing }, &out);
        eprintln!("atelier serve: connection {id} closed");
    }

    /// Let a TCP connection in, or say why not: its first line is either
    /// the token (`{"token": "…"}`, then JSON-RPC lines) or an HTTP request
    /// to upgrade to WebSocket.
    fn handshake(
        &self,
        reader: &mut BufReader<Stream>,
        stream: &Stream,
        token: &str,
    ) -> Result<Framing, String> {
        let first = match read_line(reader, MAX_HEAD_LINE) {
            Ok(Some(line)) => line,
            Ok(None) | Err(LineError::Io) => return Err("it said nothing".into()),
            Err(LineError::TooLong) => return Err("its first line is too long".into()),
        };
        let first = String::from_utf8_lossy(&first);
        if let Some((method, target)) = request_line(&first) {
            return self.upgrade(reader, stream, token, method, target);
        }
        let presented = serde_json::from_str::<Value>(&first)
            .ok()
            .and_then(|v| v.get("token").and_then(Value::as_str).map(str::to_string));
        match presented {
            Some(presented) if same_token(&presented, token) => Ok(Framing::Lines),
            _ => {
                let why = "unauthorized: the first line must be {\"token\": \"…\"} with this \
                           listener's token";
                let _ = stream.put(format!("{}\n", error(code::UNAUTHORIZED, why)).as_bytes());
                Err("wrong or missing token".into())
            }
        }
    }

    /// The WebSocket opening handshake (RFC 6455 §4.2), with the two checks
    /// of our own: where the page comes from, and the token.
    fn upgrade(
        &self,
        reader: &mut BufReader<Stream>,
        stream: &Stream,
        token: &str,
        method: &str,
        target: &str,
    ) -> Result<Framing, String> {
        let refuse = |status: &str, extra: &str, why: &str| -> Result<Framing, String> {
            let response = http_refusal(status, extra, &format!("{why}\n"));
            let _ = stream.put(response.as_bytes());
            Err(format!("{status}: {why}"))
        };
        let mut headers: Vec<(String, String)> = Vec::new();
        loop {
            let line = match read_line(reader, MAX_HEAD_LINE) {
                Ok(Some(line)) => line,
                Ok(None) | Err(LineError::Io) => return Err("the request was cut short".into()),
                Err(LineError::TooLong) => {
                    return refuse("431 Request Header Fields Too Large", "", "header too long");
                }
            };
            if line.is_empty() {
                break;
            }
            if headers.len() == MAX_HEADERS {
                return refuse(
                    "431 Request Header Fields Too Large",
                    "",
                    "too many headers",
                );
            }
            let line = String::from_utf8_lossy(&line);
            let Some((name, value)) = line.split_once(':') else {
                return refuse("400 Bad Request", "", "malformed header");
            };
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
        // Every value of a header, across its repetitions and its commas.
        let values = |name: &'static str| {
            headers
                .iter()
                .filter(move |(n, _)| n.as_str() == name)
                .flat_map(|(_, v)| v.split(','))
                .map(str::trim)
        };
        let upgrading = method == "GET"
            && values("upgrade").any(|v| v.eq_ignore_ascii_case("websocket"))
            && values("connection").any(|v| v.eq_ignore_ascii_case("upgrade"));
        if !upgrading {
            return refuse(
                "426 Upgrade Required",
                "Upgrade: websocket\r\n",
                "this port speaks WebSocket and line-delimited JSON-RPC, not HTTP",
            );
        }
        if !values("sec-websocket-version").any(|v| v == "13") {
            return refuse(
                "426 Upgrade Required",
                "Sec-WebSocket-Version: 13\r\n",
                "WebSocket version 13 only",
            );
        }
        let mut keys = headers.iter().filter(|(n, _)| n == "sec-websocket-key");
        let (Some((_, key)), None) = (keys.next(), keys.next()) else {
            return refuse("400 Bad Request", "", "one Sec-WebSocket-Key is needed");
        };
        // A browser always says where the page is from, and a page cannot
        // change it: without this any site the user visits could connect.
        // (The token stands between it and a session all the same.)
        let mut origins = headers.iter().filter(|(n, _)| n == "origin");
        match (origins.next(), origins.next()) {
            (None, _) => {}
            (Some((_, from)), None)
                if origin(from).is_ok_and(|from| self.origins.contains(&from)) => {}
            _ => {
                return refuse(
                    "403 Forbidden",
                    "",
                    "this origin is not allowed (see --allow-origin)",
                );
            }
        }
        let presented = query_param(target, "token")
            .or_else(|| values("sec-websocket-protocol").find_map(|p| p.strip_prefix("token.")));
        if !presented.is_some_and(|presented| same_token(presented, token)) {
            return refuse("401 Unauthorized", "", "wrong or missing token");
        }
        // A client that offers subprotocols expects one back — never the
        // one that carries the token.
        let protocol = if values("sec-websocket-protocol").any(|p| p == "atelier") {
            "Sec-WebSocket-Protocol: atelier\r\n"
        } else {
            ""
        };
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Accept: {}\r\n{protocol}\r\n",
            ws::accept_key(key)
        );
        stream.put(response.as_bytes()).map_err(|e| e.to_string())?;
        Ok(Framing::WebSocket(ws::Assembler::new(
            ws::Peer::Client,
            ws::MAX_MESSAGE,
        )))
    }

    /// Carry messages between a connection and a session of its own until
    /// either ends. The session's process is started by the first message.
    fn pump(&self, id: u64, mut inbound: Inbound, out: &Arc<Out>) {
        let mut session: Option<(ChildStdin, Arc<Mutex<Child>>)> = None;
        loop {
            let mut line = match inbound.next(out) {
                Next::Message(line) => line,
                Next::End => break,
                Next::Failed(why) => {
                    eprintln!("atelier serve: connection {id} cut off: {why}");
                    break;
                }
            };
            if session.is_none() {
                match self.start_session(id, out) {
                    Ok(started) => session = Some(started),
                    Err(e) => {
                        eprintln!("atelier serve: connection {id}: starting a session: {e}");
                        let why = format!("the session could not be started: {e}");
                        out.refuse(rpc::code::INTERNAL_ERROR, &why);
                        break;
                    }
                }
            }
            let (stdin, _) = session.as_mut().expect("started above");
            line.push(b'\n');
            // The session is gone (it was asked to shut down, or it died):
            // its output thread is closing the connection.
            if stdin.write_all(&line).and_then(|()| stdin.flush()).is_err() {
                break;
            }
        }
        match session {
            // The end of its stdin is the session's cue to leave, as when
            // the program that launched a `serve` goes away.
            Some((stdin, child)) => {
                drop(stdin);
                if wait(&child, REAP).is_none() {
                    eprintln!("atelier serve: connection {id}: its session did not leave; killed");
                    let mut child = lock(&child);
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            None => out.finish(ws::close::NORMAL),
        }
    }

    /// Start `atelier serve` for a connection, and the thread that forwards
    /// what it writes. Returns its stdin, and the process to wait for.
    fn start_session(
        &self,
        id: u64,
        out: &Arc<Out>,
    ) -> io::Result<(ChildStdin, Arc<Mutex<Child>>)> {
        let mut child = Command::new(&self.exe)
            .args(&self.child_args)
            // The session's tools run commands; the token is not theirs.
            .env_remove(ENV_TOKEN)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let child = Arc::new(Mutex::new(child));
        let (out, process) = (Arc::clone(out), Arc::clone(&child));
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let text = String::from_utf8_lossy(&line);
                let text = text.trim_end_matches(['\n', '\r']);
                // A failed write is the client gone: the reading side sees
                // it too, and ends the session.
                if !text.is_empty() && out.send(text).is_err() {
                    break;
                }
            }
            // A session that ends well said so itself (the answer to
            // `shutdown`); one that does not leaves the client guessing.
            if let Some(status) = wait(&process, Duration::from_secs(2))
                && !status.success()
            {
                eprintln!("atelier serve: connection {id}: its session ended with {status}");
                let why = format!("the session ended unexpectedly ({status})");
                out.refuse(rpc::code::INTERNAL_ERROR, &why);
            }
            out.finish(ws::close::NORMAL);
        });
        Ok((stdin, child))
    }
}

/// How `child` ended, if it does within `patience`.
fn wait(child: &Mutex<Child>, patience: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + patience;
    loop {
        if let Ok(Some(status)) = lock(child).try_wait() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_takes_loopback_addresses_and_socket_paths_only() {
        let tcp = |s: &str| Where::Tcp(s.parse().unwrap());
        assert_eq!(Where::parse("127.0.0.1:8080"), Ok(tcp("127.0.0.1:8080")));
        assert_eq!(Where::parse("127.0.0.1:0"), Ok(tcp("127.0.0.1:0")));
        assert_eq!(Where::parse("[::1]:9"), Ok(tcp("[::1]:9")));
        assert_eq!(Where::parse("localhost:7"), Ok(tcp("127.0.0.1:7")));
        for open in [
            "0.0.0.0:8080",
            "192.168.1.4:80",
            "[::]:80",
            "[2001:db8::1]:80",
            // Loopback by another name is not `is_loopback`, and stays out.
            "[::ffff:127.0.0.1]:80",
        ] {
            let e = Where::parse(open).unwrap_err();
            assert!(e.contains("not a loopback address"), "{open}: {e}");
        }
        for vague in ["example.com:80", "8080", "sock", "localhost", "localhost:x"] {
            assert!(Where::parse(vague).is_err(), "{vague}");
        }
        assert!(Where::parse("unix:").is_err());
        if cfg!(unixlike) {
            let unix = |s: &str| Ok(Where::Unix(PathBuf::from(s)));
            assert_eq!(Where::parse("unix:s.sock"), unix("s.sock"));
            assert_eq!(Where::parse("unix:127.0.0.1:80"), unix("127.0.0.1:80"));
            assert_eq!(Where::parse("/run/me/a.sock"), unix("/run/me/a.sock"));
            assert_eq!(Where::parse("./a.sock"), unix("./a.sock"));
        } else {
            assert!(Where::parse("unix:s.sock").is_err());
        }
    }

    #[test]
    fn origins_are_compared_in_one_form() {
        assert_eq!(
            origin("HTTP://LocalHost:5173/").unwrap(),
            "http://localhost:5173"
        );
        assert_eq!(origin("null").unwrap(), "null");
        assert_eq!(origin("app://ui").unwrap(), "app://ui");
        for bad in [
            "",
            "*",
            "localhost:5173",
            "http://",
            "http://a/b",
            "http://*",
        ] {
            assert!(origin(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_session_is_started_without_the_listeners_flags() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            child_args(&args(&[
                "serve",
                "--listen",
                "127.0.0.1:0",
                "--mode",
                "plan",
                "--allow-origin=http://a",
                "--allow-origin",
                "http://b",
                "--claude",
                "--max-sessions=2",
                "--approval-timeout=30",
                "--listen=x/y",
            ])),
            args(&[
                "serve",
                "--mode",
                "plan",
                "--claude",
                "--approval-timeout=30"
            ])
        );
    }

    #[test]
    fn a_request_line_is_told_from_a_token_line() {
        assert_eq!(
            request_line("GET /?token=abc HTTP/1.1"),
            Some(("GET", "/?token=abc"))
        );
        assert_eq!(request_line("POST / HTTP/1.0"), Some(("POST", "/")));
        for not in [
            "{\"token\": \"GET / HTTP/1.1\"}",
            "{\"token\":\"x\"}",
            "get / HTTP/1.1",
            "GET /",
            "GET / HTTP/1.1 more",
            "",
        ] {
            assert_eq!(request_line(not), None, "{not}");
        }
    }

    #[test]
    fn the_token_is_found_in_the_query() {
        assert_eq!(query_param("/?token=abc", "token"), Some("abc"));
        assert_eq!(query_param("/ws?a=1&token=abc&b=2#f", "token"), Some("abc"));
        assert_eq!(query_param("/?nottoken=abc", "token"), None);
        assert_eq!(query_param("/token=abc", "token"), None);
    }

    #[test]
    fn tokens_are_compared_whole() {
        assert!(same_token("abc", "abc"));
        assert!(!same_token("abc", "abd"));
        assert!(!same_token("ab", "abc"));
        assert!(!same_token("", "abc"));
    }

    #[test]
    fn a_line_is_read_up_to_its_limit() {
        let read = |bytes: &[u8], limit| {
            let mut r = bytes;
            let mut lines = Vec::new();
            loop {
                match read_line(&mut r, limit) {
                    Ok(Some(line)) => lines.push(String::from_utf8(line).unwrap()),
                    Ok(None) => return Ok(lines),
                    Err(LineError::TooLong) => return Err(lines),
                    Err(LineError::Io) => unreachable!(),
                }
            }
        };
        assert_eq!(
            read(b"one\r\n\ntwo\nlast", 10),
            Ok(vec!["one".into(), "".into(), "two".into(), "last".into()])
        );
        // The limit is the line's, its end not counted.
        assert_eq!(read(b"12345\n", 5), Ok(vec!["12345".to_string()]));
        assert_eq!(read(b"12345", 5), Ok(vec!["12345".to_string()]));
        assert_eq!(read(b"12345\r\n", 5), Ok(vec!["12345".to_string()]));
        assert_eq!(read(b"123456", 5), Err(vec![]));
        assert_eq!(read(b"ok\n123456\n", 5), Err(vec!["ok".to_string()]));
    }

    #[test]
    fn slots_are_given_back() {
        let counter = AtomicUsize::new(0);
        let a = Held::take(&counter, 2).unwrap();
        let _b = Held::take(&counter, 2).unwrap();
        assert!(Held::take(&counter, 2).is_none());
        drop(a);
        assert!(Held::take(&counter, 2).is_some());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
