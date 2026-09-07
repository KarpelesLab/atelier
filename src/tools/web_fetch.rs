//! The `web_fetch` tool: fetch a URL over HTTP(S).

use anyhow::{Context, Result, bail};
use rsurl::Request;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec};

/// Cap on the decoded response body, so a large page doesn't blow up the
/// context window.
const MAX_BODY_BYTES: usize = 20 * 1024;

pub struct WebFetchTool;

impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    // Leave the default (`true`): this reaches out over the network and is
    // not confined to the project directory, so it needs approval like `bash`.

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "Fetch a URL over HTTP(S) and return its status and body, decoded \
                as text and truncated to about 20KB."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The http:// or https:// URL to fetch."
                    },
                    "method": {
                        "type": "string",
                        "description": "HTTP method to use (default: GET)."
                    }
                },
                "required": ["url"]
            }),
        }
    }

    fn call(&self, _ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required argument 'url'"))?;
        let method = args.get("method").and_then(Value::as_str).unwrap_or("GET");

        if !(url.starts_with("http://") || url.starts_with("https://")) {
            bail!("url {url:?} must start with http:// or https://");
        }

        let request = Request::new(method, url)
            .with_context(|| format!("building {method} request for {url:?}"))?
            .follow_redirects(true);
        let response = request
            .send()
            .with_context(|| format!("fetching {url:?}"))?;

        let status_line = format!("HTTP {} {}", response.status, response.reason);
        let mut body = response
            .text()
            .unwrap_or_else(|_| String::from_utf8_lossy(&response.body).into_owned());

        let mut truncated = false;
        if body.len() > MAX_BODY_BYTES {
            let mut cut = MAX_BODY_BYTES;
            while !body.is_char_boundary(cut) {
                cut -= 1;
            }
            body.truncate(cut);
            truncated = true;
        }

        let mut out = format!("{status_line}\n\n{body}");
        if truncated {
            out.push_str(&format!("\n[truncated to {MAX_BODY_BYTES} bytes]"));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::FileState;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;

    fn project_root() -> PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn fetches_body_from_local_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = "hello from test server";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        });

        let url = format!("http://{addr}/");
        let root = project_root();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let out = WebFetchTool.call(&mut ctx, json!({"url": url})).unwrap();
        server.join().unwrap();

        assert!(out.starts_with("HTTP 200"));
        assert!(out.contains("hello from test server"));
    }

    #[test]
    fn malformed_url_errors() {
        let root = project_root();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        assert!(
            WebFetchTool
                .call(&mut ctx, json!({"url": "not a url"}))
                .is_err()
        );
    }

    #[test]
    fn missing_url_errors() {
        let root = project_root();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        assert!(WebFetchTool.call(&mut ctx, json!({})).is_err());
    }

    #[test]
    fn connection_failure_errors() {
        // Bind then immediately drop, so the port is (almost certainly) refused.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let url = format!("http://{addr}/");
        let root = project_root();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        assert!(WebFetchTool.call(&mut ctx, json!({"url": url})).is_err());
    }
}
