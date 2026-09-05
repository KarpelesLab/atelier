//! Glue for running user JS on an [`Interp`]: the bootstrap that assembles the
//! host objects, error rendering, and output truncation.

use std::path::PathBuf;

use kataan::Interp;
use kataan::interrupt::Interrupt;
use kataan::nbexec::ExecError;
use kataan::parser::Parser;

/// JS run before the user's code. It gathers the mangled `__atelier_*` globals
/// (registered from Rust) into the `fs` / `console` objects the script sees.
///
/// The methods are assembled here — in JS — rather than stashed as `NanBox`
/// handles on the Rust side, because kataan has a **moving** garbage collector:
/// a `NanBox` must not be held in Rust across separate `interp.run` calls.
///
/// Plain (not `globalThis.x =`) assignment is deliberate: kataan installs a
/// builtin `console` as a global *scope binding*, which a bare `console`
/// reference resolves to in preference to a `globalThis` property — so we must
/// reassign the binding itself to make the script see our capturing console.
pub const BOOTSTRAP: &str = r#"
fs = {
  readFile: __atelier_fs_readFile,
  writeFile: __atelier_fs_writeFile,
  readdir: __atelier_fs_readdir,
  exists: __atelier_fs_exists,
  mkdir: __atelier_fs_mkdir,
};
console = {
  log: __atelier_console_log,
  error: __atelier_console_error,
};
"#;

/// JS run **only** when the call opts into network (`network: true`), after
/// [`BOOTSTRAP`]. It wraps the low-level `__atelier_http_request` (registered
/// from Rust in [`super::net`]) into the friendly, synchronous network globals a
/// script sees:
///
/// - `fetch(url[, options]) -> { status, ok, body, headers }` — `options` may
///   carry `{ method, headers, body }`.
/// - `httpRequest({ method, url, headers, body }) -> { status, ok, body, headers }`
///   (also accepts a bare url string).
/// - `httpGet(url) -> string` — the response body.
///
/// Bare (not `globalThis.x =`) assignment mirrors [`BOOTSTRAP`]'s rationale.
pub const NET_BOOTSTRAP: &str = r#"
httpRequest = function (req) {
  if (typeof req === "string") req = { url: req };
  req = req || {};
  var url = req.url;
  if (!url) throw new TypeError("httpRequest: missing url");
  var method = req.method || "GET";
  var body = (req.body === undefined || req.body === null) ? undefined : String(req.body);
  var headersJson = req.headers ? JSON.stringify(req.headers) : "";
  return __atelier_http_request(method, url, body, headersJson);
};
httpGet = function (url) {
  return httpRequest({ url: url, method: "GET" }).body;
};
fetch = function (url, options) {
  options = options || {};
  return httpRequest({ url: url, method: options.method || "GET", headers: options.headers, body: options.body });
};
"#;

/// Cap on the returned output; longer output is truncated with a note.
const MAX_OUTPUT: usize = 10 * 1024;

/// Build an interpreter, install the mediated host (`fs` + `console`, plus the
/// network host when `network` is set), run the bootstrap(s) and the user's
/// program, and return the captured output as a single [`String`].
///
/// Takes fully **owned** data so it can run on a dedicated worker thread: the
/// `Interp` and every `NanBox` are created and dropped here, never crossing the
/// thread boundary — only the returned `String` (which is `Send`) does. See
/// [`super::NodeTool::call`], which drives this under a wall-clock timeout.
pub fn execute(code: String, root: PathBuf, network: bool, interrupt: Interrupt) -> String {
    // Parse every program up front so each outlives `interp` (the interpreter
    // borrows a program for its own lifetime). A user syntax error is reported
    // as a normal result, not a failure. Declared before `interp` so drop order
    // (reverse) tears `interp` down first, while the programs are still alive.
    let bootstrap = match parse(BOOTSTRAP) {
        Ok(p) => p,
        Err(e) => return format!("internal bootstrap parse error: {e}"),
    };
    let net_bootstrap = if network {
        match parse(NET_BOOTSTRAP) {
            Ok(p) => Some(p),
            Err(e) => return format!("internal network bootstrap parse error: {e}"),
        }
    } else {
        None
    };
    let user_prog = match parse(&code) {
        Ok(p) => p,
        Err(msg) => return msg,
    };

    let mut interp = Interp::new();
    // Install the host watchdog flag: a timeout on the main thread trips this,
    // and the interpreter aborts cooperatively (on loop back-edges) with
    // `ExecError::Interrupted`.
    interp.realm_mut().interrupt = Some(interrupt);
    super::fs::install(&mut interp, root);
    let out = super::console::install(&mut interp);
    if network {
        super::net::install(&mut interp);
    }

    if let Err(e) = interp.run(&bootstrap) {
        return format!(
            "internal error assembling host: {}",
            render_exec_error(&interp, &e)
        );
    }
    if let Some(nb) = net_bootstrap.as_ref()
        && let Err(e) = interp.run(nb)
    {
        return format!(
            "internal error assembling network host: {}",
            render_exec_error(&interp, &e)
        );
    }

    let result = interp.run(&user_prog);

    let mut output = out.borrow().clone();
    match result {
        Ok(value) => {
            // With no console output, fall back to the script's final value.
            if output.is_empty() {
                let display = interp.realm().to_display_string(value);
                if display != "undefined" {
                    output = display;
                }
            }
        }
        Err(e) => {
            let msg = render_exec_error(&interp, &e);
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&format!("Uncaught {msg}"));
        }
    }

    truncate(output)
}

/// Render an [`ExecError`] into a human-readable message. A thrown JS value is
/// displayed via the realm; everything else falls back to its debug form.
///
/// Must be called immediately after the failing `run`, before any further
/// `run` — the moving GC may relocate the thrown value on the next collection.
pub fn render_exec_error(interp: &Interp, err: &ExecError) -> String {
    match err {
        ExecError::Throw(nb) => interp.realm().to_display_string(*nb),
        other => format!("{other:?}"),
    }
}

/// Parse `src` into a program, mapping a parse failure to a readable message.
pub fn parse(src: &str) -> Result<kataan::ast::Program, String> {
    Parser::parse_program(src).map_err(|e| format!("SyntaxError: {e:?}"))
}

/// Truncate `output` to [`MAX_OUTPUT`] on a char boundary, appending a note.
pub fn truncate(mut output: String) -> String {
    if output.len() > MAX_OUTPUT {
        let mut end = MAX_OUTPUT;
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
        output.push_str("\n…[output truncated]");
    }
    output
}
