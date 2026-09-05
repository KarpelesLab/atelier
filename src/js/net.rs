//! The opt-in, mediated network host for the `node` tool.
//!
//! Installed **only** when the call sets `network: true` (which also makes the
//! call require approval — see [`crate::js::NodeTool::requires_approval`]). It
//! registers a single low-level global, `__atelier_http_request`, backed by the
//! blocking [`rsurl`] client; the network bootstrap (run before user code)
//! assembles the friendly `fetch` / `httpGet` / `httpRequest` globals on top of
//! it. Like `fs`, every call is **synchronous** — no promises, no event loop.
//!
//! The boundary between JS and Rust is kept all-scalar: the JS wrappers pass the
//! method, url, optional body, and a JSON-encoded headers object as strings, and
//! this function returns a plain result object `{ status, ok, body, headers }`.

use kataan::{Ctx, Interp, NanBox};
use serde_json::Value;

/// Read argument `i` as a `NanBox`, defaulting to `undefined`.
fn arg(cx: &mut Ctx, args: &[NanBox], i: usize) -> NanBox {
    args.get(i).copied().unwrap_or_else(|| cx.undefined())
}

/// Register the low-level `__atelier_http_request(method, url, body, headersJson)`
/// global. The network bootstrap wraps it into `fetch` / `httpGet` /
/// `httpRequest`.
pub fn install(interp: &mut Interp) {
    interp.register_global_fn("__atelier_http_request", 4, move |cx, _this, args| {
        let method_nb = arg(cx, args, 0);
        let method = cx.to_string(method_nb)?;
        let url_nb = arg(cx, args, 1);
        let url = cx.to_string(url_nb)?;

        // A missing/undefined body means "no body" (e.g. a GET); anything else
        // is sent verbatim as its string form.
        let body_nb = arg(cx, args, 2);
        let body = if body_nb.type_of() == "undefined" {
            None
        } else {
            Some(cx.to_string(body_nb)?)
        };

        // Headers arrive as a JSON object string (or "" for none).
        let headers_nb = arg(cx, args, 3);
        let headers_json = cx.to_string(headers_nb)?;

        let mut req = rsurl::Request::new(&method, &url)
            .map_err(|e| cx.error(&format!("fetch: invalid request: {e}")))?;

        if !headers_json.is_empty() {
            let parsed: Value = serde_json::from_str(&headers_json)
                .map_err(|e| cx.error(&format!("fetch: bad headers: {e}")))?;
            if let Value::Object(map) = parsed {
                for (name, value) in map {
                    let v = match value {
                        Value::String(s) => s,
                        other => other.to_string(),
                    };
                    req = req.header(&name, &v);
                }
            }
        }

        if let Some(b) = body {
            req = req.body(b.into_bytes());
        }

        let resp = req
            .send()
            .map_err(|e| cx.error(&format!("fetch: request failed: {e}")))?;

        let status = resp.status;
        // Decode using the response's declared charset; fall back to a lossy
        // UTF-8 decode of the raw bytes for a body that can't be decoded.
        let body_text = resp
            .text()
            .unwrap_or_else(|_| String::from_utf8_lossy(&resp.body).into_owned());

        // Assemble the `{ status, ok, body, headers }` result object. Scalars are
        // built before each `set_property` because the getters borrow `cx`
        // immutably while `set_property` borrows it mutably.
        let obj = cx.new_object();
        let status_nb = cx.number(status as f64);
        cx.set_property(obj, "status", status_nb)?;
        let ok_nb = cx.boolean((200..300).contains(&status));
        cx.set_property(obj, "ok", ok_nb)?;
        let body_val = cx.string(&body_text);
        cx.set_property(obj, "body", body_val)?;

        let hdr_obj = cx.new_object();
        for (name, value) in &resp.headers {
            let v = cx.string(value);
            cx.set_property(hdr_obj, name, v)?;
        }
        cx.set_property(obj, "headers", hdr_obj)?;

        Ok(obj)
    });
}
