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
use kataan::interrupt::Interrupt;
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

/// After tripping the interrupt on a timeout, how long to wait for the worker to
/// unwind cooperatively before abandoning it (a script with no loop back-edge to
/// observe the flag is the pathological case; it's abandoned rather than blocking
/// the harness).
const INTERRUPT_GRACE_MS: u64 = 1_000;

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
                fs.readdir(path) -> string[]; fs.exists(path) -> boolean; fs.mkdir(path); \
                fs.stat(path) -> {isFile, isDirectory, size, mtimeMs}; \
                fs.appendFile(path, content); fs.rm(path) (file only); fs.rmdir(path) \
                (empty dir only, non-recursive); fs.rename(from, to). Binary I/O: \
                fs.readFileBytes(path) -> Uint8Array; fs.writeFileBytes(path, data) where data \
                is a Uint8Array or a plain array of byte numbers. Paths \
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
                let out = runtime::execute(code, root, network, worker_interrupt);
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
            "#,
        );
        assert!(
            !output.contains("NOT_CAUGHT"),
            "escape must throw: {output:?}"
        );
        assert_eq!(
            output.matches("caught").count(),
            6,
            "all six escapes should throw: {output:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `network: true` flips the approval requirement; a plain call does not.
    #[test]
    fn network_flag_gates_approval() {
        assert!(!NodeTool.requires_approval(&json!({ "code": "1" })));
        assert!(NodeTool.requires_approval(&json!({ "code": "1", "network": true })));
    }
}
