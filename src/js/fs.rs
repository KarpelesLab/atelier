//! The mediated `fs` host object for the `node` tool.
//!
//! Each method is registered as a mangled global (`__atelier_fs_*`) that
//! captures a clone of the project root. Every path argument is resolved with
//! [`crate::tools::confine`], so a script can only touch files inside the
//! project directory; an escaping path throws a catchable JS error. All calls
//! are **synchronous** (plain [`std::fs`]) — v1 has no async and no event loop.

use std::path::{Path, PathBuf};

use kataan::{Ctx, Interp, NanBox};

/// Read the first argument as a string path.
fn arg_path(cx: &mut Ctx, args: &[NanBox]) -> Result<String, NanBox> {
    cx.to_string(args.first().copied().unwrap_or_else(|| cx.undefined()))
}

/// Resolve `path` against `root`, turning a confinement failure into a
/// catchable JS error.
fn resolve(cx: &mut Ctx, root: &Path, path: &str) -> Result<PathBuf, NanBox> {
    crate::tools::confine(root, path).map_err(|e| cx.error(&e.to_string()))
}

/// Register the `__atelier_fs_*` global functions on `interp`. The bootstrap
/// program (run before user code) assembles them into `globalThis.fs`.
pub fn install(interp: &mut Interp, root: PathBuf) {
    let r = root.clone();
    interp.register_global_fn("__atelier_fs_readFile", 1, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let path = resolve(cx, &r, &p)?;
        let content =
            std::fs::read_to_string(&path).map_err(|e| cx.error(&format!("readFile: {e}")))?;
        Ok(cx.string(&content))
    });

    let r = root.clone();
    interp.register_global_fn("__atelier_fs_writeFile", 2, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let content = cx.to_string(args.get(1).copied().unwrap_or_else(|| cx.undefined()))?;
        let path = resolve(cx, &r, &p)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| cx.error(&format!("writeFile (mkdir): {e}")))?;
        }
        std::fs::write(&path, content).map_err(|e| cx.error(&format!("writeFile: {e}")))?;
        Ok(cx.undefined())
    });

    let r = root.clone();
    interp.register_global_fn("__atelier_fs_readdir", 1, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let path = resolve(cx, &r, &p)?;
        let entries = std::fs::read_dir(&path).map_err(|e| cx.error(&format!("readdir: {e}")))?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| cx.error(&format!("readdir: {e}")))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            names.push(cx.string(&name));
        }
        Ok(cx.new_array(names))
    });

    let r = root.clone();
    interp.register_global_fn("__atelier_fs_exists", 1, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let path = resolve(cx, &r, &p)?;
        Ok(cx.boolean(path.exists()))
    });

    let r = root;
    interp.register_global_fn("__atelier_fs_mkdir", 1, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let path = resolve(cx, &r, &p)?;
        std::fs::create_dir_all(&path).map_err(|e| cx.error(&format!("mkdir: {e}")))?;
        Ok(cx.undefined())
    });
}
