//! Tool output too long for the conversation goes to a file.
//!
//! A tool whose output can be any size (`bash`, `node`) used to cut it at a
//! budget, and whatever was past the cut was gone: to see it the agent had
//! to run the command again, differently. (MCP results weren't bounded at
//! all, which is worse for the conversation.) [`fit`] instead saves
//! the whole text under [`DIR`] in the project and returns its head and its
//! tail — where a command says what it is doing, and where it says how it
//! ended — around one line naming the file:
//!
//! ```text
//! … 1,842 lines omitted (lines 121-1962) — full output: .atelier/tool-output/ab12cd.txt (use read with offset/limit, or grep) …
//! ```
//!
//! The file is inside the project, so `read` and `grep` reach it without
//! approval, and those page natively. Tools that page by themselves (`read`,
//! `grep`, `web_fetch`) don't go through here.
//!
//! Two things keep this from becoming a nuisance:
//!
//! - The directory is bounded: writing a file removes the oldest ones past
//!   [`MAX_FILES`] files or [`MAX_DIR_BYTES`] in total. The file just written
//!   always stays, whatever its size.
//! - It is nobody's source: walks of the project (`grep`, `glob`, `tree`, the
//!   `node` tool's `fs.glob`) leave it out through [`keep_in_walk`] unless
//!   the walk was pointed at it. Not every project git-ignores `.atelier/`.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// Where spilled output is kept, relative to the project root.
pub const DIR: &str = ".atelier/tool-output";

/// The default budget, in bytes, for a tool result that has no better
/// number of its own.
pub const BUDGET: usize = 30_000;

/// How many files the directory keeps, and how many bytes in all.
const MAX_FILES: usize = 50;
const MAX_DIR_BYTES: u64 = 20 * 1024 * 1024;

/// The shares of the budget given to the head and the tail of an output
/// that doesn't fit. The rest is room for the marker and for a head or tail
/// that ends early at a line boundary.
const HEAD_PERCENT: usize = 60;
const TAIL_PERCENT: usize = 25;

/// `text` if it is within `budget` bytes. Otherwise the full text is saved
/// to a new file under [`DIR`] and what comes back is its head and tail,
/// each ending on a line boundary where there is one, with a marker between
/// them that names the file. If the file can't be written the result is cut
/// the same way and the marker says why there is no file.
pub fn fit(root: &Path, text: String, budget: usize) -> String {
    if text.len() <= budget {
        return text;
    }
    let head_end = head_end(&text, budget * HEAD_PERCENT / 100);
    let tail_start = tail_start(&text, budget * TAIL_PERCENT / 100).max(head_end);
    let (head, middle, tail) = (
        &text[..head_end],
        &text[head_end..tail_start],
        &text[tail_start..],
    );

    // Whole lines were left out if both cuts fell after a newline; then the
    // line numbers say where to `read`. A single enormous line has neither.
    let omitted = if head.ends_with('\n') && middle.ends_with('\n') {
        let first = head.lines().count() + 1;
        let n = middle.lines().count();
        format!(
            "{} lines omitted (lines {first}-{})",
            thousands(n),
            first + n - 1
        )
    } else {
        format!("{} bytes omitted", thousands(middle.len()))
    };
    let whereabouts = match save(root, &text) {
        Ok(path) => format!(
            "full output: {} (use read with offset/limit, or grep)",
            path.strip_prefix(root).unwrap_or(&path).display()
        ),
        Err(e) => format!("the full output could not be saved: {e}"),
    };

    let mut out = String::with_capacity(head.len() + tail.len() + 200);
    out.push_str(head);
    if !head.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("… {omitted} — {whereabouts} …\n"));
    out.push_str(tail);
    out
}

/// A `filter_entry` predicate for a walk of the project: everything but
/// the spill directory. The walk's own starting point is never filtered,
/// so naming the directory (or a file in it) still searches it.
pub fn keep_in_walk(
    project_root: &Path,
) -> impl Fn(&ignore::DirEntry) -> bool + Send + Sync + use<> {
    let dir = project_root.join(DIR);
    move |entry| entry.depth() == 0 || entry.path() != dir
}

/// Where the head ends: after the last newline within `max` bytes, or, with
/// no newline there, at `max` (moved back to a character boundary).
fn head_end(text: &str, max: usize) -> usize {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    match text[..end].rfind('\n') {
        Some(i) => i + 1,
        None => end,
    }
}

/// Where the tail starts: after the first newline within the last `max`
/// bytes, or, with no newline there, `max` bytes from the end (moved
/// forward to a character boundary).
fn tail_start(text: &str, max: usize) -> usize {
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    // The newline may be the byte just before the window.
    if start == 0 || text.as_bytes()[start - 1] == b'\n' {
        return start;
    }
    match text[start..].find('\n') {
        // A newline that ends the text leaves no tail; keep the partial line.
        Some(i) if start + i + 1 < text.len() => start + i + 1,
        _ => start,
    }
}

/// Write `text` to a new file in the spill directory, then trim the
/// directory around it.
fn save(root: &Path, text: &str) -> std::io::Result<PathBuf> {
    let dir = root.join(DIR);
    std::fs::create_dir_all(&dir)?;
    let path = loop {
        let candidate = dir.join(format!("{}.txt", short_id()));
        if !candidate.exists() {
            break candidate;
        }
    };
    std::fs::write(&path, text)?;
    prune(&dir, &path);
    Ok(path)
}

/// Six hex digits, different on every call: short enough to read and to
/// type back, and [`save`] checks the name is free anyway.
fn short_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    SystemTime::now().hash(&mut h);
    std::process::id().hash(&mut h);
    COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut h);
    format!("{:06x}", h.finish() & 0xff_ffff)
}

/// Delete the oldest spilled files until at most [`MAX_FILES`] remain and
/// they hold at most [`MAX_DIR_BYTES`]; `keep` (the one just written) is
/// never deleted. Best effort: a file that can't be examined or removed is
/// left for the next time.
fn prune(dir: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, PathBuf, u64)> = entries
        .flatten()
        .filter(|e| e.path() != keep && e.path().extension().is_some_and(|x| x == "txt"))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file()
                .then(|| (meta.modified().ok(), e.path(), meta.len()))
        })
        .filter_map(|(modified, path, len)| Some((modified?, path, len)))
        .collect();
    // Newest first; the name settles files written in the same instant.
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let mut count = 1;
    let mut bytes = std::fs::metadata(keep).map_or(0, |m| m.len());
    for (_, path, len) in files {
        count += 1;
        bytes += len;
        if count > MAX_FILES || bytes > MAX_DIR_BYTES {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// `1842` as `1,842`.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{FileState, Tool, ToolCtx, glob, grep, read, tree};
    use serde_json::json;
    use std::time::Duration;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atelier-test-spill-{}-{}-{}",
            name,
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn numbered(lines: usize) -> String {
        (1..=lines).map(|i| format!("line {i}\n")).collect()
    }

    /// The file a fitted result names.
    fn named_file(out: &str) -> String {
        let (_, rest) = out.split_once("full output: ").expect("a marker");
        rest.split_once(' ').unwrap().0.to_string()
    }

    fn spilled(root: &Path) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(root.join(DIR))
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        files.sort();
        files
    }

    #[test]
    fn output_within_the_budget_is_untouched_and_writes_nothing() {
        let root = tempdir("fits");
        let text = numbered(10);
        assert_eq!(fit(&root, text.clone(), text.len()), text);
        assert_eq!(fit(&root, String::new(), 0), "");
        assert!(!root.join(".atelier").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn long_output_keeps_head_and_tail_and_names_the_full_file() {
        let root = tempdir("long");
        let text = numbered(2000);
        let budget = 2000;
        let out = fit(&root, text.clone(), budget);

        assert!(out.len() <= budget, "{} bytes", out.len());
        // Head: whole lines, about 60% of the budget.
        assert!(out.starts_with("line 1\nline 2\n"));
        let (head, rest) = out.split_once("… ").unwrap();
        assert!(head.ends_with('\n'));
        assert!(head.len() <= budget * 60 / 100 && head.len() > budget * 55 / 100);
        // Tail: whole lines to the very end, about 25%.
        let (marker, tail) = rest.split_once(" …\n").unwrap();
        assert!(tail.ends_with("line 1999\nline 2000\n"));
        assert!(tail.starts_with("line "));
        assert!(tail.len() <= budget * 25 / 100 && tail.len() > budget * 20 / 100);

        // The marker counts what is missing and says where it is.
        let head_lines = head.lines().count();
        let tail_lines = tail.lines().count();
        let omitted = 2000 - head_lines - tail_lines;
        let file = named_file(&out);
        assert_eq!(
            marker,
            format!(
                "{} lines omitted (lines {}-{}) — full output: {file} \
                 (use read with offset/limit, or grep)",
                thousands(omitted),
                head_lines + 1,
                head_lines + omitted
            )
        );
        assert!(marker.starts_with("1,"), "{marker}");
        assert!(file.starts_with(".atelier/tool-output/") && file.ends_with(".txt"));
        assert_eq!(file.len(), ".atelier/tool-output/".len() + 6 + ".txt".len());
        assert_eq!(std::fs::read_to_string(root.join(&file)).unwrap(), text);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn output_without_line_breaks_is_cut_by_bytes_on_character_boundaries() {
        let root = tempdir("oneline");
        // Two-byte characters, so a cut at an odd byte would split one.
        let text = "é".repeat(5000);
        let out = fit(&root, text.clone(), 1001);
        let (head, rest) = out.split_once("\n… ").unwrap();
        let (marker, tail) = rest.split_once(" …\n").unwrap();
        assert_eq!(head, "é".repeat(300));
        assert_eq!(tail, "é".repeat(125));
        assert!(
            marker.starts_with("9,150 bytes omitted — full output: "),
            "{marker}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(named_file(&out))).unwrap(),
            text
        );

        // A long last line after short ones: the head is lines, the cut
        // into the last line is not.
        let text = format!("a\nb\n{}", "x".repeat(4000));
        let out = fit(&root, text, 1000);
        assert!(out.starts_with("a\nb\n"));
        assert!(out.contains(" bytes omitted — "), "{out}");
        assert!(out.ends_with(&"x".repeat(250)));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_unwritable_directory_still_cuts_and_says_so() {
        let root = tempdir("unwritable");
        // A file where the directory should be.
        std::fs::write(root.join(".atelier"), "in the way").unwrap();
        let out = fit(&root, numbered(2000), 2000);
        assert!(out.starts_with("line 1\n"));
        assert!(out.ends_with("line 2000\n"));
        assert!(
            out.contains(" lines omitted (lines ")
                && out.contains(" — the full output could not be saved: "),
            "{out}"
        );
        assert!(out.len() < 2200);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_directory_keeps_only_the_most_recent_files() {
        let root = tempdir("prune-count");
        let dir = root.join(DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now();
        // 60 older files, `old-00` the oldest, and one that isn't ours.
        for i in 0..60u64 {
            let path = dir.join(format!("old-{i:02}.txt"));
            std::fs::write(&path, "x").unwrap();
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(now - Duration::from_secs(1000 - i))
                .unwrap();
        }
        std::fs::write(dir.join("notes.md"), "kept").unwrap();

        let out = fit(&root, numbered(2000), 2000);
        let new = root.join(named_file(&out));
        let left = spilled(&root);
        let txt: Vec<&PathBuf> = left
            .iter()
            .filter(|p| p.extension().unwrap() == "txt")
            .collect();
        assert_eq!(txt.len(), MAX_FILES);
        assert!(new.is_file());
        // The 49 newest of the old ones stay: old-11 … old-59.
        assert!(!dir.join("old-10.txt").exists());
        assert!(dir.join("old-11.txt").exists());
        assert!(dir.join("old-59.txt").exists());
        assert!(dir.join("notes.md").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_directory_is_bounded_in_bytes_but_keeps_the_new_file() {
        let root = tempdir("prune-bytes");
        let dir = root.join(DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now();
        // Three 8 MiB files (sparse): with the new one, only two fit.
        for i in 0..3u64 {
            let path = dir.join(format!("big-{i}.txt"));
            let file = std::fs::File::create(&path).unwrap();
            file.set_len(8 * 1024 * 1024).unwrap();
            file.set_modified(now - Duration::from_secs(100 - i))
                .unwrap();
        }
        let out = fit(&root, numbered(2000), 2000);
        assert!(root.join(named_file(&out)).is_file());
        assert!(!dir.join("big-0.txt").exists());
        assert!(dir.join("big-1.txt").exists());
        assert!(dir.join("big-2.txt").exists());

        // A file over the whole allowance pushes everything else out, and
        // stays itself.
        let huge = dir.join("huge.txt");
        std::fs::File::create(&huge)
            .unwrap()
            .set_len(MAX_DIR_BYTES + 1)
            .unwrap();
        prune(&dir, &huge);
        assert_eq!(spilled(&root), vec![huge]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn read_and_grep_reach_the_file_without_approval_but_walks_leave_it_out() {
        reach(false);
    }

    /// The same where the project git-ignores `.atelier/`: the walkers then
    /// skip it by themselves, and naming the file must still get through.
    #[test]
    fn a_git_ignored_spill_directory_is_still_reachable_by_name() {
        reach(true);
    }

    fn reach(ignored: bool) {
        let root = tempdir(if ignored { "reach-ignored" } else { "reach" });
        if ignored {
            std::fs::create_dir(root.join(".git")).unwrap();
            std::fs::write(root.join(".gitignore"), ".atelier/\n").unwrap();
        }
        std::fs::write(root.join("src.txt"), "line 1500 lives here too\n").unwrap();
        let out = fit(&root, numbered(2000), 2000);
        let file = named_file(&out);

        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        // In the project: no prompt, and the omitted part is one call away.
        let args = json!({"path": file, "offset": 1500, "limit": 2});
        assert!(!read::ReadTool.requires_approval_in(&args, &root));
        let page = read::ReadTool.call(&mut ctx, args).unwrap();
        assert!(
            page.contains("1500\tline 1500") && page.contains("1501\tline 1501"),
            "{page}"
        );
        assert!(!page.contains("line 1502"));

        // Named, the file (or its directory) is searched…
        for path in [file.as_str(), DIR] {
            let hits = grep::GrepTool
                .call(&mut ctx, json!({"pattern": "^line 1500$", "path": path}))
                .unwrap();
            assert_eq!(hits, format!("{file}:1500:line 1500"));
        }
        // …but a walk of the project, or of `.atelier`, doesn't wander in.
        let hits = grep::GrepTool
            .call(&mut ctx, json!({"pattern": "line 1500"}))
            .unwrap();
        assert_eq!(hits, "src.txt:1:line 1500 lives here too");
        let hits = grep::GrepTool
            .call(
                &mut ctx,
                json!({"pattern": "line 1500", "path": ".atelier"}),
            )
            .unwrap();
        assert_eq!(hits, "no matches found");
        let files = glob::GlobTool
            .call(&mut ctx, json!({"pattern": "**/*.txt"}))
            .unwrap();
        assert_eq!(files, "src.txt");
        let listing = tree::TreeTool.call(&mut ctx, json!({"depth": 4})).unwrap();
        assert_eq!(listing.contains(".atelier/"), !ignored, "{listing}");
        assert!(!listing.contains("tool-output"), "{listing}");
        // Pointed at it, they list it.
        let files = glob::GlobTool
            .call(&mut ctx, json!({"pattern": "*.txt", "path": DIR}))
            .unwrap();
        assert_eq!(files, file);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1842), "1,842");
        assert_eq!(thousands(1_000_000), "1,000,000");
        assert_eq!(thousands(12_345_678), "12,345,678");
    }
}
