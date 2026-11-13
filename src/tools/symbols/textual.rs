//! `symbols` without a language server: declarations recognised by their
//! shape, names matched as words.
//!
//! This is the fallback tier, and it is approximate on purpose. A line that
//! *looks like* a declaration — `fn name`, `class Name`, `func (r T) Name` —
//! is taken for one; a name is "referenced" wherever it appears as a whole
//! word. Nothing is parsed: there are no scopes, no types, no imports, so
//! two functions called `new` are indistinguishable and a name in a string
//! counts as a use. The tool says so on the first line of every result
//! produced here, and the patterns are kept few and obvious rather than
//! clever: a small set that is right about the common forms is easier for
//! the model to weigh than a large one that is right more often but
//! unpredictably.
//!
//! One regex per declaration form, tried in order on each line (the first
//! that matches names it); lines that start as comments are skipped.
//! Project walks honor `.gitignore` and the same exclusions as `grep`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ignore::WalkBuilder;
use regex::Regex;

use crate::tools::spill;

/// Files larger than this are not searched: generated or minified, and not
/// where a declaration is looked for.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// The most lines of comment shown above a declaration by `hover`.
const MAX_DOC_LINES: usize = 12;

/// One declaration form: a regex with a `name` group and either a fixed
/// kind or a `kind` group.
struct Form {
    re: Regex,
    kind: Option<&'static str>,
    /// Whether a search by name may land on it. `impl` blocks are worth
    /// listing in an outline, but are not where `Foo` is defined.
    named: bool,
}

/// A language the fallback knows declaration forms for.
pub(super) struct Lang {
    pub(super) name: &'static str,
    extensions: &'static [&'static str],
    forms: Vec<Form>,
    /// What a comment line starts with.
    comments: &'static [&'static str],
    /// Words the looser forms would otherwise take for names
    /// (`if (x) {` has the shape of a method).
    keywords: &'static [&'static str],
}

fn form(kind: Option<&'static str>, pattern: &str) -> Form {
    Form {
        re: Regex::new(pattern).expect("a declaration pattern"),
        kind,
        named: true,
    }
}

fn langs() -> &'static [Lang] {
    static LANGS: OnceLock<Vec<Lang>> = OnceLock::new();
    LANGS.get_or_init(|| {
        const VIS: &str = r"^\s*(?:pub(?:\([^)]*\))?\s+)?";
        const JS: &str = r"^\s*(?:export\s+)?(?:default\s+)?(?:declare\s+)?";
        const C_STYLE: &[&str] = &["//", "/*", "*"];
        const CONTROL: &[&str] = &[
            "if", "else", "for", "while", "do", "switch", "catch", "return", "function", "sizeof",
            "defined", "new", "delete", "throw", "typeof", "await",
        ];
        vec![
            Lang {
                name: "Rust",
                extensions: &["rs"],
                forms: vec![
                    form(
                        Some("fn"),
                        &format!(
                            r#"{VIS}(?:(?:default|const|async|unsafe|extern(?:\s+"[^"]*")?)\s+)*fn\s+(?P<name>[A-Za-z_]\w*)"#
                        ),
                    ),
                    form(
                        None,
                        &format!(
                            r"{VIS}(?:unsafe\s+)?(?P<kind>struct|enum|union|trait|mod|type)\s+(?P<name>[A-Za-z_]\w*)"
                        ),
                    ),
                    form(
                        None,
                        &format!(
                            r"{VIS}(?P<kind>const|static)\s+(?:mut\s+)?(?P<name>[A-Za-z_]\w*)\s*:"
                        ),
                    ),
                    form(
                        Some("macro"),
                        r"^\s*macro_rules!\s+(?P<name>[A-Za-z_]\w*)",
                    ),
                    Form {
                        named: false,
                        ..form(
                            Some("impl"),
                            r"^\s*(?:unsafe\s+)?(?P<name>impl\b.*?)\s*(?:\{.*|\bwhere\b.*)?$",
                        )
                    },
                ],
                comments: &["//"],
                keywords: &[],
            },
            Lang {
                name: "Go",
                extensions: &["go"],
                forms: vec![
                    form(
                        Some("method"),
                        r"^func\s+\([^)]*\)\s*(?P<name>[A-Za-z_]\w*)",
                    ),
                    form(Some("func"), r"^func\s+(?P<name>[A-Za-z_]\w*)"),
                    form(
                        None,
                        r"^type\s+(?P<name>[A-Za-z_]\w*)\s+(?P<kind>struct|interface)\b",
                    ),
                    form(Some("type"), r"^type\s+(?P<name>[A-Za-z_]\w*)\b"),
                    form(None, r"^(?P<kind>var|const)\s+(?P<name>[A-Za-z_]\w*)\b"),
                ],
                comments: &["//"],
                keywords: &[],
            },
            Lang {
                name: "Python",
                extensions: &["py", "pyi"],
                forms: vec![
                    form(
                        Some("def"),
                        r"^\s*(?:async\s+)?def\s+(?P<name>[A-Za-z_]\w*)",
                    ),
                    form(Some("class"), r"^\s*class\s+(?P<name>[A-Za-z_]\w*)"),
                    // A module-level constant, by the naming convention.
                    form(
                        Some("const"),
                        r"^(?P<name>[A-Z][A-Z0-9_]*)\s*(?::[^=]+)?=(?:[^=]|$)",
                    ),
                ],
                comments: &["#"],
                keywords: &[],
            },
            Lang {
                name: "TypeScript/JavaScript",
                extensions: &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"],
                forms: vec![
                    form(
                        Some("function"),
                        &format!(r"{JS}(?:async\s+)?function\s*\*?\s*(?P<name>[A-Za-z_$][\w$]*)"),
                    ),
                    form(
                        Some("class"),
                        &format!(r"{JS}(?:abstract\s+)?class\s+(?P<name>[A-Za-z_$][\w$]*)"),
                    ),
                    form(
                        None,
                        &format!(
                            r"{JS}(?:const\s+)?(?P<kind>interface|type|enum|namespace)\s+(?P<name>[A-Za-z_$][\w$]*)"
                        ),
                    ),
                    // Top-level only: an indented `const` is a local.
                    form(
                        None,
                        r"^(?:export\s+)?(?:declare\s+)?(?P<kind>const|let|var)\s+(?P<name>[A-Za-z_$][\w$]*)",
                    ),
                    form(
                        Some("method"),
                        r"^\s+(?:(?:public|private|protected|static|async|readonly|override|abstract|get|set)\s+)*(?P<name>[A-Za-z_$#][\w$]*)\s*(?:<[^>]*>)?\([^)]*\)\s*(?::\s*[^{;]+)?\{\s*$",
                    ),
                ],
                comments: C_STYLE,
                keywords: CONTROL,
            },
            Lang {
                name: "C/C++",
                extensions: &["c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx"],
                forms: vec![
                    form(
                        Some("macro"),
                        r"^\s*#\s*define\s+(?P<name>[A-Za-z_]\w*)",
                    ),
                    // The name ends the line (bar a base list or a brace):
                    // `struct foo *p;` is a use, not a definition.
                    form(
                        None,
                        r"^\s*(?:typedef\s+)?(?P<kind>struct|class|union|enum|namespace)\s+(?:class\s+)?(?P<name>[A-Za-z_]\w*)\s*(?::[^:].*)?\{?\s*$",
                    ),
                    form(
                        Some("typedef"),
                        r"^\s*typedef\b.*\b(?P<name>[A-Za-z_]\w*)\s*;\s*$",
                    ),
                    // A type, then a name, then `(`, starting in the first
                    // column: definitions and prototypes, not calls.
                    form(
                        Some("function"),
                        r"^[A-Za-z_][\w:<>,~*&\s]*?[\s*&](?:[A-Za-z_]\w*::)*(?P<name>~?[A-Za-z_]\w*)\s*\(",
                    ),
                ],
                comments: C_STYLE,
                keywords: CONTROL,
            },
        ]
    })
}

/// The language of files with extension `ext`, if it is one of ours.
pub(super) fn lang_of(ext: &str) -> Option<&'static Lang> {
    langs().iter().find(|l| l.extensions.contains(&ext))
}

/// A declaration found on a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Decl {
    /// 0-based.
    pub(super) line: usize,
    /// Byte offset of the name in the line.
    pub(super) byte: usize,
    pub(super) kind: String,
    pub(super) name: String,
    named: bool,
}

impl Lang {
    /// The declaration `line` (the `index`th of its file) makes, if any.
    pub(super) fn declaration(&self, index: usize, line: &str) -> Option<Decl> {
        let start = line.trim_start();
        if start.is_empty() || self.comments.iter().any(|c| start.starts_with(c)) {
            return None;
        }
        self.forms.iter().find_map(|form| {
            let found = form.re.captures(line)?;
            let name = found.name("name")?;
            if self.keywords.contains(&name.as_str()) {
                return None;
            }
            let kind = (found.name("kind")).map_or(form.kind.unwrap_or("symbol"), |k| k.as_str());
            Some(Decl {
                line: index,
                byte: name.start(),
                kind: kind.to_string(),
                name: name.as_str().to_string(),
                named: form.named,
            })
        })
    }

    /// Every declaration in `text`, in order: the outline.
    pub(super) fn declarations(&self, text: &str) -> Vec<Decl> {
        let lines = text.lines().enumerate();
        lines
            .filter_map(|(i, line)| self.declaration(i, line))
            .collect()
    }

    /// The comment above the declaration at `line` and the declaration
    /// line itself, as `hover` shows them. Attributes and decorators
    /// between the two are part of it; for Python the docstring below is.
    pub(super) fn documented(&self, lines: &[&str], line: usize) -> Vec<String> {
        let above = |l: &str| {
            let l = l.trim_start();
            let comment = self.comments.iter().any(|c| l.starts_with(c));
            comment || l.starts_with("#[") || l.starts_with('@')
        };
        let before = lines[..line].iter().rev();
        let first = line - before.take_while(|l| above(l)).count();
        let mut out: Vec<String> = lines[first..=line].iter().map(|l| l.to_string()).collect();
        if out.len() > MAX_DOC_LINES + 1 {
            out.drain(..out.len() - MAX_DOC_LINES - 1);
        }
        let quotes = ["\"\"\"", "'''"];
        if let Some(next) = lines.get(line + 1)
            && let Some(quote) = quotes.iter().find(|q| next.trim_start().starts_with(*q))
        {
            for (i, l) in lines[line + 1..].iter().take(MAX_DOC_LINES).enumerate() {
                out.push(l.to_string());
                // The quote that closes it: on a later line, or after the
                // opening one on the first.
                let rest = match i {
                    0 => &l.trim_start()[quote.len()..],
                    _ => l,
                };
                if rest.contains(quote) {
                    break;
                }
            }
        }
        out
    }
}

/// Whether `c` can be part of an identifier (`$` is, in JavaScript).
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// The identifier around byte offset `byte` of `line`: its start and text.
pub(super) fn word_at(line: &str, byte: usize) -> Option<(usize, &str)> {
    // Inside a character is at that character.
    let mut byte = byte.min(line.len());
    while !line.is_char_boundary(byte) {
        byte -= 1;
    }
    let start = line[..byte]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(byte, |(i, _)| i);
    let after = line[byte..].char_indices().find(|(_, c)| !is_word(*c));
    let end = after.map_or(line.len(), |(i, _)| byte + i);
    (start < end).then(|| (start, &line[start..end]))
}

/// Where `name` appears in `line` as a whole word, first occurrence.
pub(super) fn find_word(line: &str, name: &str) -> Option<usize> {
    if name.is_empty() {
        return None;
    }
    let mut from = 0;
    while let Some(i) = line[from..].find(name) {
        let (start, end) = (from + i, from + i + name.len());
        let before = line[..start].chars().next_back();
        let after = line[end..].chars().next();
        if !before.is_some_and(is_word) && !after.is_some_and(is_word) {
            return Some(start);
        }
        // On to the next character (not byte) after this false start.
        from = start + line[start..].chars().next().map_or(1, char::len_utf8);
    }
    None
}

/// One line of a project file that a search landed on.
#[derive(Debug, Clone)]
pub(super) struct Hit {
    pub(super) path: PathBuf,
    /// 0-based.
    pub(super) line: usize,
    pub(super) byte: usize,
    /// For a declaration: its kind and name.
    pub(super) decl: Option<(String, String)>,
    pub(super) text: String,
}

/// The files of the project at `root` with an extension `wanted` accepts,
/// in a stable order, with their text. Ignored files, the directories
/// `grep` leaves out, binary and oversized files are skipped.
fn files(root: &Path, wanted: &dyn Fn(&str) -> bool, visit: &mut dyn FnMut(&Path, &str)) {
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(spill::keep_in_walk(root))
        .sort_by_file_name(|a, b| a.cmp(b))
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !wanted(ext) || entry.metadata().is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(path) {
            visit(path, &text);
        }
    }
}

/// What a project-wide search found: the first hits, and how many there
/// were in all.
pub(super) struct Found {
    pub(super) hits: Vec<Hit>,
    pub(super) total: usize,
}

impl Found {
    fn new() -> Self {
        Self {
            hits: Vec::new(),
            total: 0,
        }
    }

    fn add(&mut self, keep: usize, hit: impl FnOnce() -> Hit) {
        self.total += 1;
        if self.hits.len() < keep {
            self.hits.push(hit());
        }
    }
}

/// The declarations in the project whose name `accept`s, at most `keep` of
/// them kept: in files of `only`'s language, or of every known language.
pub(super) fn find_declarations(
    root: &Path,
    only: Option<&Lang>,
    accept: &dyn Fn(&str) -> bool,
    keep: usize,
) -> Found {
    let mut found = Found::new();
    let wanted = |ext: &str| match only {
        Some(lang) => lang.extensions.contains(&ext),
        None => lang_of(ext).is_some(),
    };
    files(root, &wanted, &mut |path, text| {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let Some(lang) = lang_of(ext) else { return };
        for (i, line) in text.lines().enumerate() {
            let Some(decl) = lang.declaration(i, line) else {
                continue;
            };
            if decl.named && accept(&decl.name) {
                found.add(keep, || Hit {
                    path: path.to_path_buf(),
                    line: i,
                    byte: decl.byte,
                    decl: Some((decl.kind, decl.name)),
                    text: line.to_string(),
                });
            }
        }
    });
    found
}

/// The lines of the project on which `name` appears as a word and is not
/// being declared, at most `keep` of them kept. Searched: files of `lang`,
/// or — for a file in no known language — files with extension `ext`.
pub(super) fn find_references(
    root: &Path,
    lang: Option<&Lang>,
    ext: &str,
    name: &str,
    keep: usize,
) -> Found {
    let mut found = Found::new();
    let wanted = |e: &str| match lang {
        Some(lang) => lang.extensions.contains(&e),
        None => e == ext,
    };
    files(root, &wanted, &mut |path, text| {
        for (i, line) in text.lines().enumerate() {
            let Some(byte) = find_word(line, name) else {
                continue;
            };
            let declares = lang
                .and_then(|l| l.declaration(i, line))
                .is_some_and(|d| d.named && d.name == name);
            if !declares {
                found.add(keep, || Hit {
                    path: path.to_path_buf(),
                    line: i,
                    byte,
                    decl: None,
                    text: line.to_string(),
                });
            }
        }
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(line, kind, name)` of each declaration, lines counted from 1.
    fn outline(ext: &str, text: &str) -> Vec<(usize, String, String)> {
        let decls = lang_of(ext).unwrap().declarations(text);
        decls
            .into_iter()
            .map(|d| (d.line + 1, d.kind, d.name))
            .collect()
    }

    fn expect(ext: &str, text: &str, want: &[(usize, &str, &str)]) {
        let want: Vec<(usize, String, String)> = want
            .iter()
            .map(|(l, k, n)| (*l, k.to_string(), n.to_string()))
            .collect();
        assert_eq!(outline(ext, text), want, "{ext}");
    }

    pub(crate) const RUST: &str = "\
//! fn not_this() in a comment
use std::fmt;

/// A widget.
#[derive(Debug)]
pub struct Widget<T> {
    size: usize,
}

pub(crate) enum Shape { Round, Square }
pub trait Draw {
    type Canvas;
    fn draw(&self) -> String;
}
impl<T> Draw for Widget<T> {
    type Canvas = ();
    fn draw(&self) -> String {
        let fn_like = helper(self.size);
        fn_like
    }
}
unsafe impl Send for Shape {}
pub const MAX: usize = 3;
static mut COUNT: u32 = 0;
pub(super) async fn fetch() {}
pub const unsafe extern \"C\" fn raw() {}
macro_rules! shout {
    () => {};
}
mod inner;
pub type Alias = Widget<u8>;
union Bits { a: u8 }
fn helper(n: usize) -> String { n.to_string() }
";

    #[test]
    fn rust_declarations() {
        expect(
            "rs",
            RUST,
            &[
                (6, "struct", "Widget"),
                (10, "enum", "Shape"),
                (11, "trait", "Draw"),
                (12, "type", "Canvas"),
                (13, "fn", "draw"),
                (15, "impl", "impl<T> Draw for Widget<T>"),
                (16, "type", "Canvas"),
                (17, "fn", "draw"),
                (22, "impl", "impl Send for Shape"),
                (23, "const", "MAX"),
                (24, "static", "COUNT"),
                (25, "fn", "fetch"),
                (26, "fn", "raw"),
                (27, "macro", "shout"),
                (30, "mod", "inner"),
                (31, "type", "Alias"),
                (32, "union", "Bits"),
                (33, "fn", "helper"),
            ],
        );
        // The column is the name's.
        let decl = &lang_of("rs").unwrap().declarations(RUST)[0];
        assert_eq!(&RUST.lines().nth(5).unwrap()[decl.byte..][..6], "Widget");
    }

    #[test]
    fn go_declarations() {
        let text = "\
package shapes

// func Commented() {}
type Shape interface {
\tArea() float64
}
type Square struct{ side float64 }
type ID int
func (s *Square) Area() float64 { return s.side * s.side }
func NewSquare(side float64) *Square {
\treturn &Square{side}
}
var Default = NewSquare(1)
const Sides = 4
";
        expect(
            "go",
            text,
            &[
                (4, "interface", "Shape"),
                (7, "struct", "Square"),
                (8, "type", "ID"),
                (9, "method", "Area"),
                (10, "func", "NewSquare"),
                (13, "var", "Default"),
                (14, "const", "Sides"),
            ],
        );
    }

    #[test]
    fn python_declarations() {
        let text = "\
# def commented(): pass
import os
MAX_SIZE = 10
TIMEOUT: float = 2.5
lower = 1
class Shape(Base):
    \"\"\"A shape.\"\"\"
    def area(self):
        return 0
    async def load(self): ...
def main():
    if MAX_SIZE == 10:
        pass
";
        expect(
            "py",
            text,
            &[
                (3, "const", "MAX_SIZE"),
                (4, "const", "TIMEOUT"),
                (6, "class", "Shape"),
                (8, "def", "area"),
                (10, "def", "load"),
                (11, "def", "main"),
            ],
        );
    }

    #[test]
    fn typescript_and_javascript_declarations() {
        let text = "\
// function commented() {}
import { x } from './x';
export interface Shape { area(): number }
export type Id = string;
export const enum Color { Red }
declare namespace NS {}
export default class Square extends Base implements Shape {
  private side = 1;
  constructor(side: number) {
    super();
  }
  static async make<T>(side: number): Promise<Square> {
    if (side > 0) {
      return new Square(side);
    }
    for (const s of [1]) {
    }
  }
  get area(): number {
    return this.side ** 2;
  }
}
export async function load(id: Id) {}
function* gen() {}
export const handler = async () => {};
let counter = 0;
  const local = 1;
";
        let want = [
            (3, "interface", "Shape"),
            (4, "type", "Id"),
            (5, "enum", "Color"),
            (6, "namespace", "NS"),
            (7, "class", "Square"),
            (9, "method", "constructor"),
            (12, "method", "make"),
            (19, "method", "area"),
            (23, "function", "load"),
            (24, "function", "gen"),
            (25, "const", "handler"),
            (26, "let", "counter"),
        ];
        expect("ts", text, &want);
        expect("jsx", text, &want);
    }

    #[test]
    fn c_and_cpp_declarations() {
        let text = "\
/* int commented(void); */
#include <stdio.h>
#define MAX_LEN 64
#  define SQUARE(x) ((x) * (x))
typedef unsigned long word_t;
struct point {
    int x;
};
typedef struct node
{
    struct node *next;
} node_t;
enum class Color : int {
    Red,
};
namespace geo {
class Shape : public Base {
};
}
static int helper(struct point *p);
int main(int argc, char **argv)
{
    struct point p;
    if (argc > 1) {
        return helper(&p);
    }
    return 0;
}
const char *name_of(const node_t *n) { return 0; }
void Shape::draw() const {}
Shape::~Shape() {}
int total = compute(3);
";
        expect(
            "cpp",
            text,
            &[
                (3, "macro", "MAX_LEN"),
                (4, "macro", "SQUARE"),
                (5, "typedef", "word_t"),
                (6, "struct", "point"),
                (9, "struct", "node"),
                (13, "enum", "Color"),
                (16, "namespace", "geo"),
                (17, "class", "Shape"),
                (20, "function", "helper"),
                (21, "function", "main"),
                (29, "function", "name_of"),
                (30, "function", "draw"),
            ],
        );
        assert_eq!(outline("h", text), outline("c", text));
    }

    #[test]
    fn words_are_found_whole() {
        let line = "let foo_bar = foo(bar) + self.foo; // é𝄞 foo";
        assert_eq!(find_word(line, "foo"), Some(14));
        assert_eq!(find_word(line, "foo_bar"), Some(4));
        assert_eq!(find_word(line, "ba"), None);
        assert_eq!(find_word(line, ""), None);
        assert_eq!(find_word("$x = x$ + x", "x"), Some(10));
        assert_eq!(find_word("ééé é", "é"), Some(7));

        assert_eq!(word_at(line, 14), Some((14, "foo")));
        assert_eq!(word_at(line, 16), Some((14, "foo")));
        // Just past its end still counts: a cursor after the word.
        assert_eq!(word_at(line, 17), Some((14, "foo")));
        assert_eq!(word_at(line, 6), Some((4, "foo_bar")));
        assert_eq!(word_at(line, 12), None);
        assert_eq!(word_at("naïve()", 3), Some((0, "naïve")));
        assert_eq!(word_at("x", 99), Some((0, "x")));
    }

    #[test]
    fn hover_text_is_the_comment_the_attributes_and_the_line() {
        let rust = lang_of("rs").unwrap();
        let lines: Vec<&str> = RUST.lines().collect();
        assert_eq!(
            rust.documented(&lines, 5),
            [
                "/// A widget.",
                "#[derive(Debug)]",
                "pub struct Widget<T> {"
            ]
        );
        // Nothing above it: the line alone.
        assert_eq!(rust.documented(&lines, 9), [lines[9]]);

        let python = lang_of("py").unwrap();
        let text = "# Loads.\n@cached\ndef load(x):\n    \"\"\"Load x.\n\n    Slowly.\n    \"\"\"\n    return x\ndef one(): \n    '''One line.'''\n    pass\n";
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            python.documented(&lines, 2),
            [
                "# Loads.",
                "@cached",
                "def load(x):",
                "    \"\"\"Load x.",
                "",
                "    Slowly.",
                "    \"\"\""
            ]
        );
        assert_eq!(
            python.documented(&lines, 8),
            ["def one(): ", "    '''One line.'''"]
        );

        // A long comment keeps its end, next to the declaration.
        let mut long: Vec<String> = (0..40).map(|i| format!("// line {i}")).collect();
        long.push("fn f() {}".to_string());
        let lines: Vec<&str> = long.iter().map(String::as_str).collect();
        let shown = rust.documented(&lines, 40);
        assert_eq!(shown.len(), MAX_DOC_LINES + 1);
        assert_eq!(shown[0], "// line 28");
        assert_eq!(shown[MAX_DOC_LINES], "fn f() {}");
    }

    fn project(name: &str) -> PathBuf {
        let root = crate::trust::testing::dir(&format!("textual-{name}"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/shapes.rs"), RUST).unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "fn main() {\n    let w = helper(3); // helper\n    helper_two();\n}\nfn helper_two() {}\n",
        )
        .unwrap();
        std::fs::write(root.join("tool.py"), "def helper():\n    return helper\n").unwrap();
        // Ignored, as for grep.
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/gen.rs"), "fn helper() {}\n").unwrap();
        root
    }

    fn places(root: &Path, found: &Found) -> Vec<String> {
        let hits = found.hits.iter();
        hits.map(|h| {
            let rel = h.path.strip_prefix(root).unwrap().display();
            format!("{rel}:{}:{}", h.line + 1, h.byte + 1)
        })
        .collect()
    }

    #[test]
    fn declarations_are_found_across_the_project_by_name() {
        let root = project("decls");
        let rust = lang_of("rs");
        let found = find_declarations(&root, rust, &|n| n == "helper", 10);
        assert_eq!(places(&root, &found), ["src/shapes.rs:33:4"]);
        assert_eq!(
            found.hits[0].decl,
            Some(("fn".to_string(), "helper".to_string()))
        );
        // Every language, and a looser match.
        let found = find_declarations(&root, None, &|n| n.contains("helper"), 10);
        assert_eq!(
            places(&root, &found),
            ["src/main.rs:5:4", "src/shapes.rs:33:4", "tool.py:1:5"]
        );
        // `impl` blocks are not definitions of the type.
        let found = find_declarations(&root, rust, &|n| n.contains("Shape"), 10);
        assert_eq!(places(&root, &found), ["src/shapes.rs:10:17"]);
        // The total counts what was not kept.
        let found = find_declarations(&root, None, &|_| true, 2);
        assert_eq!((found.hits.len(), found.total), (2, 19));
    }

    #[test]
    fn references_are_whole_words_minus_the_declaration() {
        let root = project("refs");
        let found = find_references(&root, lang_of("rs"), "rs", "helper", 10);
        // Not `helper_two`, not the `fn helper` line, not target/, not the
        // Python file; one hit per line.
        assert_eq!(
            places(&root, &found),
            ["src/main.rs:2:13", "src/shapes.rs:18:23"]
        );
        assert_eq!(found.total, 2);
        // A language the fallback has no forms for: same extension, and
        // nothing is recognised as the declaration.
        std::fs::write(root.join("a.zig"), "fn helper() void {}\nhelper();\n").unwrap();
        let found = find_references(&root, None, "zig", "helper", 1);
        assert_eq!(places(&root, &found), ["a.zig:1:4"]);
        assert_eq!(found.total, 2);
    }
}
