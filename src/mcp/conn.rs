//! JSON-RPC 2.0 connection to an MCP server over stdio (newline-delimited
//! JSON on the child's stdin/stdout).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// A live JSON-RPC connection to an MCP server child process.
///
/// Owns the child so it stays alive (and keeps running) for as long as this
/// connection — and therefore any [`McpTool`](crate::mcp::tool::McpTool)
/// holding an `Arc<Mutex<Conn>>` — exists.
pub(crate) struct Conn {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: u64,
}

impl Conn {
    pub(crate) fn new(child: Child, stdin: ChildStdin, stdout: ChildStdout) -> Self {
        Self {
            child,
            stdin,
            reader: BufReader::new(stdout),
            next_id: 0,
        }
    }

    /// Send a JSON-RPC request and block until the matching response arrives.
    /// Notifications and responses to other (unexpected) ids are skipped.
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

        loop {
            let line = self
                .read_line()
                .with_context(|| format!("reading response to `{method}` from MCP server"))?;
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
        self.stdin.write_all(s.as_bytes())?;
        self.stdin.flush()?;
        Ok(())
    }

    /// Read one non-empty line from the server's stdout. Treats EOF as the
    /// server having exited and reports that clearly, including its exit
    /// status when available.
    fn read_line(&mut self) -> Result<String> {
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line)?;
            if n == 0 {
                let status = self
                    .child
                    .try_wait()
                    .ok()
                    .flatten()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "still running (stdout closed unexpectedly)".to_string());
                bail!("MCP server process closed its stdout (exit status: {status})");
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            return Ok(trimmed.to_string());
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        // Best-effort cleanup: don't leave the child running once every tool
        // referencing this connection has been dropped.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
