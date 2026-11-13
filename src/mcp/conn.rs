//! JSON-RPC 2.0 connection to an MCP server over stdio (newline-delimited
//! JSON on the child's stdin/stdout).
//!
//! The server's stdout is read by a thread of its own, which hands each line
//! over a channel. That is what lets a request give up: waiting on the
//! channel can time out, where a blocking read of the pipe could not, and a
//! server that never answers would hold the session forever. A request that
//! times out leaves the connection usable — its answer, should it still
//! come, carries an id nobody is waiting for and is skipped like any other
//! stray line.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::Child;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::jsonrpc::JsonRpc;

/// A live JSON-RPC connection to an MCP server child process.
///
/// Owns the child so it stays alive (and keeps running) for as long as this
/// connection — and therefore any [`McpTool`](crate::mcp::tool::McpTool)
/// holding an `Arc<Mutex<Conn>>` — exists.
pub(crate) struct Conn {
    /// `None` only in tests, which talk to an in-process fake over pipes.
    child: Option<Child>,
    writer: Box<dyn Write + Send>,
    /// Non-empty lines of the server's output; disconnected once it closes.
    lines: Receiver<String>,
    next_id: u64,
    /// How long a request waits for its answer.
    timeout: Duration,
}

impl Conn {
    /// A connection writing requests to `writer` and reading answers from
    /// `reader`, each request waiting up to `timeout`.
    pub(crate) fn new(
        child: Option<Child>,
        writer: impl Write + Send + 'static,
        reader: impl Read + Send + 'static,
        timeout: Duration,
    ) -> Self {
        let (tx, lines) = channel();
        // Ends when the server closes its output (it exited, or `Drop`
        // killed it) or when the connection — the receiver — is gone.
        thread::spawn(move || {
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                let line = line.trim();
                if !line.is_empty() && tx.send(line.to_string()).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            writer: Box::new(writer),
            lines,
            next_id: 0,
            timeout,
        }
    }

    /// Change how long requests wait from here on: the handshake and a tool
    /// call are given different patience.
    pub(crate) fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Send a JSON-RPC request and block until the matching response arrives
    /// or the timeout passes. Notifications and responses to other
    /// (unexpected) ids are skipped.
    pub(crate) fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_line(&msg)
            .with_context(|| format!("writing `{method}` request to MCP server"))?;

        let deadline = Instant::now() + self.timeout;
        loop {
            let line = self
                .read_line(deadline)
                .with_context(|| format!("reading response to `{method}` from MCP server"));
            let line = match line {
                Ok(Some(line)) => line,
                Ok(None) => {
                    // Tell the server to stop working on it; whatever it
                    // still sends for this id is skipped by later requests.
                    let cancel = json!({ "requestId": id, "reason": "timed out" });
                    let _ = self.notify("notifications/cancelled", cancel);
                    bail!(
                        "MCP server did not answer `{method}` within {}s",
                        self.timeout.as_secs()
                    );
                }
                Err(e) => return Err(e),
            };
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                // Tolerate stray non-JSON-RPC output on stdout from a
                // misbehaving server; keep reading for the real response.
                Err(_) => continue,
            };
            let Some(resp_id) = v.get("id").and_then(Value::as_u64) else {
                // A notification, or a request from the server — not what
                // we're waiting for.
                continue;
            };
            if resp_id != id {
                continue;
            }
            if let Some(err) = v.get("error") {
                let msg = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                bail!("MCP server returned an error for `{method}`: {msg} ({err})");
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Send a JSON-RPC notification (no response expected).
    pub(crate) fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_line(&msg)
            .with_context(|| format!("writing `{method}` notification to MCP server"))
    }

    fn write_line(&mut self, v: &Value) -> Result<()> {
        let mut s = serde_json::to_string(v)?;
        s.push('\n');
        self.writer.write_all(s.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    /// The next line of the server's output, or `None` if `deadline` passes
    /// first. The output closing means the server exited: that is reported
    /// as such, with its exit status when available.
    fn read_line(&mut self, deadline: Instant) -> Result<Option<String>> {
        let wait = deadline.saturating_duration_since(Instant::now());
        match self.lines.recv_timeout(wait) {
            Ok(line) => Ok(Some(line)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                let status = self
                    .child
                    .as_mut()
                    .and_then(|c| c.try_wait().ok().flatten())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "still running (stdout closed unexpectedly)".to_string());
                bail!("MCP server process closed its stdout (exit status: {status})");
            }
        }
    }
}

impl JsonRpc for Conn {
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        // Fully-qualified so this calls the inherent method above, not
        // itself (see the trait's doc comment for why both exist).
        Conn::request(self, method, params)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        Conn::notify(self, method, params)
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        // Best-effort cleanup: don't leave the child running once every tool
        // referencing this connection has been dropped.
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::pipe;

    /// A connection to an in-process "server": a thread that reads request
    /// lines and answers each through `answer`, which may sleep (a slow
    /// server) or return `None` (one that never answers).
    fn fake(timeout: Duration, answer: impl Fn(&Value) -> Option<Value> + Send + 'static) -> Conn {
        let (server_in, client_out) = pipe().unwrap();
        let (client_in, mut server_out) = pipe().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(server_in).lines() {
                let req: Value = serde_json::from_str(&line.unwrap()).unwrap();
                if let Some(result) = answer(&req) {
                    let resp = json!({ "jsonrpc": "2.0", "id": req["id"], "result": result });
                    if writeln!(server_out, "{resp}").is_err() {
                        break;
                    }
                }
            }
        });
        Conn::new(None, client_out, client_in, timeout)
    }

    #[test]
    fn a_request_that_is_not_answered_times_out() {
        let mut conn = fake(Duration::from_millis(100), |_| None);
        let started = Instant::now();
        let err = conn.request("initialize", json!({})).unwrap_err();
        assert!(
            err.to_string()
                .contains("did not answer `initialize` within")
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_connection_survives_a_timed_out_call() {
        // `slow` answers after the timeout; the late answer must not be
        // taken for the next request's.
        let mut conn = fake(Duration::from_millis(150), |req| {
            match req["method"].as_str()? {
                "slow" => {
                    thread::sleep(Duration::from_millis(400));
                    Some(json!("late"))
                }
                "fast" => Some(json!("on time")),
                // `notifications/cancelled`, sent when `slow` timed out.
                _ => None,
            }
        });
        assert!(conn.request("slow", json!({})).is_err());
        conn.set_timeout(Duration::from_secs(5));
        assert_eq!(conn.request("fast", json!({})).unwrap(), "on time");
    }

    #[test]
    fn a_closed_output_is_reported_as_the_server_exiting() {
        let (_server_in, client_out) = pipe().unwrap();
        let (client_in, server_out) = pipe().unwrap();
        drop(server_out);
        let mut conn = Conn::new(None, client_out, client_in, Duration::from_secs(5));
        let err = conn.request("initialize", json!({})).unwrap_err();
        assert!(format!("{err:#}").contains("closed its stdout"));
    }
}
