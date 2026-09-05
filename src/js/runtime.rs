//! Glue for running user JS on an [`Interp`]: the bootstrap that assembles the
//! host objects, error rendering, and output truncation.

use kataan::Interp;
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

/// Cap on the returned output; longer output is truncated with a note.
const MAX_OUTPUT: usize = 10 * 1024;

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
