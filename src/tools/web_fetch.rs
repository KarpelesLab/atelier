//! The `web_fetch` tool: fetch a URL over HTTP(S).
//!
//! HTML is converted to readable text (headings, list items and links kept,
//! scripts and styles dropped) so a page's content fits where its markup
//! wouldn't; long results are paged with `offset`/`max_chars`.

use anyhow::{Context, Result, bail};
use rsurl::Request;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec};

/// Default cap on the returned text (in characters), so a large page doesn't
/// blow up the context window; `offset` pages through the rest.
const DEFAULT_MAX_CHARS: usize = 30_000;

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
            description: "Fetch a URL over HTTP(S) and return its status and body as text. \
                HTML pages are converted to readable text (links kept as [text](url)); pass \
                raw=true for the original markup. Long results are cut at max_chars \
                (default 30000); fetch again with `offset` to read further."
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
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Character offset to start from, to page through a long result (default 0)."
                    },
                    "max_chars": {
                        "type": "integer",
                        "description": "Maximum characters to return (default 30000)."
                    },
                    "raw": {
                        "type": "boolean",
                        "description": "Return HTML as-is instead of converting it to text (default false)."
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
        let body = response
            .text()
            .unwrap_or_else(|_| String::from_utf8_lossy(&response.body).into_owned());
        let is_html = response
            .header("content-type")
            .is_some_and(|t| t.to_ascii_lowercase().contains("html"));
        let raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);
        let text = if is_html && !raw {
            html_to_text(&body)
        } else {
            body
        };

        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let max = args
            .get("max_chars")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_MAX_CHARS, |n| n.max(1) as usize);
        let total = text.chars().count();
        let page: String = text.chars().skip(offset).take(max).collect();
        let end = (offset + max).min(total);

        let mut out = status_line;
        if end < total || offset > 0 {
            out.push_str(&format!(" [characters {offset}-{end} of {total}]"));
        }
        out.push_str("\n\n");
        out.push_str(&page);
        if end < total {
            out.push_str(&format!(
                "\n[{} more characters: fetch again with offset {end}]",
                total - end
            ));
        }
        Ok(out)
    }
}

/// Convert HTML to readable text: drop scripts, styles and markup; turn
/// block elements into line breaks, headings into `#` lines, list items into
/// `- ` lines and links into `[text](url)`; decode entities; squeeze blank
/// space.
fn html_to_text(html: &str) -> String {
    const SKIP: &[&str] = &["script", "style", "noscript", "svg", "head", "template"];
    const BLOCK: &[&str] = &[
        "p",
        "div",
        "br",
        "tr",
        "section",
        "article",
        "header",
        "footer",
        "nav",
        "main",
        "aside",
        "table",
        "ul",
        "ol",
        "pre",
        "blockquote",
        "hr",
        "dl",
        "dt",
        "dd",
        "figure",
        "form",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "li",
        "title",
        "body",
    ];
    let mut out = String::new();
    let mut rest = html;
    let mut href: Vec<Option<String>> = Vec::new();
    while let Some(lt) = rest.find('<') {
        out.push_str(&decode_entities(&rest[..lt]));
        rest = &rest[lt..];
        // Comments.
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.find("-->").map_or("", |e| &after[e + 3..]);
            continue;
        }
        let Some(gt) = rest.find('>') else {
            break;
        };
        let tag = &rest[1..gt];
        rest = &rest[gt + 1..];
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if !closing && SKIP.contains(&name.as_str()) {
            let end = format!("</{name}");
            let lower = rest.to_ascii_lowercase();
            rest = lower.find(&end).map_or("", |e| {
                let r = &rest[e..];
                r.find('>').map_or("", |g| &r[g + 1..])
            });
            continue;
        }
        match (name.as_str(), closing) {
            ("a", false) => {
                href.push(attr(tag, "href"));
                out.push('[');
            }
            ("a", true) => match href.pop().flatten() {
                Some(url) => out.push_str(&format!("]({url})")),
                None => out.push(']'),
            },
            ("li", false) => out.push_str("\n- "),
            // Items stay tight: the next one starts its own line.
            ("li", true) => {}
            (h, false)
                if h.len() == 2 && h.starts_with('h') && h.as_bytes()[1].is_ascii_digit() =>
            {
                let level = (h.as_bytes()[1] - b'0') as usize;
                out.push_str(&format!("\n\n{} ", "#".repeat(level)));
            }
            ("td" | "th", _) => out.push(' '),
            (n, _) if BLOCK.contains(&n) => out.push('\n'),
            _ => {}
        }
    }
    out.push_str(&decode_entities(rest));
    squeeze(&out)
}

/// The value of attribute `name` in a tag's text.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        // Whole attribute name only.
        if at > 0 && !lower.as_bytes()[at - 1].is_ascii_whitespace() {
            continue;
        }
        let rest = tag[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let value = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => rest[1..].split(q).next().unwrap_or(""),
            _ => rest
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()
                .unwrap_or(""),
        };
        return Some(decode_entities(value));
    }
    None
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let entity = rest[1..]
            .find(';')
            .filter(|&e| e <= 10)
            .map(|e| &rest[1..=e]);
        let decoded = entity.and_then(|e| match e {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => {
                let n = e.strip_prefix('#')?;
                let code = match n.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => n.parse().ok()?,
                };
                char::from_u32(code)
            }
        });
        match (entity, decoded) {
            (Some(e), Some(c)) => {
                out.push(c);
                rest = &rest[e.len() + 2..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Collapse runs of spaces within lines and of blank lines between them.
fn squeeze(s: &str) -> String {
    let mut out = String::new();
    let mut blank = 0;
    for line in s.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        out.push_str(&line);
    }
    out
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
            approved: false,
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
            approved: false,
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
            approved: false,
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
            approved: false,
        };
        assert!(WebFetchTool.call(&mut ctx, json!({"url": url})).is_err());
    }

    #[test]
    fn html_becomes_readable_text() {
        let html = r#"<html><head><title>T</title><style>p{color:red}</style></head>
            <body><h1>Main &amp; Title</h1><script>var x = "<p>no</p>";</script>
            <p>Hello <b>world</b>, see <a href="https://e.x/a?b=1&amp;c=2">the docs</a>.</p>
            <!-- hidden --><ul><li>one</li><li>two&#33;</li></ul><p>caf&#xE9;&nbsp;ok</p></body></html>"#;
        assert_eq!(
            html_to_text(html),
            "# Main & Title\n\nHello world, see [the docs](https://e.x/a?b=1&c=2).\n\n- one\n- two!\n\ncafé ok"
        );
    }

    #[test]
    fn pages_long_results() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = "0123456789";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let root = project_root();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let out = WebFetchTool
            .call(
                &mut ctx,
                json!({"url": format!("http://{addr}/"), "offset": 3, "max_chars": 4}),
            )
            .unwrap();
        server.join().unwrap();
        assert!(out.contains("[characters 3-7 of 10]"), "{out}");
        assert!(out.contains("\n\n3456\n"), "{out}");
        assert!(out.contains("fetch again with offset 7"), "{out}");
    }
}
