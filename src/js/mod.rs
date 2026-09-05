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

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use kataan::Interp;

use crate::tools::{Tool, ToolCtx, ToolSpec};

mod console;
mod fs;
mod runtime;

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
            description: "Run a JavaScript program in a sandboxed runtime (no network; no \
                require/import — CommonJS and ES modules are unavailable). Two globals are \
                provided directly, already in scope (do NOT require() or import them): `console` \
                (log, error) and a SYNCHRONOUS `fs` confined to the project directory. The fs \
                methods return values directly — no promises, callbacks, or await: \
                fs.readFile(path[, 'utf8']) -> string; fs.writeFile(path, content); \
                fs.readdir(path) -> string[]; fs.exists(path) -> boolean; fs.mkdir(path). Paths \
                are relative to the project root and cannot escape it. Returns the captured \
                console output. Use this for logic that would be awkward as a shell one-liner."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "The JavaScript source to run." }
                },
                "required": ["code"]
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        // 1. The script source.
        let code = args
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("the `node` tool requires a string `code` argument"))?;

        // 2. The confinement root for the mediated `fs`.
        let root = ctx.project_root.to_path_buf();

        // Parse both programs up front so they outlive `interp` (the interpreter
        // borrows a program's source for its own lifetime; declaring the
        // programs first keeps that borrow valid). A user syntax error is
        // reported as a normal result, not a tool failure.
        let bootstrap = runtime::parse(runtime::BOOTSTRAP)
            .map_err(|e| anyhow!("internal bootstrap parse error: {e}"))?;
        let user_prog = match runtime::parse(code) {
            Ok(p) => p,
            Err(msg) => return Ok(msg),
        };

        // 3. Build the interpreter, install the mediated host, assemble it in JS,
        // then run the user's code on the same interpreter.
        //
        // TODO(v1 limitation): there is no execution timeout — an infinite-loop
        // script will hang the calling thread. A future version should run the
        // interpreter under a watchdog / step budget.
        let mut interp = Interp::new();
        fs::install(&mut interp, root);
        let out = console::install(&mut interp);

        if let Err(e) = interp.run(&bootstrap) {
            return Ok(format!(
                "internal error assembling host: {}",
                runtime::render_exec_error(&interp, &e)
            ));
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
                let msg = runtime::render_exec_error(&interp, &e);
                if !output.is_empty() && !output.ends_with('\n') {
                    output.push('\n');
                }
                output.push_str(&format!("Uncaught {msg}"));
            }
        }

        Ok(runtime::truncate(output))
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
}
