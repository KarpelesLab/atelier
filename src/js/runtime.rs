//! Glue for running user JS on an [`Interp`]: the bootstrap that assembles the
//! host objects, top-level `await`, and error rendering.

use std::cell::RefCell;
use std::rc::Rc;

use kataan::Interp;
use kataan::NanBox;
use kataan::heap::Handle;
use kataan::interrupt::Interrupt;
use kataan::nbexec::ExecError;
use kataan::parser::Parser;
use kataan::realm::Realm;

use super::fs::Scope;

/// JS run before the user's code. It gathers the mangled `__atelier_*` globals
/// (registered from Rust) into the `fs` / `console` / `os` / `encoding` /
/// `hash` objects the script sees, and defines the pure, dependency-free
/// `path` object entirely in JS.
///
/// The methods are assembled here — in JS — rather than stashed as `NanBox`
/// handles on the Rust side, because kataan has a **moving** garbage collector:
/// a `NanBox` must not be held in Rust across separate `interp.run` calls.
///
/// Plain (not `globalThis.x =`) assignment is deliberate: kataan installs a
/// builtin `console` as a global *scope binding*, which a bare `console`
/// reference resolves to in preference to a `globalThis` property — so we must
/// reassign the binding itself to make the script see our capturing console.
///
/// `path`, `os`, `encoding`, and `hash` are pure helpers (no filesystem or
/// network access), so — unlike `fs` and the network globals — they're always
/// assembled here, regardless of the `network` flag; they don't affect the
/// tool's confinement or approval requirement.
///
/// `fs.promises` (also `require("fs/promises")`) is the same operations
/// returning promises: each runs the synchronous method when called and
/// settles with its result or its error — so it is checked exactly as the
/// synchronous one, and `await` and `.then` work as in Node. kataan drains
/// its job queue at the end of every `interp.run`, so the reactions run
/// before the script's result is read.
///
/// `require` knows only the built-in host objects (`fs`, `fs/promises`,
/// `path`, `os`, with or without `node:`), for scripts written the Node way;
/// there is no module loading.
pub const BOOTSTRAP: &str = r#"
fs = {
  readFile: __atelier_fs_readFile,
  writeFile: __atelier_fs_writeFile,
  readdir: __atelier_fs_readdir,
  exists: __atelier_fs_exists,
  mkdir: __atelier_fs_mkdir,
  stat: __atelier_fs_stat,
  appendFile: __atelier_fs_appendFile,
  rm: __atelier_fs_rm,
  rmdir: __atelier_fs_rmdir,
  rename: __atelier_fs_rename,
  glob: __atelier_fs_glob,
  copyFile: __atelier_fs_copyFile,
  // Binary I/O uses a REAL Uint8Array: readFileBytes wraps the host-returned
  // byte-number array in `new Uint8Array(...)`, and writeFileBytes normalizes a
  // Uint8Array (or plain byte array) to a plain array before crossing to Rust.
  readFileBytes: function (p) { return new Uint8Array(__atelier_fs_readFileBytes(p)); },
  writeFileBytes: function (p, data) {
    var a = [];
    for (var i = 0; i < data.length; i++) a[i] = data[i];
    return __atelier_fs_writeFileBytes(p, a);
  },
};
// The host objects are captured here, not looked up by name when called:
// a script's own top-level `const fs = require("fs")` is a global lexical
// binding that every later name lookup of `fs` would find (in its TDZ).
fs.promises = (function (host) {
  var p = {};
  Object.keys(host).forEach(function (name) {
    var f = host[name];
    p[name] = function () {
      var args = arguments;
      return new Promise(function (resolve) { resolve(f.apply(host, args)); });
    };
  });
  return p;
})(fs);
console = {
  log: __atelier_console_log,
  error: __atelier_console_error,
};
os = {
  platform: __atelier_os_platform,
  arch: __atelier_os_arch,
  type: __atelier_os_type,
  EOL: "\n",
};
encoding = {
  base64Encode: __atelier_encoding_base64Encode,
  base64Decode: __atelier_encoding_base64Decode,
  hexEncode: __atelier_encoding_hexEncode,
  hexDecode: __atelier_encoding_hexDecode,
};
hash = {
  sha256: __atelier_hash_sha256,
};
// path — pure POSIX-style path utilities. No filesystem access: these are
// string manipulations only, so (unlike fs) they need no Rust host call.
path = (function () {
  function isAbs(p) {
    return p.length > 0 && p.charAt(0) === "/";
  }

  // Collapse a split path into its resolved segments, dropping "." entries
  // and resolving ".." against what's already collected. `allowAboveRoot`
  // keeps a leading ".." instead of dropping it (used for relative paths).
  function normalizeArray(parts, allowAboveRoot) {
    var res = [];
    for (var i = 0; i < parts.length; i++) {
      var p = parts[i];
      if (!p || p === ".") continue;
      if (p === "..") {
        if (res.length && res[res.length - 1] !== "..") {
          res.pop();
        } else if (allowAboveRoot) {
          res.push("..");
        }
      } else {
        res.push(p);
      }
    }
    return res;
  }

  function normalize(p) {
    p = String(p);
    if (p === "") return ".";
    var absolute = isAbs(p);
    var trailingSlash = p.length > 1 && p.charAt(p.length - 1) === "/";
    var parts = normalizeArray(p.split("/"), !absolute);
    var out = parts.join("/");
    if (!out && !absolute) out = ".";
    if (out && trailingSlash) out += "/";
    return (absolute ? "/" : "") + out;
  }

  function join() {
    var parts = [];
    for (var i = 0; i < arguments.length; i++) {
      var a = arguments[i];
      if (a === undefined || a === null) continue;
      a = String(a);
      if (a.length) parts.push(a);
    }
    var joined = parts.join("/");
    return joined ? normalize(joined) : ".";
  }

  function dirname(p) {
    p = String(p);
    if (p === "") return ".";
    var absolute = isAbs(p);
    var end = p.length;
    while (end > 1 && p.charAt(end - 1) === "/") end--;
    p = p.substring(0, end);
    var idx = p.lastIndexOf("/");
    if (idx === -1) return absolute ? "/" : ".";
    if (idx === 0) return "/";
    return p.substring(0, idx);
  }

  function basename(p, ext) {
    p = String(p);
    var end = p.length;
    while (end > 0 && p.charAt(end - 1) === "/") end--;
    p = p.substring(0, end);
    var idx = p.lastIndexOf("/");
    var base = idx === -1 ? p : p.substring(idx + 1);
    if (ext && base.length > ext.length && base.substring(base.length - ext.length) === ext) {
      base = base.substring(0, base.length - ext.length);
    }
    return base;
  }

  function extname(p) {
    var base = basename(p);
    var idx = base.lastIndexOf(".");
    // No extension for a dotfile (leading dot) or when there's no dot.
    if (idx <= 0) return "";
    return base.substring(idx);
  }

  function isAbsolute(p) {
    return isAbs(String(p));
  }

  return {
    join: join,
    dirname: dirname,
    basename: basename,
    extname: extname,
    normalize: normalize,
    isAbsolute: isAbsolute,
    sep: "/",
  };
})();
require = (function (builtins) {
  return function (name) {
    var n = String(name);
    if (n.indexOf("node:") === 0) n = n.substring(5);
    if (Object.prototype.hasOwnProperty.call(builtins, n)) return builtins[n];
    throw new Error("Cannot find module '" + name + "': only fs, fs/promises, path and os can be required");
  };
})({ fs: fs, "fs/promises": fs.promises, path: path, os: os });
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

/// Build an interpreter, install the mediated host (`fs` within `scope` +
/// `console`, plus the network host when `network` is set and the `tools`
/// object when the call came with a [`Port`](super::tools::Port) to the
/// session's tools), run the bootstrap(s) and the user's program, and return
/// the captured output as a single [`String`].
///
/// A program that only parses as the body of an async function — one with a
/// top-level `await` — is run as one (see [`parse_user`]); its rejection is
/// reported like an uncaught throw.
///
/// Takes fully **owned** data so it can run on a dedicated worker thread: the
/// `Interp` and every `NanBox` are created and dropped here, never crossing the
/// thread boundary — only the returned `String` (which is `Send`) does. See
/// [`super::NodeTool::call`], which drives this under a wall-clock timeout.
pub fn execute(
    code: String,
    scope: Scope,
    network: bool,
    tools: Option<super::tools::Port>,
    interrupt: Interrupt,
) -> String {
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
    let tools_bootstrap = if tools.is_some() {
        match parse(super::tools::BOOTSTRAP) {
            Ok(p) => Some(p),
            Err(e) => return format!("internal tools bootstrap parse error: {e}"),
        }
    } else {
        None
    };
    let user_prog = match parse_user(&code) {
        Ok(p) => p,
        Err(msg) => return msg,
    };

    let mut interp = Interp::new();
    // Install the host watchdog flag: a timeout on the main thread trips this,
    // and the interpreter aborts cooperatively (on loop back-edges) with
    // `ExecError::Interrupted`.
    interp.realm_mut().interrupt = Some(interrupt);
    super::fs::install(&mut interp, scope);
    let uncaught = install_uncaught(&mut interp);
    super::os::install(&mut interp);
    super::encoding::install(&mut interp);
    super::hash::install(&mut interp);
    let out = super::console::install(&mut interp);
    if network {
        super::net::install(&mut interp);
    }
    if let Some(port) = tools {
        super::tools::install(&mut interp, port);
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
    if let Some(tb) = tools_bootstrap.as_ref()
        && let Err(e) = interp.run(tb)
    {
        return format!(
            "internal error assembling the tools host: {}",
            render_exec_error(&interp, &e)
        );
    }

    let result = interp.run(&user_prog);

    let mut output = out.borrow().clone();
    let rejected = uncaught.borrow_mut().take();
    match result {
        Ok(_) if rejected.is_some() => {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&format!("Uncaught {}", rejected.unwrap_or_default()));
        }
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

    output
}

/// Register `__atelier_uncaught(reason)`, which the async wrapper of a
/// top-level-`await` program rejects into (see [`parse_user`]). The reason is
/// kept as text — `String(reason)`, so an `Error` reads `Error: message` —
/// since a `NanBox` must not outlive the call that handed it over.
fn install_uncaught(interp: &mut Interp) -> Rc<RefCell<Option<String>>> {
    let slot = Rc::new(RefCell::new(None));
    let kept = slot.clone();
    interp.register_global_fn("__atelier_uncaught", 1, move |cx, _this, args| {
        let reason = args.first().copied().unwrap_or_else(|| cx.undefined());
        let text = cx
            .to_string(reason)
            .unwrap_or_else(|_| "(an error that cannot be shown)".into());
        *kept.borrow_mut() = Some(text);
        Ok(cx.undefined())
    });
    slot
}

/// Render an [`ExecError`] into a human-readable message. A thrown error
/// object reads `Name: message`; any other thrown JS value is displayed via
/// the realm; everything else falls back to its debug form.
///
/// Must be called immediately after the failing `run`, before any further
/// `run` — the moving GC may relocate the thrown value on the next collection.
pub fn render_exec_error(interp: &Interp, err: &ExecError) -> String {
    match err {
        ExecError::Throw(nb) => {
            error_text(interp.realm(), *nb).unwrap_or_else(|| interp.realm().to_display_string(*nb))
        }
        other => format!("{other:?}"),
    }
}

/// `name: message` for a thrown object that has a string `message` (own or
/// inherited), as `Error.prototype.toString` would put it; `None` for
/// anything else. The realm's display form of an object is `[object
/// Object]`, which says nothing about what went wrong.
fn error_text(realm: &Realm, v: NanBox) -> Option<String> {
    let object = Handle::from_raw(v.as_handle()?);
    let string = |key: &str| -> Option<String> {
        // Up the prototype chain, a few links at most.
        let mut at = Some(object);
        for _ in 0..8 {
            let here = at?;
            if let Some(value) = realm.get_property(here, key) {
                let handle = Handle::from_raw(value.as_handle()?);
                return (realm.type_of(handle)? == "string")
                    .then(|| realm.to_display_string(value));
            }
            at = realm.object_proto(here);
        }
        None
    };
    let message = string("message")?;
    let name = string("name").unwrap_or_else(|| "Error".into());
    Some(match (name.is_empty(), message.is_empty()) {
        (true, _) => message,
        (false, true) => name,
        (false, false) => format!("{name}: {message}"),
    })
}

/// Parse `src` into a program, mapping a parse failure to a readable message.
pub fn parse(src: &str) -> Result<kataan::ast::Program, String> {
    Parser::parse_program(src).map_err(|e| format!("SyntaxError: {e:?}"))
}

/// Parse the user's program. A script is a script; one that does not parse
/// as one but does as the body of an async function is a script with a
/// top-level `await` (which kataan's script goal does not allow), and is run
/// as that function, its rejection going to `__atelier_uncaught`. The body
/// starts on the first line, so line numbers in errors stay the user's. What
/// parses neither way is reported with the script's own syntax error.
fn parse_user(src: &str) -> Result<kataan::ast::Program, String> {
    let err = match parse(src) {
        Ok(p) => return Ok(p),
        Err(e) => e,
    };
    let wrapped = format!("(async function () {{{src}\n}})().then(undefined, __atelier_uncaught);");
    parse(&wrapped).map_err(|_| err)
}
