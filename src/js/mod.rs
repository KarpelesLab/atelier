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

use anyhow::Result;
use serde_json::{Value, json};

use crate::tools::{Tool, ToolCtx, ToolSpec};

/// The `node` tool: run JavaScript with a mediated, project-confined host.
pub struct NodeTool;

impl Tool for NodeTool {
    fn name(&self) -> &str {
        "node"
    }

    fn requires_approval(&self) -> bool {
        // v1 host is confined to the project root (fs) with no network, so the
        // tool cannot escape the project and never prompts.
        false
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "node".into(),
            description: "Run a JavaScript program in a sandboxed runtime. A mediated `fs` \
                (readFile, writeFile, readdir, exists, mkdir) confined to the project directory \
                and `console.log`/`console.error` are available. Use this for logic that would be \
                awkward as a shell one-liner. Returns the captured console output."
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

    fn call(&self, _ctx: &mut ToolCtx, _args: Value) -> Result<String> {
        anyhow::bail!("the `node` tool is not yet implemented")
    }
}

#[cfg(test)]
mod smoke {
    //! Confirms the kataan embedding API this module is built on.

    #[test]
    fn native_fn_and_run() {
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
}
