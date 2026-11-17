//! JavaScript scripting via an embedded kataan runtime (roadmap M8).
//!
//! The `node` tool runs a JS script on [kataan](https://github.com/KarpelesLab/kataan)
//! with **host APIs we implement and mediate**. kataan's core is sans-I/O, so we
//! provide `fs`/`console` ourselves and check every effect at call time. By
//! default `fs` is confined to the project root and there is no network, so a
//! plain call can't escape the project and is auto-approved; asking for the
//! network or for paths outside the project makes the call need approval.
//!
//! # Outside the project
//!
//! A call can name paths outside the project its script may use:
//! `allow_read` (read only) and `allow_write` (read and write), each entry
//! covering itself and what is under it. Declaring one makes the call need
//! approval ([`Tool::requires_approval_in`]) — the user is shown exactly what
//! the script asks for, ahead of the prompt (`risk::signals`) — and the
//! mediated `fs` then allows those prefixes and still refuses every other
//! outside path (see [`fs::Scope`]). The grant is per call: nothing is
//! remembered, and "always" saves no rule for it (`permissions::propose`).
//!
//! # Code mode
//!
//! A call with `tools: true` also gets a `tools` object, to call the
//! session's other tools from the script. That needs the session — its
//! rules, its hooks, its user — so such a call is not run by
//! [`NodeTool::call`] but by the agent's tool runner through [`tools::run`],
//! which serves the script's tool calls on the turn's thread. See [`tools`].

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use kataan::interrupt::Interrupt;
use serde_json::{Value, json};

use crate::tools::{Tool, ToolCtx, ToolSpec, spill};

mod console;
mod encoding;
mod fs;
mod hash;
mod net;
mod os;
mod runtime;
pub mod tools;

/// Default wall-clock budget for a script when the call omits `timeout_ms`.
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Hard ceiling on `timeout_ms` so a call cannot pin a worker thread forever by
/// request (a runaway script past this is abandoned — see [`NodeTool::call`]).
const MAX_TIMEOUT_MS: u64 = 600_000;
/// The most output, in bytes, a script's result carries; past it the result
/// is the head and the tail, with all of it in a file (see [`spill`]).
const MAX_OUTPUT_BYTES: usize = 10 * 1024;
/// Worker-thread stack. The tree-walk interpreter recurses on the native stack;
/// kataan tunes its eval-depth guard for a ~2 MB stack, so a generous stack here
/// keeps a comfortable margin before the (catchable) `RangeError` fires.
const WORKER_STACK_BYTES: usize = 16 * 1024 * 1024;

/// After tripping the interrupt on a timeout, how long to wait for the worker to
/// unwind cooperatively before abandoning it (a script with no loop back-edge to
/// observe the flag is the pathological case; it's abandoned rather than blocking
/// the harness).
const INTERRUPT_GRACE_MS: u64 = 1_000;

/// The `node` tool: run JavaScript with a mediated, project-confined host.
pub struct NodeTool;

/// What a `node` call asks for, apart from `tools`.
struct Params {
    code: String,
    /// Install the network host.
    network: bool,
    /// The script's time budget, within `1..=MAX_TIMEOUT_MS`.
    timeout_ms: u64,
    /// Paths outside the project the script may use.
    grants: Grants,
}

/// The paths a call names in `allow_read` / `allow_write`, made absolute
/// (relative to the project root, `~` the home directory) and lexically
/// normalized — the form [`fs::Scope`] checks paths in.
#[derive(Debug, Default, PartialEq)]
pub struct Grants {
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
}

impl Grants {
    /// The grants `args` declare. A malformed list (not an array of
    /// non-empty strings) is an error, never read as no grant.
    pub fn parse(args: &Value, root: &Path) -> Result<Self> {
        let list = |key: &str| -> Result<Vec<PathBuf>> {
            let Some(value) = args.get(key).filter(|v| !v.is_null()) else {
                return Ok(Vec::new());
            };
            let items = value
                .as_array()
                .ok_or_else(|| anyhow!("`{key}` must be an array of paths"))?;
            items
                .iter()
                .map(|item| match item.as_str() {
                    Some(p) if !p.trim().is_empty() => Ok(absolute(root, p)),
                    _ => Err(anyhow!(
                        "`{key}` must be an array of non-empty path strings"
                    )),
                })
                .collect()
        };
        Ok(Self {
            read: list("allow_read")?,
            write: list("allow_write")?,
        })
    }

    /// The grants that reach outside `root` (one inside it grants nothing
    /// the script did not already have).
    pub fn outside(&self, root: &Path) -> Self {
        let keep = |list: &[PathBuf]| -> Vec<PathBuf> {
            list.iter()
                .filter(|p| !p.starts_with(root))
                .cloned()
                .collect()
        };
        Self {
            read: keep(&self.read),
            write: keep(&self.write),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.read.is_empty() && self.write.is_empty()
    }

    /// The `fs` scope of a script run with these grants in `root`.
    fn scope(self, root: &Path) -> fs::Scope {
        fs::Scope {
            root: root.to_path_buf(),
            read: self.read,
            write: self.write,
        }
    }
}

/// `path` made absolute against `root`, with a leading `~` read as the home
/// directory, and `.`/`..` folded (as [`crate::tools::confine`] does).
fn absolute(root: &Path, path: &str) -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    let joined = match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => match home() {
            Some(h) => h.join(rest.trim_start_matches('/')),
            None => root.join(path),
        },
        _ => root.join(path),
    };
    crate::tools::normalize(&joined)
}

/// Whether a `node` call names any path in `allow_read`/`allow_write` — the
/// question without the root at hand, where a path in the project counts
/// too. "Always" saves no blanket `node` rule for such a call.
pub fn declares_paths(args: &Value) -> bool {
    ["allow_read", "allow_write"]
        .iter()
        .any(|k| args.get(*k).is_some_and(|v| !v.is_null()))
}

/// What the approval prompt says about a `node` call beyond its arguments:
/// the network, and each outside path, by what the script may do there.
pub fn approval_signals(args: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if args
        .get("network")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        out.push("network access (fetch)".to_string());
    }
    for (key, what) in [
        ("allow_read", "reads outside the project"),
        ("allow_write", "writes outside the project"),
    ] {
        let Some(items) = args.get(key).and_then(Value::as_array) else {
            continue;
        };
        let paths: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
        if !paths.is_empty() {
            out.push(format!("{what}: {}", paths.join(", ")));
        }
    }
    out
}

impl Params {
    fn parse(args: &Value, root: &Path) -> Result<Self> {
        let code = args
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("the `node` tool requires a string `code` argument"))?
            .to_owned();
        let network = args
            .get("network")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS);
        let grants = Grants::parse(args, root)?.outside(root);
        Ok(Self {
            code,
            network,
            timeout_ms,
            grants,
        })
    }
}

/// Whether a `node` call asks for the `tools` object (code mode). Such a
/// call is run by the session's tool runner, which can serve the script's
/// tool calls (see [`tools`]); [`NodeTool::call`] alone cannot.
pub fn wants_tools(args: &Value) -> bool {
    args.get("tools").and_then(Value::as_bool).unwrap_or(false)
}

impl Tool for NodeTool {
    fn name(&self) -> &str {
        "node"
    }

    fn requires_approval(&self, args: &Value) -> bool {
        // Confined by default (mediated fs stays in the project root), so a plain
        // call is auto-approved. A call that opts into network (`network: true`)
        // is unconfined and requires approval; so, without the root to tell, is
        // one that names any path for `allow_read`/`allow_write`.
        args.get("network")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || declares_paths(args)
    }

    /// As [`requires_approval`](Tool::requires_approval), but a declared path
    /// inside the project asks for nothing new and needs no approval. A
    /// malformed list does (the call will fail, but nothing is waved through).
    fn requires_approval_in(&self, args: &Value, root: &Path) -> bool {
        let network = args
            .get("network")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        network || Grants::parse(args, root).map_or(true, |g| !g.outside(root).is_empty())
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "node".into(),
            description: "Run a JavaScript program in a sandboxed runtime (no module \
                loading: `import` is unavailable, and require() only returns the built-ins \
                'fs', 'fs/promises', 'path' and 'os'). Six globals are provided directly, \
                already in scope: `console` (log, error), an `fs` confined to the project \
                directory, a pure `path` (POSIX-style path utilities, no I/O), a pure `os` \
                (host facts, no I/O), a pure `encoding` (base64/hex string conversions, no \
                I/O), and a pure `hash` (SHA-256, no I/O). The fs methods are SYNCHRONOUS and \
                return values directly (no callbacks); fs.promises has the same methods \
                returning promises, for `await fs.promises.readFile(...)` (top-level await \
                works): \
                fs.readFile(path[, 'utf8']) -> string; fs.writeFile(path, content); \
                fs.readdir(path) -> string[]; fs.exists(path) -> boolean; fs.mkdir(path); \
                fs.stat(path) -> {isFile, isDirectory, size, mtimeMs}; \
                fs.appendFile(path, content); fs.rm(path) (file only); fs.rmdir(path) \
                (empty dir only, non-recursive); fs.rename(from, to); \
                fs.glob(pattern) -> string[] (e.g. 'src/**/*.rs'; lists project files matching \
                the pattern, respecting .gitignore, sorted and capped at 500 results); \
                fs.copyFile(src, dst) (copies a file, creating dst's parent directories). \
                Binary I/O: \
                fs.readFileBytes(path) -> Uint8Array; fs.writeFileBytes(path, data) where data \
                is a Uint8Array or a plain array of byte numbers. Paths \
                are relative to the project root and cannot escape it, except paths the \
                call lists in `allow_read`/`allow_write` (which makes it require approval). \
                `path` (always \
                available, no filesystem access of its own): path.join(...parts), \
                path.dirname(p), path.basename(p[, ext]), path.extname(p), path.normalize(p), \
                path.isAbsolute(p), path.sep ('/'). `os` (always available): os.platform() \
                (e.g. 'darwin'/'linux'/'win32'), os.arch(), os.type() (e.g. 'Darwin'), \
                os.EOL ('\\n'). `encoding` (always available, no I/O): \
                encoding.base64Encode(str) -> string; encoding.base64Decode(b64) -> string \
                (UTF-8 lossy on the decoded bytes); encoding.hexEncode(str) -> string; \
                encoding.hexDecode(hex) -> string (UTF-8 lossy on the decoded bytes) — all \
                operate on the UTF-8 bytes of the input string and throw on invalid input. \
                `hash` (always available, no I/O): hash.sha256(str) -> string (lowercase hex \
                digest of the UTF-8 bytes of the input). Returns the captured \
                console output. Use this for logic that would be awkward as a shell one-liner. \
                Execution is bounded by `timeout_ms` (default 5000); a script that exceeds it is \
                aborted and the tool returns a timeout notice. Set `network: true` to additionally \
                get SYNCHRONOUS HTTP globals (this makes the call require approval): \
                fetch(url[, {method, headers, body}]) -> {status, ok, body, headers}; \
                httpRequest({method, url, headers, body}) -> {status, ok, body, headers}; \
                httpGet(url) -> string (the body). Without `network: true` those globals are \
                undefined (typeof fetch === 'undefined'). Set `tools: true` to call your other \
                tools from the script through a SYNCHRONOUS `tools` global: \
                tools.call(name, args) -> {ok, text, json} runs the tool and returns its \
                result to the script as data (`text` is what the tool would have answered \
                you; `json` is set when that text is a JSON object or array; a refused or \
                failed call has ok === false and the reason in `text` — it does not throw); \
                tools.list() -> [{name, description}] and tools.schema(name) -> the tool's \
                input schema. Prefer such a script to calling the tools one by one when you \
                need many calls whose intermediate results you do not have to read \
                yourself, or when you would filter, join or count over large results: only \
                what the script prints comes back to you. Each call is checked exactly as \
                if you had made it (permission rules, approval). At most 50 tool calls per \
                script; MCP tools are called by their full name (mcp__server__tool)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "The JavaScript source to run." },
                    "network": {
                        "type": "boolean",
                        "description": "Enable the synchronous fetch/httpGet/httpRequest HTTP \
                            globals. Default false. Setting it true makes this call require \
                            user approval.",
                        "default": false
                    },
                    "tools": {
                        "type": "boolean",
                        "description": "Provide the `tools` global, to call other tools \
                            from the script and work on their results there. Default false.",
                        "default": false
                    },
                    "allow_read": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Paths OUTSIDE the project the script's fs may read \
                            (each covers itself and everything under it; absolute, `~/`, or \
                            relative to the project root). Naming any makes this call require \
                            user approval. Only for files the task really needs outside the \
                            project; every other outside path still throws."
                    },
                    "allow_write": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Paths OUTSIDE the project the script's fs may read and \
                            write (create, append, remove, rename). Same form and approval as \
                            `allow_read`."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Wall-clock execution budget in milliseconds. Default \
                            5000. A script that runs longer is aborted and the tool returns a \
                            timeout notice. With `tools`, time spent inside a tool call does \
                            not count.",
                        "default": DEFAULT_TIMEOUT_MS
                    }
                },
                "required": ["code"]
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        // The `tools` object needs someone to run the script's tool calls and
        // to decide each of them — the session, which a tool cannot reach.
        // The tool runner takes such a call before it gets here; should one
        // arrive anyway it is refused, never run with tools nobody checks.
        if wants_tools(&args) {
            bail!(
                "`tools: true` is not available here: the script's tool calls need the \
                 session to approve and run them"
            );
        }
        // Owned inputs for the worker thread: the script source, where its
        // `fs` may go, and whether to install the network host.
        let Params {
            code,
            network,
            timeout_ms,
            grants,
        } = Params::parse(&args, ctx.project_root)?;
        // Outside paths are only for a call that went through approval, which
        // `requires_approval_in` demands for them: calling the tool directly
        // does not skip the prompt.
        if !grants.is_empty() && !ctx.approved {
            bail!(
                "`allow_read`/`allow_write` name paths outside the project, which needs the \
                 user's approval, and this call was not approved"
            );
        }
        let scope = grants.scope(ctx.project_root);
        let project_root = ctx.project_root;

        // Run the interpreter on a dedicated worker thread and wait for its
        // result with a wall-clock deadline. The `Interp`/`NanBox` values live
        // and die entirely inside the worker; only the output `String` (which is
        // `Send`) crosses back over the channel.
        //
        // On a timeout we trip a cooperative interrupt (kataan >= 0.0.9): the
        // interpreter observes it on the next loop back-edge and aborts with
        // `ExecError::Interrupted`, so the worker actually stops and is reaped —
        // no leaked CPU. The `Interrupt` is an `Arc<AtomicBool>` (Send+Sync), so
        // the main thread can trip it while the `Interp` stays on the worker.
        let interrupt = Interrupt::new();
        let worker_interrupt = interrupt.clone();
        let (tx, rx) = mpsc::channel::<String>();
        let worker = thread::Builder::new()
            .name("node-script".into())
            .stack_size(WORKER_STACK_BYTES)
            .spawn(move || {
                let out = runtime::execute(code, scope, network, None, worker_interrupt);
                // Ignore a send error: it only means we already timed out and
                // the receiver is gone.
                let _ = tx.send(out);
            })
            .map_err(|e| anyhow!("failed to spawn node worker thread: {e}"))?;

        match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
            Ok(output) => {
                // Reap the finished worker so it doesn't linger as a zombie.
                let _ = worker.join();
                Ok(spill::fit(project_root, output, MAX_OUTPUT_BYTES))
            }
            Err(RecvTimeoutError::Timeout) => {
                // Trip the interrupt and give the interpreter a moment to unwind,
                // then reap it. If it doesn't observe the flag within the grace
                // window (a script with no back-edge to check), abandon it rather
                // than block the harness.
                interrupt.trip();
                if rx
                    .recv_timeout(Duration::from_millis(INTERRUPT_GRACE_MS))
                    .is_ok()
                {
                    let _ = worker.join();
                }
                Ok(format!(
                    "node: script exceeded the {timeout_ms}ms time limit and was aborted"
                ))
            }
            Err(RecvTimeoutError::Disconnected) => {
                // The worker panicked before sending (should not happen — the
                // interpreter turns script errors into a normal result).
                let _ = worker.join();
                Ok("node: script worker terminated unexpectedly".into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;

    use crate::tools::{FileState, Tool, ToolCtx};

    use super::NodeTool;

    /// A unique, freshly created temp directory for a test (no external crate).
    fn tmpdir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("atelier-node-test-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Run `code` through `NodeTool` with a fresh context rooted at `root`.
    fn run(root: &std::path::Path, code: &str) -> String {
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: root,
            fstate: &mut fstate,
            approved: false,
        };
        NodeTool
            .call(&mut ctx, json!({ "code": code }))
            .expect("node tool should not fail at the harness level")
    }

    /// Confirms the kataan embedding API this module is built on.
    #[test]
    fn kataan_smoke() {
        use kataan::Interp;
        use kataan::parser::Parser;

        let mut interp = Interp::new();
        interp.register_global_fn("hostAdd", 2, |cx, _this, args| {
            let a = cx.to_number(args.first().copied().unwrap_or_else(|| cx.undefined()))?;
            let b = cx.to_number(args.get(1).copied().unwrap_or_else(|| cx.undefined()))?;
            Ok(cx.number(a + b))
        });
        let program = Parser::parse_program("hostAdd(2, 3)").unwrap();
        let result = interp.run(&program).unwrap();
        assert_eq!(interp.realm().to_display_string(result), "5");
    }

    #[test]
    fn fs_write_then_read_and_console() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                fs.writeFile("out.txt", "hello from js");
                var back = fs.readFile("out.txt");
                console.log("read:", back);
            "#,
        );

        // The file really landed on disk inside the root.
        let written = std::fs::read_to_string(root.join("out.txt")).unwrap();
        assert_eq!(written, "hello from js");
        // And the console output was captured.
        assert!(
            output.contains("read: hello from js"),
            "unexpected output: {output:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fs_path_escape_is_rejected() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                try {
                    fs.readFile("../../etc/passwd");
                    console.log("NOT_CAUGHT");
                } catch (e) {
                    console.log("caught:", e.message);
                }
            "#,
        );
        assert!(
            output.contains("caught:"),
            "escape should throw a catchable error, got: {output:?}"
        );
        assert!(
            !output.contains("NOT_CAUGHT"),
            "escaping path must not succeed: {output:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plain_console_log() {
        let root = tmpdir();
        let output = run(&root, r#"console.log("hi")"#);
        assert_eq!(output.trim(), "hi");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An infinite loop is aborted at the wall-clock deadline and returns the
    /// timeout notice promptly. NOTE: because kataan v0.0.8 has no JS interrupt,
    /// the worker thread is abandoned and keeps spinning until the process exits
    /// — acceptable in a test, but it does leak one thread.
    #[test]
    fn timeout_aborts_infinite_loop() {
        let root = tmpdir();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };

        let start = std::time::Instant::now();
        let output = NodeTool
            .call(
                &mut ctx,
                json!({ "code": "while (true) {}", "timeout_ms": 300 }),
            )
            .expect("node tool should not fail at the harness level");
        let elapsed = start.elapsed();

        assert!(
            output.contains("exceeded the 300ms time limit"),
            "unexpected output: {output:?}"
        );
        // The interrupt (kataan >= 0.0.9) actually stops the worker: the call
        // returns near the deadline + a short grace, and the worker is reaped
        // (not leaked). Bound it well under the grace window to prove the loop
        // observed the interrupt rather than running free.
        assert!(
            elapsed < std::time::Duration::from_millis(300 + super::INTERRUPT_GRACE_MS + 500),
            "timeout took too long (worker may not have been interrupted): {elapsed:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A fast script finishes and returns normally well within its budget.
    #[test]
    fn completes_before_timeout() {
        let root = tmpdir();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let output = NodeTool
            .call(
                &mut ctx,
                json!({ "code": "console.log(1 + 2)", "timeout_ms": 5000 }),
            )
            .expect("node tool should not fail at the harness level");
        assert_eq!(output.trim(), "3");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Output past the budget keeps its start and its end; the whole of it
    /// is in a file under the project.
    #[test]
    fn long_output_is_saved_to_a_file() {
        let root = tmpdir();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let code = r#"for (let i = 1; i <= 3000; i++) console.log("row " + i);"#;
        let output = NodeTool.call(&mut ctx, json!({ "code": code })).unwrap();
        assert!(
            output.len() < super::MAX_OUTPUT_BYTES,
            "{} bytes",
            output.len()
        );
        assert!(output.starts_with("row 1\nrow 2\n"), "{output}");
        assert!(output.contains("row 3000"), "{output}");
        let (_, rest) = output.split_once("full output: ").expect("a marker");
        let full = std::fs::read_to_string(root.join(rest.split_once(' ').unwrap().0)).unwrap();
        assert!(full.contains("row 1500\n"));
        assert!(!output.contains("row 1500\n"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Without `network: true`, the HTTP globals are not installed.
    #[test]
    fn network_globals_absent_by_default() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"console.log(typeof fetch, typeof httpGet, typeof httpRequest)"#,
        );
        assert_eq!(output.trim(), "undefined undefined undefined");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// With `network: true`, the HTTP globals are installed as functions (no
    /// request is made, so this doesn't touch the network).
    #[test]
    fn network_globals_present_when_enabled() {
        let root = tmpdir();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let output = NodeTool
            .call(
                &mut ctx,
                json!({
                    "code": "console.log(typeof fetch, typeof httpGet, typeof httpRequest)",
                    "network": true
                }),
            )
            .expect("node tool should not fail at the harness level");
        assert_eq!(output.trim(), "function function function");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `fs.stat` distinguishes a file from a directory and reports a size.
    #[test]
    fn fs_stat_file_and_dir() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                fs.writeFile("f.txt", "abcde");
                fs.mkdir("sub");
                var f = fs.stat("f.txt");
                var d = fs.stat("sub");
                console.log(f.isFile, f.isDirectory, f.size, typeof f.mtimeMs);
                console.log(d.isFile, d.isDirectory);
            "#,
        );
        assert!(
            output.contains("true false 5 number"),
            "unexpected file stat: {output:?}"
        );
        assert!(
            output.contains("false true"),
            "unexpected dir stat: {output:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `fs.appendFile` creates the file then appends to it.
    #[test]
    fn fs_append_file() {
        let root = tmpdir();
        run(
            &root,
            r#"
                fs.appendFile("log.txt", "one\n");
                fs.appendFile("log.txt", "two\n");
            "#,
        );
        let written = std::fs::read_to_string(root.join("log.txt")).unwrap();
        assert_eq!(written, "one\ntwo\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `fs.rm` removes a file; removing a directory with it throws.
    #[test]
    fn fs_rm_file_and_rejects_dir() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                fs.writeFile("gone.txt", "x");
                fs.rm("gone.txt");
                console.log("exists:", fs.exists("gone.txt"));
                fs.mkdir("adir");
                try { fs.rm("adir"); console.log("NOT_CAUGHT"); }
                catch (e) { console.log("caught"); }
            "#,
        );
        assert!(
            !root.join("gone.txt").exists(),
            "file should have been removed"
        );
        assert!(output.contains("exists: false"), "unexpected: {output:?}");
        assert!(
            output.contains("caught"),
            "rm on dir should throw: {output:?}"
        );
        assert!(!output.contains("NOT_CAUGHT"), "unexpected: {output:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `fs.rename` moves a file within the project root.
    #[test]
    fn fs_rename() {
        let root = tmpdir();
        run(
            &root,
            r#"
                fs.writeFile("a.txt", "content");
                fs.rename("a.txt", "b.txt");
            "#,
        );
        assert!(!root.join("a.txt").exists(), "source should be gone");
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "content"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Binary round-trip: write raw bytes, read them back as a Uint8Array, and
    /// assert equality inside the script.
    #[test]
    fn fs_binary_round_trip() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                var src = [0, 1, 2, 127, 128, 254, 255];
                fs.writeFileBytes("bin.dat", src);
                var back = fs.readFileBytes("bin.dat");
                var isU8 = (back instanceof Uint8Array);
                var eq = (back.length === src.length);
                for (var i = 0; i < src.length; i++) if (back[i] !== src[i]) eq = false;
                console.log("u8:", isU8, "eq:", eq, "len:", back.length);
            "#,
        );
        assert!(
            output.contains("u8: true eq: true len: 7"),
            "binary round-trip failed: {output:?}"
        );
        // The bytes really landed on disk.
        assert_eq!(
            std::fs::read(root.join("bin.dat")).unwrap(),
            vec![0u8, 1, 2, 127, 128, 254, 255]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A confined write of a Uint8Array (not a plain array) also round-trips,
    /// exercising the bootstrap's normalization path.
    #[test]
    fn fs_binary_write_from_uint8array() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                var u = new Uint8Array([10, 20, 30]);
                fs.writeFileBytes("u.dat", u);
                var back = fs.readFileBytes("u.dat");
                console.log(back[0], back[1], back[2], back.length);
            "#,
        );
        assert!(output.contains("10 20 30 3"), "unexpected: {output:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The new methods stay confined: an escaping path throws.
    #[test]
    fn fs_new_methods_reject_escape() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                function esc(fn) {
                    try { fn(); return "NOT_CAUGHT"; } catch (e) { return "caught"; }
                }
                console.log(esc(function () { fs.stat("../../etc/passwd"); }));
                console.log(esc(function () { fs.appendFile("../x", "y"); }));
                console.log(esc(function () { fs.rm("../x"); }));
                console.log(esc(function () { fs.rename("../a", "b"); }));
                console.log(esc(function () { fs.readFileBytes("../../etc/passwd"); }));
                console.log(esc(function () { fs.writeFileBytes("../x", [1]); }));
                console.log(esc(function () { fs.copyFile("../../etc/passwd", "x"); }));
                console.log(esc(function () { fs.copyFile("a.txt", "../x"); }));
            "#,
        );
        assert!(
            !output.contains("NOT_CAUGHT"),
            "escape must throw: {output:?}"
        );
        assert_eq!(
            output.matches("caught").count(),
            8,
            "all eight escapes should throw: {output:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `fs.glob` lists project files matching a pattern, sorted.
    #[test]
    fn fs_glob_lists_matching_files() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                fs.writeFile("b.txt", "b");
                fs.writeFile("a.txt", "a");
                fs.writeFile("d.rs", "d");
                var matches = fs.glob("*.txt");
                console.log(Array.isArray(matches), matches.length);
                console.log(matches.join(","));
            "#,
        );
        assert!(
            output.contains("true 2"),
            "expected exactly the two top-level .txt files: {output:?}"
        );
        assert!(
            output.contains("a.txt,b.txt"),
            "expected sorted results: {output:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `fs.glob` with `**` reaches into subdirectories too.
    #[test]
    fn fs_glob_recursive_pattern() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                fs.writeFile("top.txt", "x");
                fs.writeFile("sub/deep.txt", "y");
                var matches = fs.glob("**/*.txt");
                console.log(matches.length, matches.indexOf("top.txt") >= 0, matches.indexOf("sub/deep.txt") >= 0);
            "#,
        );
        assert!(
            output.contains("2 true true"),
            "unexpected recursive glob result: {output:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `fs.copyFile` copies a file's contents to a new path, creating parent
    /// directories, and leaves the source untouched.
    #[test]
    fn fs_copy_file() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                fs.writeFile("src.txt", "copy me");
                fs.copyFile("src.txt", "nested/dst.txt");
                console.log(fs.readFile("src.txt"));
                console.log(fs.readFile("nested/dst.txt"));
            "#,
        );
        assert!(output.contains("copy me"), "unexpected output: {output:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("nested/dst.txt")).unwrap(),
            "copy me"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src.txt")).unwrap(),
            "copy me",
            "source should be unchanged"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `network: true` flips the approval requirement; a plain call does not.
    #[test]
    fn network_flag_gates_approval() {
        assert!(!NodeTool.requires_approval(&json!({ "code": "1" })));
        assert!(NodeTool.requires_approval(&json!({ "code": "1", "network": true })));
    }

    /// `path` is a pure, always-available global: exercise the common cases
    /// for join/dirname/basename/extname/normalize/isAbsolute/sep.
    #[test]
    fn path_utilities() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                console.log(path.join("a", "b", "c.txt"));
                console.log(path.join("/a/", "b"));
                console.log(path.join("a", "", "b"));
                console.log(path.join());
                console.log(path.dirname("/foo/bar/baz.js"));
                console.log(path.dirname("foo"));
                console.log(path.dirname("/"));
                console.log(path.basename("/foo/bar/baz.js"));
                console.log(path.basename("/foo/bar/baz.js", ".js"));
                console.log(path.basename("/foo/bar/"));
                console.log(path.extname("index.html"));
                console.log(path.extname(".gitignore"));
                console.log(path.extname("noext"));
                console.log(path.normalize("/foo/../bar//baz/./"));
                console.log(path.isAbsolute("/foo"));
                console.log(path.isAbsolute("foo"));
                console.log(path.sep);
            "#,
        );
        let expected = "\
a/b/c.txt
/a/b
a/b
.
/foo/bar
.
/
baz.js
baz
bar
.html


/bar/baz/
true
false
/
";
        assert_eq!(output, expected, "unexpected path output: {output:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `os` is a pure, always-available global exposing host facts. Don't
    /// assert a specific platform string — just that it's non-empty and that
    /// the deterministic bits (`EOL`, the types of the functions) hold.
    #[test]
    fn os_facts() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                console.log(typeof os.platform, typeof os.arch, typeof os.type);
                console.log(os.platform().length > 0, os.arch().length > 0, os.type().length > 0);
                console.log(os.EOL === "\n");
            "#,
        );
        let mut lines = output.lines();
        assert_eq!(lines.next(), Some("function function function"));
        assert_eq!(lines.next(), Some("true true true"));
        assert_eq!(lines.next(), Some("true"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `encoding` and `hash` are pure, always-available globals: a base64
    /// round-trip, a hex round-trip, and a known SHA-256 digest.
    #[test]
    fn encoding_and_hash_globals() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                var b64 = encoding.base64Encode("hello world");
                console.log(b64);
                console.log(encoding.base64Decode(b64));

                var hex = encoding.hexEncode("hello world");
                console.log(hex);
                console.log(encoding.hexDecode(hex));

                console.log(hash.sha256("abc"));
            "#,
        );
        let mut lines = output.lines();
        assert_eq!(lines.next(), Some("aGVsbG8gd29ybGQ="));
        assert_eq!(lines.next(), Some("hello world"));
        assert_eq!(lines.next(), Some("68656c6c6f20776f726c64"));
        assert_eq!(lines.next(), Some("hello world"));
        assert_eq!(
            lines.next(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Invalid base64/hex input throws a catchable JS error rather than
    /// panicking the worker.
    #[test]
    fn encoding_rejects_invalid_input() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                function esc(fn) {
                    try { fn(); return "NOT_CAUGHT"; } catch (e) { return "caught"; }
                }
                console.log(esc(function () { encoding.base64Decode("not valid base64!!"); }));
                console.log(esc(function () { encoding.hexDecode("zz"); }));
                console.log(esc(function () { encoding.hexDecode("abc"); }));
            "#,
        );
        assert!(
            !output.contains("NOT_CAUGHT"),
            "invalid input must throw: {output:?}"
        );
        assert_eq!(
            output.matches("caught").count(),
            3,
            "all three invalid inputs should throw: {output:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Run a `node` call with `args` in `root`, approved or not.
    fn call(
        root: &std::path::Path,
        args: serde_json::Value,
        approved: bool,
    ) -> anyhow::Result<String> {
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: root,
            fstate: &mut fstate,
            approved,
        };
        NodeTool.call(&mut ctx, args)
    }

    /// `fs.promises` mirrors the synchronous methods, and a top-level
    /// `await` runs the script as an async body.
    #[test]
    fn fs_promises_with_top_level_await() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                await fs.promises.writeFile("p.txt", "via promises");
                const text = await fs.promises.readFile("p.txt", "utf8");
                const names = await fs.promises.readdir(".");
                console.log(text, names.join(","), await fs.promises.exists("p.txt"));
            "#,
        );
        assert_eq!(output.trim(), "via promises p.txt true", "{output}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The `Promise.all` example of docs/scripting.md.
    #[test]
    fn fs_promises_with_promise_all() {
        let root = tmpdir();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a"), "12345").unwrap();
        std::fs::write(root.join("src/b"), "678").unwrap();
        let output = run(
            &root,
            r#"
                const fsp = require("fs/promises");
                const names = await fsp.readdir("src");
                const sizes = await Promise.all(names.map(async (n) => (await fsp.stat("src/" + n)).size));
                console.log(sizes.reduce((a, b) => a + b, 0));
            "#,
        );
        assert_eq!(output.trim(), "8", "{output}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A refused path rejects the promise (no throw at the call), with the
    /// same confinement as the synchronous method; `.then` chains run before
    /// the result is read.
    #[test]
    fn fs_promises_reject_an_escape() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                fs.promises.readFile("../../etc/passwd").then(
                    function () { console.log("NOT_REJECTED"); },
                    function (e) { console.log("rejected:", e.message); }
                );
                console.log("first");
            "#,
        );
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines[0], "first", "{output}");
        assert!(lines[1].starts_with("rejected:"), "{output}");
        assert!(lines[1].contains("escapes the project root"), "{output}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An awaited rejection nobody catches ends the script like a throw.
    #[test]
    fn uncaught_rejection_after_await_is_reported() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                console.log("before");
                await fs.promises.readFile("../outside.txt");
                console.log("NOT_REACHED");
            "#,
        );
        assert!(output.starts_with("before\nUncaught Error: "), "{output}");
        assert!(!output.contains("NOT_REACHED"), "{output}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A thrown error reads `Name: message`, not `[object Object]`.
    #[test]
    fn uncaught_error_shows_its_message() {
        let root = tmpdir();
        assert_eq!(
            run(&root, r#"throw new TypeError("bad thing")"#).trim(),
            "Uncaught TypeError: bad thing"
        );
        assert_eq!(run(&root, r#"throw "plain""#).trim(), "Uncaught plain");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A real syntax error is reported as the script's, not hidden by the
    /// top-level-`await` retry.
    #[test]
    fn syntax_error_is_still_reported() {
        let root = tmpdir();
        let output = run(&root, "let x = ;");
        assert!(output.starts_with("SyntaxError"), "{output}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `require` returns the built-in host objects, Node-style, and nothing
    /// else.
    #[test]
    fn require_returns_the_builtins() {
        let root = tmpdir();
        let output = run(
            &root,
            r#"
                const fs = require("fs");
                const fsp = require("node:fs/promises");
                const path = require("path");
                console.log(fs.promises === fsp, typeof fsp.readFile, path.join("a", "b"),
                    typeof require("os").platform);
                try { require("child_process"); } catch (e) { console.log("refused:", e.message); }
            "#,
        );
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines[0], "true function a/b function", "{output}");
        assert!(
            lines[1].starts_with("refused: Cannot find module"),
            "{output}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An approved call reaches the outside paths it declared — read-only
    /// ones for reading only — and no other.
    #[test]
    fn outside_paths_granted_by_an_approved_call() {
        let root = tmpdir();
        let ro = tmpdir();
        let rw = tmpdir();
        let other = tmpdir();
        std::fs::write(ro.join("in.txt"), "outside text").unwrap();
        std::fs::write(other.join("secret.txt"), "no").unwrap();
        let code = format!(
            r#"
                console.log(fs.readFile({ro:?} + "/in.txt"));
                try {{ fs.writeFile({ro:?} + "/x.txt", "x"); console.log("RO_WRITTEN"); }}
                catch (e) {{ console.log("ro write:", e.message); }}
                await fs.promises.writeFile({rw:?} + "/sub/out.txt", "written");
                try {{ fs.readFile({other:?} + "/secret.txt"); console.log("OTHER_READ"); }}
                catch (e) {{ console.log("other:", e.message); }}
            "#,
            ro = ro.to_str().unwrap(),
            rw = rw.to_str().unwrap(),
            other = other.to_str().unwrap(),
        );
        let args = json!({
            "code": code,
            "allow_read": [ro],
            "allow_write": [rw],
        });
        let output = call(&root, args, true).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines[0], "outside text", "{output}");
        assert!(lines[1].contains("granted for reading only"), "{output}");
        assert!(lines[2].starts_with("other:"), "{output}");
        assert!(lines[2].contains("allow_read/allow_write"), "{output}");
        assert_eq!(
            std::fs::read_to_string(rw.join("sub/out.txt")).unwrap(),
            "written"
        );
        assert!(!ro.join("x.txt").exists());
        for dir in [root, ro, rw, other] {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Outside paths are refused to a call that did not go through approval,
    /// so calling the tool directly cannot skip the prompt; a declared path
    /// inside the project asks for nothing.
    #[test]
    fn outside_paths_need_approval() {
        let root = tmpdir();
        let args = json!({ "code": "1", "allow_read": ["/etc"] });
        assert!(NodeTool.requires_approval_in(&args, &root));
        assert!(NodeTool.requires_approval(&args));
        let err = call(&root, args, false).unwrap_err();
        assert!(err.to_string().contains("not approved"), "{err}");

        let inside = json!({ "code": "1", "allow_write": ["build", "./out/../x"] });
        assert!(!NodeTool.requires_approval_in(&inside, &root));
        assert_eq!(call(&root, inside, false).unwrap().trim(), "1");

        // Malformed: asks (and then fails), never read as no grant.
        for bad in [json!("/etc"), json!([5]), json!([""])] {
            let args = json!({ "code": "1", "allow_read": bad });
            assert!(NodeTool.requires_approval_in(&args, &root), "{args}");
            assert!(call(&root, args, true).is_err());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Grants are made absolute: relative to the root, `~` the home.
    #[test]
    fn grants_are_absolute_and_normalized() {
        let root = std::path::Path::new("/work/proj");
        let args = json!({
            "allow_read": ["../shared/./lib", "/etc/hosts"],
            "allow_write": ["~/notes", "sub"],
        });
        let grants = super::Grants::parse(&args, root).unwrap();
        assert_eq!(
            grants.read,
            [
                PathBuf::from("/work/shared/lib"),
                PathBuf::from("/etc/hosts")
            ]
        );
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(grants.write[0], PathBuf::from(home).join("notes"));
        }
        let outside = grants.outside(root);
        assert_eq!(outside.write.len(), 1, "the in-project grant is dropped");
        assert_eq!(outside.read.len(), 2);
    }
}
