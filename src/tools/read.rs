//! The `read` tool: read a file's contents with `cat -n` style line numbers.
//!
//! Beyond plain text it handles what a coding agent runs into: images are
//! returned as images (the model sees them), PDFs as their extracted text
//! (pure Rust, via `oxideav-pdf`; page ranges supported), and Jupyter notebooks
//! as rendered cells with their outputs. Reading outside the project is
//! possible but needs approval ([`Tool::requires_approval_in`]) — e.g. a
//! screenshot path the user pasted, or a temp file another tool wrote.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolImage, ToolOutput, ToolSpec, base64_encode, normalize, path_arg};

/// Largest image returned inline (the model API's per-image limit, with
/// headroom for base64's growth).
const MAX_IMAGE_BYTES: u64 = 3_750_000;

pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn requires_approval(&self, _args: &serde_json::Value) -> bool {
        false
    }

    /// Inside the project: never. Outside it: yes.
    fn requires_approval_in(&self, args: &Value, root: &Path) -> bool {
        path_arg(args).is_ok_and(|p| super::confine(root, p).is_err())
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "Read a file, returning its contents with 1-based line numbers \
                (offset/limit read a slice of a large file). Images (png, jpg, gif, webp) \
                are returned as images you can see; PDFs as extracted text (use `pages` \
                for large ones); Jupyter notebooks (.ipynb) as cells with their outputs. \
                Paths are relative to the project root; files outside the project can be \
                read with the user's approval."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file, relative to the project root (or absolute)."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "1-based line number to start reading from (default 1)."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to return (default: all)."
                    },
                    "pages": {
                        "type": "string",
                        "description": "PDF only: page range to extract, e.g. \"3\" or \"1-5\" (default: all)."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        self.call_output(ctx, args).map(|o| o.text)
    }

    fn call_output(&self, ctx: &mut ToolCtx, args: Value) -> Result<ToolOutput> {
        let path = path_arg(&args)?;
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize);

        let resolved = resolve(ctx, path)?;
        if resolved.is_dir() {
            bail!("{path:?} is a directory, not a file");
        }
        let ext = resolved
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();

        if let Some(mime) = image_mime(&ext) {
            return read_image(&resolved, path, mime);
        }
        let text = match ext.as_str() {
            "pdf" => pdf_text(&resolved, args.get("pages").and_then(Value::as_str))?,
            "ipynb" => {
                let raw = read_text(&resolved, path)?;
                // Record the raw JSON: that's what notebook_edit compares.
                ctx.fstate.record(&resolved, &raw);
                render_notebook(&raw).with_context(|| format!("parsing notebook {path:?}"))?
            }
            _ => {
                let contents = read_text(&resolved, path)?;
                // Record the FULL contents so Edit can detect staleness correctly.
                ctx.fstate.record(&resolved, &contents);
                contents
            }
        };
        Ok(ToolOutput::text(number_lines(&text, offset, limit)))
    }
}

/// Resolve `path`: confined to the project, unless this call was approved
/// (which [`ReadTool::requires_approval_in`] demands for outside paths).
fn resolve(ctx: &ToolCtx, path: &str) -> Result<PathBuf> {
    match ctx.resolve(path) {
        Ok(p) => Ok(p),
        Err(_) if ctx.approved => {
            let p = Path::new(path);
            Ok(normalize(&if p.is_absolute() {
                p.to_path_buf()
            } else {
                ctx.project_root.join(p)
            }))
        }
        Err(e) => Err(e),
    }
}

fn read_text(resolved: &Path, path: &str) -> Result<String> {
    std::fs::read_to_string(resolved).map_err(|e| anyhow::anyhow!("failed to read {path:?}: {e}"))
}

/// `cat -n` style numbering of the lines in `[offset, offset + limit)`.
fn number_lines(text: &str, offset: usize, limit: Option<usize>) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let start = offset.min(lines.len() + 1);
    let end = match limit {
        Some(l) => (start + l - 1).min(lines.len()),
        None => lines.len(),
    };

    let mut out = String::new();
    for (i, line) in lines
        .iter()
        .enumerate()
        .skip(start.saturating_sub(1))
        .take(end.saturating_sub(start.saturating_sub(1)))
    {
        out.push_str(&format!("{}\t{}\n", i + 1, line));
    }
    out
}

fn image_mime(ext: &str) -> Option<&'static str> {
    match ext {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

fn read_image(resolved: &Path, path: &str, mime: &str) -> Result<ToolOutput> {
    let size = std::fs::metadata(resolved)
        .map_err(|e| anyhow::anyhow!("failed to read {path:?}: {e}"))?
        .len();
    if size > MAX_IMAGE_BYTES {
        bail!(
            "{path:?} is {size} bytes, too large to view (max {MAX_IMAGE_BYTES}); \
             scale it down first (e.g. with ImageMagick via bash)"
        );
    }
    let bytes =
        std::fs::read(resolved).map_err(|e| anyhow::anyhow!("failed to read {path:?}: {e}"))?;
    Ok(ToolOutput {
        text: format!("[image {path}: {mime}, {size} bytes]"),
        images: vec![ToolImage {
            mime: mime.to_string(),
            data: base64_encode(&bytes),
        }],
    })
}

/// Text of a PDF (or of `pages`, e.g. "3" or "2-5"), page by page.
///
/// `oxideav-pdf` yields positioned text runs; each page's runs are grouped
/// into lines by baseline (top to bottom) and ordered left to right within
/// a line. Runs on a line are concatenated as-is: producers put real space
/// glyphs (or `TJ` gaps, which the extractor turns into spaces) between
/// words, so adding separators would split words that span runs.
fn pdf_text(resolved: &Path, pages: Option<&str>) -> Result<String> {
    let range = pages.map(parse_pages).transpose()?;
    let bytes =
        std::fs::read(resolved).with_context(|| format!("reading {}", resolved.display()))?;
    let mut reader = oxideav_pdf::reader::DocumentReader::open(&bytes)
        .map_err(|e| anyhow::anyhow!("not a readable PDF: {e}"))?;
    let extraction = reader
        .marked_text_extraction()
        .map_err(|e| anyhow::anyhow!("extracting PDF text: {e}"))?;

    let mut by_page: BTreeMap<u32, Vec<&oxideav_pdf::TextRun>> = BTreeMap::new();
    for m in &extraction.runs {
        let page = m.page_index + 1;
        if range.is_none_or(|(first, last)| (first..=last).contains(&page)) {
            by_page.entry(page).or_default().push(&m.run);
        }
    }

    let mut out = String::new();
    for (page, runs) in by_page {
        out.push_str(&format!("[page {page}]\n"));
        for line in layout_lines(runs) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    if out.trim().is_empty() {
        return Ok("[no extractable text: the PDF may be scanned images]".to_string());
    }
    Ok(out)
}

/// A page's runs as text lines, in the order the page draws them — which
/// is reading order for the producers that matter (and keeps columns
/// apart), and doesn't depend on which way the page's y axis points (run
/// positions don't include the page's transform, and many producers flip
/// it). A run whose baseline differs from the current line's (by more than
/// half the font size) starts a new line; runs within a line are ordered
/// left to right.
fn layout_lines(runs: Vec<&oxideav_pdf::TextRun>) -> Vec<String> {
    let mut lines: Vec<(f32, Vec<&oxideav_pdf::TextRun>)> = Vec::new();
    for run in runs {
        let tolerance = (run.font_size.abs() * 0.5).max(1.0);
        match lines.last_mut() {
            Some((y, line)) if (*y - run.position.1).abs() <= tolerance => line.push(run),
            _ => lines.push((run.position.1, vec![run])),
        }
    }
    lines
        .into_iter()
        .filter_map(|(_, mut line)| {
            line.sort_by(|a, b| a.position.0.total_cmp(&b.position.0));
            let text: String = line.iter().map(|r| r.text.as_str()).collect();
            let text = expand_ligatures(&text);
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            (!text.is_empty()).then_some(text)
        })
        .collect()
}

/// Spell out typographic ligature glyphs (ﬁ → fi, …), so the text reads
/// and searches like what it says.
fn expand_ligatures(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{FB00}' => out.push_str("ff"),
            '\u{FB01}' => out.push_str("fi"),
            '\u{FB02}' => out.push_str("fl"),
            '\u{FB03}' => out.push_str("ffi"),
            '\u{FB04}' => out.push_str("ffl"),
            '\u{FB05}' | '\u{FB06}' => out.push_str("st"),
            c => out.push(c),
        }
    }
    out
}

/// "3" -> (3, 3); "2-5" -> (2, 5).
fn parse_pages(pages: &str) -> Result<(u32, u32)> {
    let parse = |s: &str| {
        s.trim()
            .parse::<u32>()
            .ok()
            .filter(|&n| n > 0)
            .ok_or_else(|| {
                anyhow::anyhow!("invalid page range {pages:?} (use e.g. \"3\" or \"1-5\")")
            })
    };
    let (first, last) = match pages.split_once('-') {
        Some((a, b)) => (parse(a)?, parse(b)?),
        None => {
            let n = parse(pages)?;
            (n, n)
        }
    };
    if last < first {
        bail!("invalid page range {pages:?}");
    }
    Ok((first, last))
}

/// Render a notebook's cells (and outputs) as readable text.
fn render_notebook(raw: &str) -> Result<String> {
    let nb: Value = serde_json::from_str(raw)?;
    let cells = nb["cells"].as_array().context("no `cells` array")?;
    let mut out = String::new();
    for (i, cell) in cells.iter().enumerate() {
        let kind = cell["cell_type"].as_str().unwrap_or("?");
        out.push_str(&format!("<cell {i}"));
        if let Some(id) = cell["id"].as_str() {
            out.push_str(&format!(" id=\"{id}\""));
        }
        out.push_str(&format!(" type={kind}>\n"));
        out.push_str(&joined(&cell["source"]));
        if !out.ends_with('\n') {
            out.push('\n');
        }
        for output in cell["outputs"].as_array().into_iter().flatten() {
            out.push_str(&render_output(output));
        }
        out.push_str("</cell>\n");
    }
    Ok(out)
}

fn render_output(output: &Value) -> String {
    let body = match output["output_type"].as_str() {
        Some("stream") => joined(&output["text"]),
        Some("error") => format!(
            "{}: {}",
            output["ename"].as_str().unwrap_or("error"),
            output["evalue"].as_str().unwrap_or("")
        ),
        _ => {
            let data = &output["data"];
            if !data["text/plain"].is_null() {
                joined(&data["text/plain"])
            } else if let Some(mime) = data
                .as_object()
                .and_then(|d| d.keys().find(|k| k.starts_with("image/")))
            {
                format!("[{mime} output]")
            } else {
                return String::new();
            }
        }
    };
    let mut s = format!("<output>\n{body}");
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("</output>\n");
    s
}

/// A notebook multiline string: either a string or an array of lines.
fn joined(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
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
    fn reads_with_line_numbers() {
        let root = tempdir("read-basic");
        std::fs::write(root.join("f.txt"), "a\nb\nc\n").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let out = ReadTool.call(&mut ctx, json!({"path": "f.txt"})).unwrap();
        assert_eq!(out, "1\ta\n2\tb\n3\tc\n");
        assert!(fstate.was_read(&root.join("f.txt")));
    }

    #[test]
    fn offset_and_limit() {
        let root = tempdir("read-offset");
        std::fs::write(root.join("f.txt"), "a\nb\nc\nd\n").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let out = ReadTool
            .call(&mut ctx, json!({"path": "f.txt", "offset": 2, "limit": 2}))
            .unwrap();
        assert_eq!(out, "2\tb\n3\tc\n");
    }

    #[test]
    fn missing_file_errors() {
        let root = tempdir("read-missing");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        assert!(
            ReadTool
                .call(&mut ctx, json!({"path": "nope.txt"}))
                .is_err()
        );
    }

    #[test]
    fn directory_errors() {
        let root = tempdir("read-dir");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        assert!(ReadTool.call(&mut ctx, json!({"path": "sub"})).is_err());
    }

    #[test]
    fn path_escape_rejected() {
        let root = tempdir("read-escape");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        assert!(
            ReadTool
                .call(&mut ctx, json!({"path": "../etc/passwd"}))
                .is_err()
        );
    }

    #[test]
    fn outside_needs_approval_and_reads_once_approved() {
        let root = tempdir("read-outside");
        let other = tempdir("read-outside-other");
        std::fs::write(other.join("o.txt"), "outside\n").unwrap();
        let abs = other.join("o.txt").to_string_lossy().into_owned();
        assert!(ReadTool.requires_approval_in(&json!({"path": abs}), &root));
        assert!(!ReadTool.requires_approval_in(&json!({"path": "in.txt"}), &root));
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: true,
        };
        let out = ReadTool.call(&mut ctx, json!({"path": abs})).unwrap();
        assert_eq!(out, "1\toutside\n");
    }

    #[test]
    fn accepts_file_path_alias() {
        let root = tempdir("read-alias");
        std::fs::write(root.join("f.txt"), "x\n").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let out = ReadTool
            .call(&mut ctx, json!({"file_path": "f.txt"}))
            .unwrap();
        assert_eq!(out, "1\tx\n");
    }

    #[test]
    fn images_come_back_as_images() {
        let root = tempdir("read-image");
        let png = b"\x89PNG\r\n\x1a\nfake";
        std::fs::write(root.join("p.png"), png).unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let out = ReadTool
            .call_output(&mut ctx, json!({"path": "p.png"}))
            .unwrap();
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].mime, "image/png");
        assert_eq!(out.images[0].data, base64_encode(png));
        assert!(out.text.contains("image/png"));
    }

    #[test]
    fn notebooks_render_cells_and_outputs() {
        let nb = json!({
            "cells": [
                {"cell_type": "markdown", "id": "m1", "source": ["# Title\n", "text"]},
                {"cell_type": "code", "id": "c1", "source": "print(1)",
                 "outputs": [{"output_type": "stream", "text": ["1\n"]},
                             {"output_type": "display_data", "data": {"image/png": "AA=="}}]}
            ],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5
        });
        let r = render_notebook(&nb.to_string()).unwrap();
        assert_eq!(
            r,
            "<cell 0 id=\"m1\" type=markdown>\n# Title\ntext\n</cell>\n\
             <cell 1 id=\"c1\" type=code>\nprint(1)\n<output>\n1\n</output>\n\
             <output>\n[image/png output]\n</output>\n</cell>\n"
        );
    }

    #[test]
    fn page_ranges() {
        assert_eq!(parse_pages("3").unwrap(), (3, 3));
        assert_eq!(parse_pages("2-5").unwrap(), (2, 5));
        assert!(parse_pages("5-2").is_err());
        assert!(parse_pages("0").is_err());
        assert!(parse_pages("x").is_err());
    }

    #[test]
    fn reads_pdf_text() {
        // A one-page PDF saying "ZEBRA42".
        let stream = b"BT /F1 24 Tf 20 40 Td (ZEBRA42) Tj ET";
        let objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(),
            [
                format!("<< /Length {} >>\nstream\n", stream.len()).into_bytes(),
                stream.to_vec(),
                b"\nendstream".to_vec(),
            ]
            .concat(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend(format!("{} 0 obj\n", i + 1).bytes());
            pdf.extend(o);
            pdf.extend(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
        for o in offsets {
            pdf.extend(format!("{o:010} 00000 n \n").bytes());
        }
        pdf.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .bytes(),
        );
        let root = tempdir("read-pdf");
        std::fs::write(root.join("d.pdf"), pdf).unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let out = ReadTool.call(&mut ctx, json!({"path": "d.pdf"})).unwrap();
        assert!(out.contains("[page 1]") && out.contains("ZEBRA42"), "{out}");
        // A range past the last page: nothing to extract.
        let out = ReadTool
            .call(&mut ctx, json!({"path": "d.pdf", "pages": "2-3"}))
            .unwrap();
        assert!(!out.contains("ZEBRA42"), "{out}");
        // Not a PDF at all.
        std::fs::write(root.join("bad.pdf"), "nope").unwrap();
        assert!(ReadTool.call(&mut ctx, json!({"path": "bad.pdf"})).is_err());
    }

    #[test]
    fn pdf_runs_become_lines() {
        let run = |text: &str, x: f32, y: f32| oxideav_pdf::TextRun {
            text: text.into(),
            position: (x, y),
            font_name: "F1".into(),
            font_size: 10.0,
            render_mode: Default::default(),
            text_rise: 0.0,
        };
        // Lines come out in drawing order whichever way y points (this page
        // draws its first line at the larger y; a flipped one wouldn't);
        // within a line, runs are ordered by x, with their own space glyphs
        // (one baseline slightly jittered); blank lines are dropped.
        let runs = [
            run("e\u{FB00}ect", 40.0, 100.0),
            run("ond ", 15.0, 100.0),
            run("Sec", 0.0, 100.0),
            run("Line", 0.0, 80.0),
            run("ne", 25.0, 80.0),
            run(" li", 20.0, 80.4),
            run("d", 30.0, 80.0),
            run("   ", 50.0, 60.0),
            run("Third", 0.0, 120.0),
        ];
        assert_eq!(
            layout_lines(runs.iter().collect()),
            ["Second effect", "Line lined", "Third"]
        );
    }
}
