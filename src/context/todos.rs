//! Open TODOs provider: a bounded, gitignore-aware scan for `TODO`/`FIXME`/
//! `XXX` markers left in the project's source files.

use super::{ContextItem, ContextProvider};
use ignore::WalkBuilder;
use regex::Regex;
use std::io::BufRead;
use std::path::Path;

/// Scans the project (respecting `.gitignore`, text files only) for
/// `TODO`/`FIXME`/`XXX` markers and reports them as a compact
/// [`ContextItem`]. Returns `None` when none are found. The scan is bounded
/// (files, lines, and matches) so it stays cheap on large trees.
pub struct TodosProvider;

const MAX_FILES: usize = 500;
const MAX_TOTAL_LINES: usize = 20_000;
const MAX_MATCHES: usize = 100;
const MAX_SHOWN: usize = 10;
const MAX_FILE_SIZE: u64 = 256 * 1024;

impl ContextProvider for TodosProvider {
    fn name(&self) -> &str {
        "todos"
    }

    fn gather(&self, root: &Path) -> Option<ContextItem> {
        let matches = scan_todos(root);
        if matches.is_empty() {
            return None;
        }
        Some(ContextItem::new("open TODOs", render_todos(&matches), 50))
    }
}

/// One `TODO`/`FIXME`/`XXX` occurrence.
struct TodoMatch {
    path: String,
    line: usize,
    marker: &'static str,
    text: String,
}

fn todo_regex() -> Regex {
    Regex::new(r"\b(TODO|FIXME|XXX)\b").expect("valid regex")
}

/// Walks `root` (respecting `.gitignore`, skipping hidden dirs, large files,
/// and non-UTF8/binary files) collecting marker occurrences, bounded by
/// [`MAX_FILES`], [`MAX_TOTAL_LINES`], and [`MAX_MATCHES`].
fn scan_todos(root: &Path) -> Vec<TodoMatch> {
    let re = todo_regex();
    let mut matches = Vec::new();
    let mut files_scanned = 0usize;
    let mut lines_scanned = 0usize;

    let walker = WalkBuilder::new(root).hidden(true).git_ignore(true).build();

    for result in walker {
        if files_scanned >= MAX_FILES
            || lines_scanned >= MAX_TOTAL_LINES
            || matches.len() >= MAX_MATCHES
        {
            break;
        }
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let size = match entry.metadata() {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        if size > MAX_FILE_SIZE {
            continue;
        }

        let path = entry.path();
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => continue,
        };

        files_scanned += 1;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();

        let reader = std::io::BufReader::new(file);
        for (i, line) in reader.lines().enumerate() {
            if lines_scanned >= MAX_TOTAL_LINES || matches.len() >= MAX_MATCHES {
                break;
            }
            let line = match line {
                Ok(l) => l,
                // Not valid UTF-8 (or unreadable): treat as binary, skip the
                // rest of this file rather than erroring out.
                Err(_) => break,
            };
            lines_scanned += 1;
            if let Some((marker, text)) = find_marker(&re, &line) {
                matches.push(TodoMatch {
                    path: rel.clone(),
                    line: i + 1,
                    marker,
                    text,
                });
            }
        }
    }

    matches
}

/// If `line` contains a `TODO`/`FIXME`/`XXX` marker, returns it plus the
/// trailing text (trimmed of leading punctuation/whitespace).
fn find_marker(re: &Regex, line: &str) -> Option<(&'static str, String)> {
    let m = re.captures(line)?.get(1)?;
    let marker = match m.as_str() {
        "TODO" => "TODO",
        "FIXME" => "FIXME",
        "XXX" => "XXX",
        _ => return None,
    };
    let rest = line[m.end()..].trim_start_matches([':', '-', ' ', '\t']);
    Some((marker, rest.trim().to_string()))
}

fn render_todos(matches: &[TodoMatch]) -> String {
    let mut body = format!("{} open TODO/FIXME/XXX marker(s):\n", matches.len());
    for m in matches.iter().take(MAX_SHOWN) {
        body.push_str(&format!("{}:{}: {} {}\n", m.path, m.line, m.marker, m.text));
    }
    if matches.len() > MAX_SHOWN {
        body.push_str(&format!("+{} more\n", matches.len() - MAX_SHOWN));
    }
    body.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_markers_and_renders() {
        let dir = std::env::temp_dir().join(format!("atelier-todos-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.rs"),
            "fn main() {}\n// TODO: fix this\nlet x = 1; // FIXME broken\n",
        )
        .unwrap();

        let provider = TodosProvider;
        let item = provider.gather(&dir).expect("expected todos");
        assert_eq!(item.title, "open TODOs");
        assert!(item.body.contains("2 open TODO/FIXME/XXX marker(s):"));
        assert!(item.body.contains("a.rs:2: TODO fix this"));
        assert!(item.body.contains("a.rs:3: FIXME broken"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_markers_returns_none() {
        let dir = std::env::temp_dir().join(format!("atelier-todos-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();

        let provider = TodosProvider;
        assert!(provider.gather(&dir).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_marker_extracts_text() {
        let re = todo_regex();
        assert_eq!(
            find_marker(&re, "// TODO: fix this"),
            Some(("TODO", "fix this".to_string()))
        );
        assert_eq!(find_marker(&re, "no markers here"), None);
    }
}
