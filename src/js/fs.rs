//! The mediated `fs` host object for the `node` tool.
//!
//! Each method is registered as a mangled global (`__atelier_fs_*`) that
//! captures the call's [`Scope`]. Every path argument is resolved with
//! [`Scope::resolve`]: a path inside the project (checked by
//! [`crate::tools::confine`]) is always allowed, a path outside it only when
//! the call declared it in `allow_read`/`allow_write` (which makes the call
//! need approval — see [`super::Grants`]), and anything else throws a
//! catchable JS error. The methods are **synchronous** (plain [`std::fs`]);
//! `fs.promises` is a promise-shaped layer over them, built in the bootstrap
//! (see [`super::runtime::BOOTSTRAP`]).

use std::path::PathBuf;
use std::rc::Rc;

use globset::Glob;
use ignore::WalkBuilder;
use kataan::{Ctx, Interp, NanBox};

/// Cap on the number of paths [`__atelier_fs_glob`](install) returns, mirroring
/// the `glob` tool's own cap (`src/tools/glob.rs`) in spirit (a smaller number
/// here since script output also flows through the console-output truncation).
const GLOB_MAX_RESULTS: usize = 500;

/// What a method does with a path: a read needs a read or a write grant
/// outside the project, a write needs a write grant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    Read,
    Write,
}

/// Where a script's `fs` may go: the project, plus the outside paths the
/// (approved) call declared. Grants are absolute and lexically normalized,
/// like the paths they are checked against, and each covers itself and
/// everything under it.
#[derive(Clone, Debug)]
pub struct Scope {
    pub root: PathBuf,
    /// Outside paths that may be read.
    pub read: Vec<PathBuf>,
    /// Outside paths that may be read and written.
    pub write: Vec<PathBuf>,
}

impl Scope {
    /// Resolve `path` for `access`: inside the project it is resolved as the
    /// file tools resolve it; outside, it must fall under a grant that allows
    /// `access`. Lexical, like [`crate::tools::confine`]: symlinks are not
    /// followed.
    pub fn resolve(&self, path: &str, access: Access) -> Result<PathBuf, String> {
        let escape = match crate::tools::confine(&self.root, path) {
            Ok(p) => return Ok(p),
            Err(e) => e,
        };
        let abs = crate::tools::normalize(&self.root.join(path));
        let under = |grants: &[PathBuf]| grants.iter().any(|g| abs.starts_with(g));
        if under(&self.write) || (access == Access::Read && under(&self.read)) {
            return Ok(abs);
        }
        if access == Access::Write && under(&self.read) {
            return Err(format!(
                "path {path:?} is outside the project and granted for reading only \
                 (allow_write grants writing)"
            ));
        }
        Err(format!(
            "{escape} (reaching outside the project needs the path in the call's \
             allow_read/allow_write)"
        ))
    }
}

/// Read the first argument as a string path.
fn arg_path(cx: &mut Ctx, args: &[NanBox]) -> Result<String, NanBox> {
    cx.to_string(args.first().copied().unwrap_or_else(|| cx.undefined()))
}

/// Read argument `i` as a string.
fn arg_str(cx: &mut Ctx, args: &[NanBox], i: usize) -> Result<String, NanBox> {
    cx.to_string(args.get(i).copied().unwrap_or_else(|| cx.undefined()))
}

/// Resolve `path` within `scope` for `access`, turning a refusal into a
/// catchable JS error.
fn resolve(cx: &mut Ctx, scope: &Scope, path: &str, access: Access) -> Result<PathBuf, NanBox> {
    scope.resolve(path, access).map_err(|e| cx.error(&e))
}

/// Register the `__atelier_fs_*` global functions on `interp`. The bootstrap
/// program (run before user code) assembles them into `globalThis.fs`.
pub fn install(interp: &mut Interp, scope: Scope) {
    let scope = Rc::new(scope);
    let r = scope.clone();
    interp.register_global_fn("__atelier_fs_readFile", 1, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let path = resolve(cx, &r, &p, Access::Read)?;
        let content =
            std::fs::read_to_string(&path).map_err(|e| cx.error(&format!("readFile: {e}")))?;
        Ok(cx.string(&content))
    });

    let r = scope.clone();
    interp.register_global_fn("__atelier_fs_writeFile", 2, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let content = cx.to_string(args.get(1).copied().unwrap_or_else(|| cx.undefined()))?;
        let path = resolve(cx, &r, &p, Access::Write)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| cx.error(&format!("writeFile (mkdir): {e}")))?;
        }
        std::fs::write(&path, content).map_err(|e| cx.error(&format!("writeFile: {e}")))?;
        Ok(cx.undefined())
    });

    let r = scope.clone();
    interp.register_global_fn("__atelier_fs_readdir", 1, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let path = resolve(cx, &r, &p, Access::Read)?;
        let entries = std::fs::read_dir(&path).map_err(|e| cx.error(&format!("readdir: {e}")))?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| cx.error(&format!("readdir: {e}")))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            names.push(cx.string(&name));
        }
        Ok(cx.new_array(names))
    });

    let r = scope.clone();
    interp.register_global_fn("__atelier_fs_exists", 1, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let path = resolve(cx, &r, &p, Access::Read)?;
        Ok(cx.boolean(path.exists()))
    });

    let r = scope.clone();
    interp.register_global_fn("__atelier_fs_mkdir", 1, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let path = resolve(cx, &r, &p, Access::Write)?;
        std::fs::create_dir_all(&path).map_err(|e| cx.error(&format!("mkdir: {e}")))?;
        Ok(cx.undefined())
    });

    // stat(path) -> { isFile, isDirectory, size, mtimeMs }. Follows symlinks
    // (like Node's `fs.stat`). `mtimeMs` is the modified time in milliseconds
    // since the Unix epoch, or 0 when the platform can't report it.
    let r = scope.clone();
    interp.register_global_fn("__atelier_fs_stat", 1, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let path = resolve(cx, &r, &p, Access::Read)?;
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
    let r = scope.clone();
    interp.register_global_fn("__atelier_fs_appendFile", 2, move |cx, _this, args| {
        use std::io::Write;
        let p = arg_path(cx, args)?;
        let content = arg_str(cx, args, 1)?;
        let path = resolve(cx, &r, &p, Access::Write)?;
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
    let r = scope.clone();
    interp.register_global_fn("__atelier_fs_rm", 1, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let path = resolve(cx, &r, &p, Access::Write)?;
        if path.is_dir() {
            return Err(cx.error("rm: path is a directory (use rmdir)"));
        }
        std::fs::remove_file(&path).map_err(|e| cx.error(&format!("rm: {e}")))?;
        Ok(cx.undefined())
    });

    // rmdir(path) — remove an empty directory. NON-recursive for safety: a
    // non-empty directory (or a non-directory path) throws.
    let r = scope.clone();
    interp.register_global_fn("__atelier_fs_rmdir", 1, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let path = resolve(cx, &r, &p, Access::Write)?;
        std::fs::remove_dir(&path).map_err(|e| cx.error(&format!("rmdir: {e}")))?;
        Ok(cx.undefined())
    });

    // rename(from, to) — both paths need write access (the source goes away).
    let r = scope.clone();
    interp.register_global_fn("__atelier_fs_rename", 2, move |cx, _this, args| {
        let from = arg_path(cx, args)?;
        let to = arg_str(cx, args, 1)?;
        let from_path = resolve(cx, &r, &from, Access::Write)?;
        let to_path = resolve(cx, &r, &to, Access::Write)?;
        std::fs::rename(&from_path, &to_path).map_err(|e| cx.error(&format!("rename: {e}")))?;
        Ok(cx.undefined())
    });

    // readFileBytes(path) — returns the file's raw bytes as a plain JS array of
    // byte numbers (0..255). The bootstrap wraps this in `new Uint8Array(...)`
    // so the script sees a real Uint8Array (kataan exposes a working
    // `Uint8Array` global, as its own Buffer host relies on it).
    let r = scope.clone();
    interp.register_global_fn("__atelier_fs_readFileBytes", 1, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let path = resolve(cx, &r, &p, Access::Read)?;
        let bytes = std::fs::read(&path).map_err(|e| cx.error(&format!("readFileBytes: {e}")))?;
        let nums: Vec<NanBox> = bytes.iter().map(|&b| cx.number(f64::from(b))).collect();
        Ok(cx.new_array(nums))
    });

    // writeFileBytes(path, data) — write raw bytes, creating parents if needed.
    // The bootstrap normalizes `data` (a Uint8Array or a plain array of byte
    // numbers) into a plain array before calling this, so here we just read
    // `length` and each indexed element and coerce to a byte.
    let r = scope.clone();
    interp.register_global_fn("__atelier_fs_writeFileBytes", 2, move |cx, _this, args| {
        let p = arg_path(cx, args)?;
        let data = args.get(1).copied().unwrap_or_else(|| cx.undefined());
        let path = resolve(cx, &r, &p, Access::Write)?;

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

    // glob(pattern) -> string[] — list project files matching a glob pattern
    // (e.g. "src/**/*.rs"), respecting .gitignore. Mirrors `src/tools/glob.rs`:
    // walk the project root with `ignore::WalkBuilder` (which skips
    // gitignored/hidden-by-default entries the same way `git status` would),
    // match each file's project-relative path (or bare file name) against a
    // compiled `globset::Glob`, and return sorted, capped results.
    let r = scope.clone();
    interp.register_global_fn("__atelier_fs_glob", 1, move |cx, _this, args| {
        let pattern = arg_path(cx, args)?;
        let matcher = Glob::new(&pattern)
            .map_err(|e| cx.error(&format!("glob: invalid pattern {pattern:?}: {e}")))?
            .compile_matcher();

        let mut results = Vec::new();
        let walker = WalkBuilder::new(&r.root)
            .hidden(false)
            .filter_entry(crate::tools::spill::keep_in_walk(&r.root))
            .build();
        for entry in walker {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let file_path = entry.path();
            let rel = file_path.strip_prefix(&r.root).unwrap_or(file_path);
            if matcher.is_match(rel) || matcher.is_match(file_path.file_name().unwrap_or_default())
            {
                results.push(rel.to_string_lossy().into_owned());
                if results.len() >= GLOB_MAX_RESULTS {
                    break;
                }
            }
        }
        results.sort();

        let items: Vec<NanBox> = results.iter().map(|s| cx.string(s)).collect();
        Ok(cx.new_array(items))
    });

    // copyFile(src, dst) — copy a file; src needs read access, dst write access.
    // Creates dst's parent directories first (mirroring writeFile).
    let r = scope;
    interp.register_global_fn("__atelier_fs_copyFile", 2, move |cx, _this, args| {
        let src = arg_path(cx, args)?;
        let dst = arg_str(cx, args, 1)?;
        let src_path = resolve(cx, &r, &src, Access::Read)?;
        let dst_path = resolve(cx, &r, &dst, Access::Write)?;
        if let Some(parent) = dst_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| cx.error(&format!("copyFile (mkdir): {e}")))?;
        }
        std::fs::copy(&src_path, &dst_path).map_err(|e| cx.error(&format!("copyFile: {e}")))?;
        Ok(cx.undefined())
    });
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Access, Scope};

    fn scope() -> Scope {
        Scope {
            root: PathBuf::from("/work/proj"),
            read: vec![PathBuf::from("/data/ref")],
            write: vec![PathBuf::from("/tmp/out")],
        }
    }

    /// In the project anything goes; outside, only under a grant that
    /// allows the access, matched by whole components.
    #[test]
    fn resolve_by_grant_and_access() {
        let s = scope();
        let ok = |p: &str, a| s.resolve(p, a).ok();
        assert_eq!(
            ok("src/a.rs", Access::Write),
            Some(PathBuf::from("/work/proj/src/a.rs"))
        );
        assert_eq!(
            ok("/data/ref/x/y.txt", Access::Read),
            Some(PathBuf::from("/data/ref/x/y.txt"))
        );
        assert_eq!(ok("/data/ref/x.txt", Access::Write), None);
        assert_eq!(
            ok("../../tmp/out/a", Access::Write),
            Some(PathBuf::from("/tmp/out/a"))
        );
        assert_eq!(
            ok("/tmp/out/a", Access::Read),
            Some(PathBuf::from("/tmp/out/a"))
        );
        // A sibling sharing a prefix is not under the grant.
        assert_eq!(ok("/data/reference", Access::Read), None);
        // `..` is folded before the check.
        assert_eq!(ok("/data/ref/../secret", Access::Read), None);
        assert_eq!(ok("/etc/passwd", Access::Read), None);
    }
}
