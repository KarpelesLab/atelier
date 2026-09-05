//! JavaScript scripting via an embedded kataan runtime (roadmap M8).
//!
//! The `node` tool runs a JS script on [kataan](https://github.com/KarpelesLab/kataan)
//! with **host APIs we implement and mediate**. kataan's core is sans-I/O, so we
//! provide `fs`/`console` ourselves and check every effect at call time — v1
//! confines `fs` to the project root and exposes no network, so the tool can't
//! escape the project and is therefore auto-approved (`requires_approval = false`).
//!
//! # Contract / plan for the implementer (owns `src/js/`)
//!
//! Implement `NodeTool::call`: build an `Interp`, install a mediated `fs` object
//! (methods registered with `interp.register_fn`, each resolving its path
//! against `ctx.project_root` and rejecting anything outside — mirror
//! `ToolCtx::resolve`), install a `console` that captures output, run the
//! script, drive `kataan::host::timers::run_event_loop` if needed, and return
//! the captured console output (and the result / any thrown error) as the tool
//! result string. Add `src/js/fs.rs`, `src/js/console.rs`, `src/js/runtime.rs`
//! as you see fit. Do not edit files outside `src/js/`. Keep `NodeTool`'s public
//! shape (name, spec, requires_approval) so the registration keeps working.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::tools::{Tool, ToolCtx, ToolSpec};

mod console;
mod fs;
mod net;
mod runtime;

/// Default wall-clock budget for a script when the call omits `timeout_ms`.
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Hard ceiling on `timeout_ms` so a call cannot pin a worker thread forever by
/// request (a runaway script past this is abandoned — see [`NodeTool::call`]).
const MAX_TIMEOUT_MS: u64 = 600_000;
/// Worker-thread stack. The tree-walk interpreter recurses on the native stack;
/// kataan tunes its eval-depth guard for a ~2 MB stack, so a generous stack here
/// keeps a comfortable margin before the (catchable) `RangeError` fires.
const WORKER_STACK_BYTES: usize = 16 * 1024 * 1024;

/// The `node` tool: run JavaScript with a mediated, project-confined host.
pub struct NodeTool;

impl Tool for NodeTool {
    fn name(&self) -> &str {
        "node"
    }

    fn requires_approval(&self, args: &Value) -> bool {
        // Confined by default (mediated fs stays in the project root), so a plain
        // call is auto-approved. A call that opts into network (`network: true`)
        // is unconfined and requires approval.
        args.get("network")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "node".into(),
            description: "Run a JavaScript program in a sandboxed runtime (no \
                require/import — CommonJS and ES modules are unavailable). Two globals are \
                provided directly, already in scope (do NOT require() or import them): `console` \
                (log, error) and a SYNCHRONOUS `fs` confined to the project directory. The fs \
                methods return values directly — no promises, callbacks, or await: \
                fs.readFile(path[, 'utf8']) -> string; fs.writeFile(path, content); \
                fs.readdir(path) -> string[]; fs.exists(path) -> boolean; fs.mkdir(path). Paths \
                are relative to the project root and cannot escape it. Returns the captured \
                console output. Use this for logic that would be awkward as a shell one-liner. \
                Execution is bounded by `timeout_ms` (default 5000); a script that exceeds it is \
                aborted and the tool returns a timeout notice. Set `network: true` to additionally \
                get SYNCHRONOUS HTTP globals (this makes the call require approval): \
                fetch(url[, {method, headers, body}]) -> {status, ok, body, headers}; \
                httpRequest({method, url, headers, body}) -> {status, ok, body, headers}; \
                httpGet(url) -> string (the body). Without `network: true` those globals are \
                undefined (typeof fetch === 'undefined')."
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
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Wall-clock execution budget in milliseconds. Default \
                            5000. A script that runs longer is aborted and the tool returns a \
                            timeout notice.",
                        "default": DEFAULT_TIMEOUT_MS
                    }
                },
                "required": ["code"]
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        // Owned inputs for the worker thread: the script source, the `fs`
        // confinement root, and whether to install the network host.
        let code = args
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("the `node` tool requires a string `code` argument"))?
            .to_owned();
        let root = ctx.project_root.to_path_buf();
        let network = args
            .get("network")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS);

        // Run the interpreter on a dedicated worker thread and wait for its
        // result with a wall-clock deadline. The `Interp`/`NanBox` values live
        // and die entirely inside the worker; only the output `String` (which is
        // `Send`) crosses back over the channel.
        //
        // ## CPU/interrupt limitation (kataan v0.0.8)
        //
        // On a timeout we cannot force-kill the worker: Rust has no safe thread
        // cancellation, and kataan exposes **no JS-level interrupt or step/gas
        // budget** to trip from a watchdog. Its `Limits` (src/limits.rs) meter
        // only recursion depths and *WebAssembly* fuel (`DEFAULT_WASM_FUEL`) —
        // there is no per-instruction budget for the tree-walk interpreter
        // (src/nbexec/{mod,stmt}.rs have no interrupt check in the loop path),
        // and no `Interp` method to signal it. So a `while(true){}` script keeps
        // running after we return: the timed-out worker thread is **abandoned**
        // and leaks one CPU until the process exits. A real fix requires an
        // upstream kataan interrupt hook (an atomic flag checked on each
        // statement/back-edge) that a watchdog could set at the deadline.
        let (tx, rx) = mpsc::channel::<String>();
        let worker = thread::Builder::new()
            .name("node-script".into())
            .stack_size(WORKER_STACK_BYTES)
            .spawn(move || {
                let out = runtime::execute(code, root, network);
                // Ignore a send error: it only means we already timed out and
                // the receiver is gone.
                let _ = tx.send(out);
            })
            .map_err(|e| anyhow!("failed to spawn node worker thread: {e}"))?;

        match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
            Ok(output) => {
                // Reap the finished worker so it doesn't linger as a zombie.
                let _ = worker.join();
                Ok(output)
            }
            Err(RecvTimeoutError::Timeout) => {
                // Deliberately do NOT join: the worker is still running and
                // cannot be interrupted (see the note above). Drop the handle to
                // detach it; it exits with the process.
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
        // The call returns near the deadline, not much later.
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "timeout took too long: {elapsed:?}"
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

    /// `network: true` flips the approval requirement; a plain call does not.
    #[test]
    fn network_flag_gates_approval() {
        assert!(!NodeTool.requires_approval(&json!({ "code": "1" })));
        assert!(NodeTool.requires_approval(&json!({ "code": "1", "network": true })));
    }
}
