//! The `apply_patch` tool: apply a unified diff spanning one or more files in a
//! single atomic call.
//!
//! This is the natural way for a model to express a multi-file change: one
//! unified diff, applied all-or-nothing. Each file section is located, its hunks
//! are matched against the current file contents (with a forgiving fallback when
//! the `@@` line numbers don't line up), and the whole set is validated in memory
//! before anything is written. If any hunk cannot be applied unambiguously, the
//! entire call aborts and no file is touched.

#![allow(dead_code)]

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec};

/// One line inside a hunk.
#[derive(Debug)]
enum LineKind {
    Context,
    Add,
    Remove,
}

#[derive(Debug)]
struct HLine {
    kind: LineKind,
    text: String,
}

/// A single `@@ ... @@` hunk.
#[derive(Debug)]
struct Hunk {
    /// 1-based start line on the old side (a hint; `0` for pure insertions).
    old_start: usize,
    lines: Vec<HLine>,
}

impl Hunk {
    /// The "before" block: context + removed lines, in order.
    fn old_block(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter(|l| matches!(l.kind, LineKind::Context | LineKind::Remove))
            .map(|l| l.text.as_str())
            .collect()
    }

    /// The "after" block: context + added lines, in order.
    fn new_block(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter(|l| matches!(l.kind, LineKind::Context | LineKind::Add))
            .map(|l| l.text.as_str())
            .collect()
    }

    fn added(&self) -> usize {
        self.lines
            .iter()
            .filter(|l| matches!(l.kind, LineKind::Add))
            .count()
    }

    fn removed(&self) -> usize {
        self.lines
            .iter()
            .filter(|l| matches!(l.kind, LineKind::Remove))
            .count()
    }
}

/// One file's section within the patch.
#[derive(Debug)]
struct FilePatch {
    /// `None` for `/dev/null` (a creation).
    old_path: Option<String>,
    /// `None` for `/dev/null` (a deletion).
    new_path: Option<String>,
    hunks: Vec<Hunk>,
    /// Whether a `\ No newline at end of file` marker appeared in this section.
    saw_no_newline: bool,
}

/// Turn a `--- ` / `+++ ` header value into a target path, or `None` for
/// `/dev/null`. Strips a trailing tab-delimited timestamp and an `a/`,`b/`
/// prefix if present.
fn header_path(raw: &str) -> Option<String> {
    let raw = raw.split('\t').next().unwrap_or(raw).trim_end();
    if raw == "/dev/null" {
        return None;
    }
    let stripped = raw
        .strip_prefix("a/")
        .or_else(|| raw.strip_prefix("b/"))
        .unwrap_or(raw);
    Some(stripped.to_string())
}

/// Parse `-<start>[,<len>]` / `+<start>[,<len>]` into `(start, len)`; `len`
/// defaults to 1 when omitted.
fn parse_range(tok: &str) -> Result<(usize, usize)> {
    let mut it = tok.splitn(2, ',');
    let start: usize = it
        .next()
        .unwrap_or("")
        .parse()
        .with_context(|| format!("bad line number in hunk range {tok:?}"))?;
    let len: usize = match it.next() {
        Some(l) => l
            .parse()
            .with_context(|| format!("bad length in hunk range {tok:?}"))?,
        None => 1,
    };
    Ok((start, len))
}

/// Parse a `@@ -a,b +c,d @@` header into `(old_start, old_len, new_len)`.
fn parse_hunk_header(line: &str) -> Result<(usize, usize, usize)> {
    let rest = line
        .strip_prefix("@@")
        .ok_or_else(|| anyhow::anyhow!("not a hunk header: {line:?}"))?;
    let close = rest
        .find("@@")
        .ok_or_else(|| anyhow::anyhow!("malformed hunk header (missing closing '@@'): {line:?}"))?;
    let nums = rest[..close].trim();
    let mut old = None;
    let mut new = None;
    for tok in nums.split_whitespace() {
        if let Some(t) = tok.strip_prefix('-') {
            old = Some(parse_range(t)?);
        } else if let Some(t) = tok.strip_prefix('+') {
            new = Some(parse_range(t)?);
        }
    }
    let (old_start, old_len) =
        old.ok_or_else(|| anyhow::anyhow!("malformed hunk header (no '-' range): {line:?}"))?;
    let (_new_start, new_len) =
        new.ok_or_else(|| anyhow::anyhow!("malformed hunk header (no '+' range): {line:?}"))?;
    Ok((old_start, old_len, new_len))
}

/// Parse a whole unified diff into its per-file sections.
fn parse_patch(patch: &str) -> Result<Vec<FilePatch>> {
    let lines: Vec<&str> = patch.lines().collect();
    let mut files = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let Some(old_raw) = line.strip_prefix("--- ") else {
            // Skip anything that isn't a file header: `diff --git`, `index`,
            // mode lines, blank separators, preamble, etc.
            i += 1;
            continue;
        };
        let next = lines.get(i + 1).copied().ok_or_else(|| {
            anyhow::anyhow!("patch: '--- ' header is not followed by a '+++ ' line")
        })?;
        let new_raw = next.strip_prefix("+++ ").ok_or_else(|| {
            anyhow::anyhow!("patch: expected '+++ ' after '--- ' line, found {next:?}")
        })?;
        let old_path = header_path(old_raw);
        let new_path = header_path(new_raw);
        i += 2;

        let mut hunks = Vec::new();
        let mut saw_no_newline = false;
        while i < lines.len() && lines[i].starts_with("@@") {
            let (old_start, old_len, new_len) = parse_hunk_header(lines[i])?;
            i += 1;
            let mut old_rem = old_len;
            let mut new_rem = new_len;
            let mut hlines = Vec::new();
            while i < lines.len() && (old_rem > 0 || new_rem > 0) {
                let l = lines[i];
                if let Some(c) = l.strip_prefix(' ') {
                    hlines.push(HLine {
                        kind: LineKind::Context,
                        text: c.to_string(),
                    });
                    old_rem = old_rem.saturating_sub(1);
                    new_rem = new_rem.saturating_sub(1);
                } else if let Some(c) = l.strip_prefix('+') {
                    hlines.push(HLine {
                        kind: LineKind::Add,
                        text: c.to_string(),
                    });
                    new_rem = new_rem.saturating_sub(1);
                } else if let Some(c) = l.strip_prefix('-') {
                    hlines.push(HLine {
                        kind: LineKind::Remove,
                        text: c.to_string(),
                    });
                    old_rem = old_rem.saturating_sub(1);
                } else if l.starts_with('\\') {
                    // "\ No newline at end of file" — note and skip.
                    saw_no_newline = true;
                } else if l.is_empty() {
                    // A blank context line whose single leading space was
                    // stripped (common when patches are hand-edited).
                    hlines.push(HLine {
                        kind: LineKind::Context,
                        text: String::new(),
                    });
                    old_rem = old_rem.saturating_sub(1);
                    new_rem = new_rem.saturating_sub(1);
                } else {
                    break;
                }
                i += 1;
            }
            // Consume a trailing no-newline marker that follows the counted body.
            if i < lines.len() && lines[i].starts_with('\\') {
                saw_no_newline = true;
                i += 1;
            }
            hunks.push(Hunk {
                old_start,
                lines: hlines,
            });
        }

        if old_path.is_none() && new_path.is_none() {
            bail!("patch: a file section maps /dev/null to /dev/null");
        }
        files.push(FilePatch {
            old_path,
            new_path,
            hunks,
            saw_no_newline,
        });
    }

    if files.is_empty() {
        bail!(
            "patch: no file sections found. Expected unified-diff headers like \
            '--- a/path' / '+++ b/path'."
        );
    }
    Ok(files)
}

/// Find where `old_block` sits in `lines`. Tries the line-number hint first
/// (adjusted by the cumulative `offset` from earlier hunks), then falls back to
/// a unique content search. Returns the 0-based index at which to splice.
fn locate(
    lines: &[String],
    old_block: &[&str],
    old_start: usize,
    offset: isize,
    display: &str,
    hunk_no: usize,
) -> Result<usize> {
    // A pure insertion (no context, no removed lines): position by line number.
    if old_block.is_empty() {
        let idx = (old_start as isize - 1 + offset).clamp(0, lines.len() as isize);
        return Ok(idx as usize);
    }

    let matches_at = |start: usize| -> bool {
        lines[start..start + old_block.len()]
            .iter()
            .zip(old_block.iter())
            .all(|(a, b)| a == b)
    };

    // Try the (offset-adjusted) hint first — exact when hunks apply in order.
    let hint = old_start as isize - 1 + offset;
    if hint >= 0 {
        let h = hint as usize;
        if h + old_block.len() <= lines.len() && matches_at(h) {
            return Ok(h);
        }
    }

    // Fall back to searching for a unique occurrence of the block.
    let mut found = None;
    let mut count = 0usize;
    if old_block.len() <= lines.len() {
        for start in 0..=(lines.len() - old_block.len()) {
            if matches_at(start) {
                count += 1;
                if found.is_none() {
                    found = Some(start);
                }
                if count > 1 {
                    break;
                }
            }
        }
    }
    match (found, count) {
        (Some(s), 1) => Ok(s),
        (_, 0) => bail!(
            "apply_patch: hunk #{hunk_no} for {display:?} does not apply — its context/removed \
            lines were not found in the file"
        ),
        _ => bail!(
            "apply_patch: hunk #{hunk_no} for {display:?} is ambiguous — its context/removed \
            lines match more than one place in the file; add more surrounding context"
        ),
    }
}

/// Apply all hunks of a modification to `lines` in place.
fn apply_hunks(lines: &mut Vec<String>, hunks: &[Hunk], display: &str) -> Result<()> {
    let mut offset: isize = 0;
    for (n, hunk) in hunks.iter().enumerate() {
        let old_block = hunk.old_block();
        let new_block = hunk.new_block();
        let at = locate(lines, &old_block, hunk.old_start, offset, display, n + 1)?;
        let replacement: Vec<String> = new_block.iter().map(|s| s.to_string()).collect();
        let removed = old_block.len();
        lines.splice(at..at + removed, replacement);
        offset += new_block.len() as isize - removed as isize;
    }
    Ok(())
}

/// Build the contents of a newly-created file from its hunks.
fn build_new_file(fp: &FilePatch) -> String {
    let mut all: Vec<&str> = Vec::new();
    for h in &fp.hunks {
        all.extend(h.new_block());
    }
    let mut content = all.join("\n");
    if !all.is_empty() && !fp.saw_no_newline {
        content.push('\n');
    }
    content
}

/// A validated change, ready to be written once every file has passed.
enum Planned {
    Create {
        resolved: std::path::PathBuf,
        content: String,
        display: String,
        added: usize,
    },
    Modify {
        resolved: std::path::PathBuf,
        content: String,
        display: String,
        added: usize,
        removed: usize,
    },
    Delete {
        resolved: std::path::PathBuf,
        display: String,
        removed: usize,
    },
}

pub struct ApplyPatchTool;

impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn requires_approval(&self, _args: &Value) -> bool {
        // Every target path is confined to the project root by
        // `ToolCtx::resolve`, exactly like `edit`/`multiedit`; auto-approved.
        false
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "Apply a unified diff that may span several files in a single atomic \
                call — the natural way to make a coordinated multi-file change. The `patch` is a \
                standard unified diff: per-file sections beginning '--- a/<path>' and \
                '+++ b/<path>' (the 'a/' and 'b/' prefixes are optional), each followed by one \
                or more '@@ ... @@' hunks whose lines are ' ' context, '-' removed, and '+' \
                added. File creation is expressed with '--- /dev/null' and deletion with \
                '+++ /dev/null'. Hunks are located by their context and removed lines; if the \
                '@@' line numbers are off, the hunk is still applied wherever its context \
                uniquely matches. Any file being modified or deleted must have been read earlier \
                in this session and not have changed on disk since. The whole call is atomic: if \
                any hunk of any file cannot be applied unambiguously, nothing is written and a \
                clear error names the offending file and hunk."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "A unified diff covering one or more files. See the tool \
                            description for the supported shape (headers, hunks, /dev/null for \
                            create/delete)."
                    }
                },
                "required": ["patch"]
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let patch = args
            .get("patch")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required argument 'patch'"))?;

        let files = parse_patch(patch)?;

        // Plan every file change in memory, running all checks. Nothing is
        // written until every file's every hunk has applied cleanly.
        let mut plan: Vec<Planned> = Vec::with_capacity(files.len());
        for fp in &files {
            match (&fp.old_path, &fp.new_path) {
                // Creation: /dev/null -> new file.
                (None, Some(new_path)) => {
                    let resolved = ctx.resolve(new_path)?;
                    if resolved.exists() {
                        bail!(
                            "apply_patch: cannot create {new_path:?} — it already exists on disk"
                        );
                    }
                    let content = build_new_file(fp);
                    // Every line of a created file is an addition.
                    let added = fp.hunks.iter().flat_map(|h| h.new_block()).count();
                    plan.push(Planned::Create {
                        resolved,
                        content,
                        display: new_path.clone(),
                        added,
                    });
                }
                // Deletion: existing file -> /dev/null.
                (Some(old_path), None) => {
                    let resolved = ctx.resolve(old_path)?;
                    if !ctx.fstate.was_read(&resolved) {
                        bail!(
                            "apply_patch: {old_path:?} has not been read in this session yet; \
                            read it before deleting it via a patch"
                        );
                    }
                    let current = std::fs::read_to_string(&resolved)
                        .with_context(|| format!("reading {old_path:?}"))?;
                    if ctx.fstate.is_stale(&resolved, &current) {
                        bail!(
                            "apply_patch: {old_path:?} has changed on disk since it was last \
                            read; re-read it before patching"
                        );
                    }
                    let removed = fp.hunks.iter().map(Hunk::removed).sum();
                    plan.push(Planned::Delete {
                        resolved,
                        display: old_path.clone(),
                        removed,
                    });
                }
                // Modification: existing file -> existing file.
                (Some(old_path), Some(new_path)) => {
                    let target = new_path;
                    let resolved = ctx.resolve(target)?;
                    if !ctx.fstate.was_read(&resolved) {
                        bail!(
                            "apply_patch: {target:?} has not been read in this session yet; read \
                            it before editing"
                        );
                    }
                    let current = std::fs::read_to_string(&resolved)
                        .with_context(|| format!("reading {target:?}"))?;
                    if ctx.fstate.is_stale(&resolved, &current) {
                        bail!(
                            "apply_patch: {target:?} has changed on disk since it was last read; \
                            re-read it before editing"
                        );
                    }
                    let _ = old_path;
                    let mut lines: Vec<String> = current.split('\n').map(str::to_string).collect();
                    apply_hunks(&mut lines, &fp.hunks, target)?;
                    let content = lines.join("\n");
                    let added = fp.hunks.iter().map(Hunk::added).sum();
                    let removed = fp.hunks.iter().map(Hunk::removed).sum();
                    plan.push(Planned::Modify {
                        resolved,
                        content,
                        display: target.clone(),
                        added,
                        removed,
                    });
                }
                (None, None) => unreachable!("parse_patch rejects /dev/null -> /dev/null"),
            }
        }

        // Every file validated — now execute the writes.
        let mut created = 0;
        let mut modified = 0;
        let mut deleted = 0;
        let mut summary = String::new();
        for action in &plan {
            match action {
                Planned::Create {
                    resolved,
                    content,
                    display,
                    added,
                } => {
                    if let Some(parent) = resolved.parent() {
                        std::fs::create_dir_all(parent).with_context(|| {
                            format!("creating parent directories for {display:?}")
                        })?;
                    }
                    std::fs::write(resolved, content)
                        .with_context(|| format!("writing {display:?}"))?;
                    ctx.fstate.record(resolved, content);
                    created += 1;
                    summary.push_str(&format!("\n  created  {display} (+{added})"));
                }
                Planned::Modify {
                    resolved,
                    content,
                    display,
                    added,
                    removed,
                } => {
                    std::fs::write(resolved, content)
                        .with_context(|| format!("writing {display:?}"))?;
                    ctx.fstate.record(resolved, content);
                    modified += 1;
                    summary.push_str(&format!("\n  modified {display} (+{added} -{removed})"));
                }
                Planned::Delete {
                    resolved,
                    display,
                    removed,
                } => {
                    std::fs::remove_file(resolved)
                        .with_context(|| format!("deleting {display:?}"))?;
                    deleted += 1;
                    summary.push_str(&format!("\n  deleted  {display} (-{removed})"));
                }
            }
        }

        Ok(format!(
            "applied patch: {created} created, {modified} modified, {deleted} deleted{summary}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::FileState;
    use std::path::PathBuf;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atelier-test-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn single_file_hunk_modify() {
        let root = tempdir("apply-single");
        let file = root.join("f.txt");
        let orig = "line1\nline2\nline3\n";
        std::fs::write(&file, orig).unwrap();
        let mut fstate = FileState::new();
        fstate.record(&file, orig);
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let patch = "\
--- a/f.txt
+++ b/f.txt
@@ -1,3 +1,3 @@
 line1
-line2
+LINE2
 line3
";
        let out = ApplyPatchTool
            .call(&mut ctx, json!({ "patch": patch }))
            .unwrap();
        assert!(out.contains("1 modified"));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "line1\nLINE2\nline3\n"
        );
    }

    #[test]
    fn multi_file_patch() {
        let root = tempdir("apply-multi");
        let a = root.join("a.txt");
        let b = root.join("b.txt");
        std::fs::write(&a, "aaa\n").unwrap();
        std::fs::write(&b, "bbb\n").unwrap();
        let mut fstate = FileState::new();
        fstate.record(&a, "aaa\n");
        fstate.record(&b, "bbb\n");
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let patch = "\
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-aaa
+AAA
--- a/b.txt
+++ b/b.txt
@@ -1 +1 @@
-bbb
+BBB
";
        let out = ApplyPatchTool
            .call(&mut ctx, json!({ "patch": patch }))
            .unwrap();
        assert!(out.contains("2 modified"));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "AAA\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "BBB\n");
    }

    #[test]
    fn new_file_creation() {
        let root = tempdir("apply-create");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let patch = "\
--- /dev/null
+++ b/sub/new.txt
@@ -0,0 +1,2 @@
+hello
+world
";
        let out = ApplyPatchTool
            .call(&mut ctx, json!({ "patch": patch }))
            .unwrap();
        assert!(out.contains("1 created"));
        let created = root.join("sub/new.txt");
        assert_eq!(std::fs::read_to_string(&created).unwrap(), "hello\nworld\n");
        // Created file was recorded so a later edit won't be rejected.
        assert!(ctx.fstate.was_read(&created));
    }

    #[test]
    fn file_deletion() {
        let root = tempdir("apply-delete");
        let file = root.join("old.txt");
        std::fs::write(&file, "gone\n").unwrap();
        let mut fstate = FileState::new();
        fstate.record(&file, "gone\n");
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let patch = "\
--- a/old.txt
+++ /dev/null
@@ -1 +0,0 @@
-gone
";
        let out = ApplyPatchTool
            .call(&mut ctx, json!({ "patch": patch }))
            .unwrap();
        assert!(out.contains("1 deleted"));
        assert!(!file.exists());
    }

    #[test]
    fn fuzzy_application_when_line_numbers_off() {
        let root = tempdir("apply-fuzzy");
        let file = root.join("f.txt");
        let orig = "a\nb\nc\nd\ne\n";
        std::fs::write(&file, orig).unwrap();
        let mut fstate = FileState::new();
        fstate.record(&file, orig);
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        // The @@ line numbers are wildly wrong, but the context is unique.
        let patch = "\
--- a/f.txt
+++ b/f.txt
@@ -200,3 +200,3 @@
 b
-c
+C
 d
";
        ApplyPatchTool
            .call(&mut ctx, json!({ "patch": patch }))
            .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\nb\nC\nd\ne\n");
    }

    #[test]
    fn atomic_failure_leaves_first_file_untouched() {
        let root = tempdir("apply-atomic");
        let f1 = root.join("file1.txt");
        let f2 = root.join("file2.txt");
        std::fs::write(&f1, "one\n").unwrap();
        std::fs::write(&f2, "two\n").unwrap();
        let mut fstate = FileState::new();
        fstate.record(&f1, "one\n");
        fstate.record(&f2, "two\n");
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        // file1's hunk is valid; file2's context does not exist -> whole call aborts.
        let patch = "\
--- a/file1.txt
+++ b/file1.txt
@@ -1 +1 @@
-one
+ONE
--- a/file2.txt
+++ b/file2.txt
@@ -1 +1 @@
-nonexistent
+X
";
        let err = ApplyPatchTool
            .call(&mut ctx, json!({ "patch": patch }))
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("file2.txt"), "error should name file2: {msg}");
        assert!(msg.contains("hunk #1"), "error should name the hunk: {msg}");
        // Nothing written: file1 must still hold its original content.
        assert_eq!(std::fs::read_to_string(&f1).unwrap(), "one\n");
        assert_eq!(std::fs::read_to_string(&f2).unwrap(), "two\n");
    }

    #[test]
    fn requires_read_first_for_modify() {
        let root = tempdir("apply-unread");
        let file = root.join("f.txt");
        std::fs::write(&file, "x\n").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let patch = "\
--- a/f.txt
+++ b/f.txt
@@ -1 +1 @@
-x
+y
";
        let err = ApplyPatchTool
            .call(&mut ctx, json!({ "patch": patch }))
            .unwrap_err();
        assert!(err.to_string().contains("has not been read"));
        // Untouched.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x\n");
    }

    #[test]
    fn rejects_stale_read_for_modify() {
        let root = tempdir("apply-stale");
        let file = root.join("f.txt");
        std::fs::write(&file, "x\n").unwrap();
        let mut fstate = FileState::new();
        fstate.record(&file, "x\n");
        // File changes on disk after it was read.
        std::fs::write(&file, "changed\n").unwrap();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let patch = "\
--- a/f.txt
+++ b/f.txt
@@ -1 +1 @@
-changed
+y
";
        let err = ApplyPatchTool
            .call(&mut ctx, json!({ "patch": patch }))
            .unwrap_err();
        assert!(err.to_string().contains("changed on disk"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "changed\n");
    }

    #[test]
    fn rejects_path_escaping_root() {
        let root = tempdir("apply-escape");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let patch = "\
--- /dev/null
+++ b/../evil.txt
@@ -0,0 +1 @@
+pwned
";
        let err = ApplyPatchTool
            .call(&mut ctx, json!({ "patch": patch }))
            .unwrap_err();
        assert!(err.to_string().contains("escapes the project root"));
    }
}
