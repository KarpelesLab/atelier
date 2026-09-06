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

/// Read argument `i` as a string.
fn arg_str(cx: &mut Ctx, args: &[NanBox], i: usize) -> Result<String, NanBox> {
    cx.to_string(args.get(i).copied().unwrap_or_else(|| cx.undefined()))
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

    let r = root.clone();
    interp.register_global_fn("__atelier_fs_mkdir", 1, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let path = resolve(cx, &r, &p)?;
        std::fs::create_dir_all(&path).map_err(|e| cx.error(&format!("mkdir: {e}")))?;
        Ok(cx.undefined())
    });

    // stat(path) -> { isFile, isDirectory, size, mtimeMs }. Follows symlinks
    // (like Node's `fs.stat`). `mtimeMs` is the modified time in milliseconds
    // since the Unix epoch, or 0 when the platform can't report it.
    let r = root.clone();
    interp.register_global_fn("__atelier_fs_stat", 1, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let path = resolve(cx, &r, &p)?;
        let meta = std::fs::metadata(&path).map_err(|e| cx.error(&format!("stat: {e}")))?;
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0.0, |d| d.as_millis() as f64);

        let obj = cx.new_object();
        let is_file = cx.boolean(meta.is_file());
        cx.set_property(obj, "isFile", is_file)?;
        let is_dir = cx.boolean(meta.is_dir());
        cx.set_property(obj, "isDirectory", is_dir)?;
        let size = cx.number(meta.len() as f64);
        cx.set_property(obj, "size", size)?;
        let mtime = cx.number(mtime_ms);
        cx.set_property(obj, "mtimeMs", mtime)?;
        Ok(obj)
    });

    // appendFile(path, content) — append text, creating the file and any missing
    // parent directories first.
    let r = root.clone();
    interp.register_global_fn("__atelier_fs_appendFile", 2, move |cx, _this, args| {
        use std::io::Write;
        let p = arg_path(cx, args)?;
        let content = arg_str(cx, args, 1)?;
        let path = resolve(cx, &r, &p)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| cx.error(&format!("appendFile (mkdir): {e}")))?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| cx.error(&format!("appendFile: {e}")))?;
        f.write_all(content.as_bytes())
            .map_err(|e| cx.error(&format!("appendFile: {e}")))?;
        Ok(cx.undefined())
    });

    // rm(path) — remove a file. Errors if the path is a directory (use rmdir).
    let r = root.clone();
    interp.register_global_fn("__atelier_fs_rm", 1, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let path = resolve(cx, &r, &p)?;
        if path.is_dir() {
            return Err(cx.error("rm: path is a directory (use rmdir)"));
        }
        std::fs::remove_file(&path).map_err(|e| cx.error(&format!("rm: {e}")))?;
        Ok(cx.undefined())
    });

    // rmdir(path) — remove an empty directory. NON-recursive for safety: a
    // non-empty directory (or a non-directory path) throws.
    let r = root.clone();
    interp.register_global_fn("__atelier_fs_rmdir", 1, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let path = resolve(cx, &r, &p)?;
        std::fs::remove_dir(&path).map_err(|e| cx.error(&format!("rmdir: {e}")))?;
        Ok(cx.undefined())
    });

    // rename(from, to) — both paths are confined to the project root.
    let r = root.clone();
    interp.register_global_fn("__atelier_fs_rename", 2, move |cx, _this, args| {
        let from = arg_path(cx, args)?;
        let to = arg_str(cx, args, 1)?;
        let from_path = resolve(cx, &r, &from)?;
        let to_path = resolve(cx, &r, &to)?;
        std::fs::rename(&from_path, &to_path).map_err(|e| cx.error(&format!("rename: {e}")))?;
        Ok(cx.undefined())
    });

    // readFileBytes(path) — returns the file's raw bytes as a plain JS array of
    // byte numbers (0..255). The bootstrap wraps this in `new Uint8Array(...)`
    // so the script sees a real Uint8Array (kataan exposes a working
    // `Uint8Array` global, as its own Buffer host relies on it).
    let r = root.clone();
    interp.register_global_fn("__atelier_fs_readFileBytes", 1, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let path = resolve(cx, &r, &p)?;
        let bytes = std::fs::read(&path).map_err(|e| cx.error(&format!("readFileBytes: {e}")))?;
        let nums: Vec<NanBox> = bytes.iter().map(|&b| cx.number(f64::from(b))).collect();
        Ok(cx.new_array(nums))
    });

    // writeFileBytes(path, data) — write raw bytes, creating parents if needed.
    // The bootstrap normalizes `data` (a Uint8Array or a plain array of byte
    // numbers) into a plain array before calling this, so here we just read
    // `length` and each indexed element and coerce to a byte.
    let r = root;
    interp.register_global_fn("__atelier_fs_writeFileBytes", 2, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let data = args.get(1).copied().unwrap_or_else(|| cx.undefined());
        let path = resolve(cx, &r, &p)?;

        let len_nb = cx.get(data, "length")?;
        let len = cx.to_number(len_nb)?;
        if !(len.is_finite() && len >= 0.0) {
            return Err(cx.error("writeFileBytes: data must be an array-like of bytes"));
        }
        let len = len as usize;
        let mut bytes = Vec::with_capacity(len);
        for i in 0..len {
            let el = cx.get(data, &i.to_string())?;
            let n = cx.to_number(el)?;
            // Truncate to a byte the way Uint8Array assignment does.
            bytes.push((n as i64).rem_euclid(256) as u8);
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| cx.error(&format!("writeFileBytes (mkdir): {e}")))?;
        }
        std::fs::write(&path, bytes).map_err(|e| cx.error(&format!("writeFileBytes: {e}")))?;
        Ok(cx.undefined())
    });
}
