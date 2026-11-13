//! The `symbols` tool: code navigation — where a name is defined, who uses
//! it, what a file declares, what a symbol's signature is.
//!
//! One tool, two tiers, and every result says which produced it.
//!
//! **Tier 1: a language server**, when the project has one (see
//! [`crate::lsp`] for which, and for how long it lives). The answers are
//! the compiler's: `definition` follows imports and trait impls,
//! `references` knows two functions called `new` apart. A server runs the
//! project's code, so one is started only in a project the user has
//! trusted — the rule the diagnostics checkers follow, for the same reason.
//!
//! **Tier 2: textual search** ([`textual`]), when there is no server, the
//! project is not trusted, or the server did not answer in time (servers
//! index for a while after starting; the call does not fail, and the next
//! one may get through). Declarations are recognised by shape and names
//! matched as words. That is often enough and sometimes wrong, so a tier-2
//! result opens with a line saying it is an approximation and why there was
//! no server — the model should weigh it as it would a `grep`.
//!
//! # What the model sees
//!
//! Never the server's JSON. Each location is one line,
//!
//! ```text
//! src/tools/grep.rs:57:8  fn  call  — fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
//! ```
//!
//! with the path relative to the project, the line and the column counted
//! from 1 (the column in characters), and the source line as context;
//! lists are capped, with a count of what was left out.
//!
//! # Naming a position
//!
//! `definition`, `references` and `hover` are about the symbol at a
//! position. Counting columns is what models are worst at, so the call may
//! give the `symbol`'s text instead and the tool finds it on the line
//! ([`locate`]); with neither, the name the line declares is meant.
//!
//! # Outside the project
//!
//! A definition may be in the standard library or a dependency. Such a
//! location is reported — path, line — but the file is not opened here:
//! this tool is confined to the project, and reading that file is the
//! `read` tool's call, with its own approval for outside paths.

mod textual;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec, confine, path_arg};
use crate::lsp::{self, Client, Failure, ServerSpec, Servers, position};
use crate::settings::{LspServerConfig, Settings};
use crate::trust::{Store, Trust};

/// The most locations listed by `definition`, `references` and `search`.
const MAX_ENTRIES: usize = 100;
/// The most symbols listed by `outline`: a file's outline is the one list
/// worth reading whole.
const MAX_OUTLINE: usize = 200;
/// The longest line of context shown.
const MAX_CONTEXT_CHARS: usize = 160;
/// How much of a server's hover text is passed on.
const MAX_HOVER_LINES: usize = 60;
/// How many same-named declarations tier-2 `hover` shows.
const MAX_HOVER_DECLS: usize = 3;

pub struct SymbolsTool {
    servers: Arc<Servers>,
}

impl SymbolsTool {
    /// The tool on the process's language servers.
    pub fn new() -> Self {
        Self {
            servers: Arc::clone(Servers::global()),
        }
    }
}

/// What a call works with besides the project's files.
struct Setup<'a> {
    /// The project's `[[lsp]]` entries.
    configured: &'a [LspServerConfig],
    /// Whether the project's commands may run (see [`crate::trust`]).
    trusted: bool,
    /// Whether a program is on `PATH`.
    has: &'a dyn Fn(&str) -> bool,
}

impl Tool for SymbolsTool {
    fn name(&self) -> &str {
        "symbols"
    }

    // Only reads: files of the project, and answers from a server that is
    // told nothing but what those files contain.
    fn mutates(&self, _args: &Value) -> bool {
        false
    }

    fn requires_approval(&self, _args: &Value) -> bool {
        false
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "Navigate code by symbol. Prefer this over grep to find where \
                something is defined (`definition`, or `search` when you only have a name), \
                who calls or uses it (`references`), what a file contains (`outline`), or a \
                symbol's signature and documentation (`hover`). Uses the project's language \
                server when one is available: results are then exact. Otherwise it falls \
                back to textual matching of names and declaration shapes — the result's \
                first line says so (\"textual approximation\"), and such results can miss \
                or over-report, like a grep. Results are `path:line:col  kind  name  — \
                source line`, capped. For `definition`, `references` and `hover`, point at \
                a symbol with `path` and `line`, plus `symbol` (its name as written on that \
                line) or `column`."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["definition", "references", "hover", "outline", "search"],
                        "description": "definition: where the symbol at a position is defined. \
                            references: where it is used. hover: its signature and docs. \
                            outline: the symbols a file declares. search: symbols anywhere \
                            in the project whose name matches `query`."
                    },
                    "path": {
                        "type": "string",
                        "description": "File, relative to the project root. Required except for search."
                    },
                    "line": {
                        "type": "integer",
                        "description": "Line the symbol is on, counted from 1 (definition, references, hover)."
                    },
                    "symbol": {
                        "type": "string",
                        "description": "The symbol's name as it appears on that line; its first \
                            occurrence is used (for `a::b` or `a.b`, the last part). Easier \
                            than `column`. With neither, the name declared on the line is meant."
                    },
                    "column": {
                        "type": "integer",
                        "description": "Column of the symbol, in characters counted from 1. An alternative to `symbol`."
                    },
                    "query": {
                        "type": "string",
                        "description": "Name, or part of one, to search for (search)."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        self.call_with(ctx.project_root, &args, Store::from_env())
    }
}

impl SymbolsTool {
    /// [`Tool::call`], with the record of trust decisions to consult.
    fn call_with(&self, root: &Path, args: &Value, store: Store) -> Result<String> {
        // A broken atelier.toml was already reported at startup; here it
        // just means "no [[lsp]] table".
        let settings = Settings::load(root).unwrap_or_default();
        // Asked on every call: `/trust` switches the servers on
        // mid-session, as it does the checkers.
        let trusted = Trust::load(root, &settings, store).active();
        let setup = Setup {
            configured: &settings.lsp,
            trusted,
            has: &crate::context::on_path,
        };
        self.run(root, args, &setup)
    }

    fn run(&self, root: &Path, args: &Value, setup: &Setup) -> Result<String> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required argument 'action'"))?;
        match action {
            "search" => {
                let query = args.get("query").and_then(Value::as_str).map(str::trim);
                let Some(query) = query.filter(|q| !q.is_empty()) else {
                    bail!("search needs a 'query': a symbol name or part of one");
                };
                Ok(self.search(root, query, setup))
            }
            "outline" => {
                let source = Source::read(root, args)?;
                Ok(self.outline(root, &source, setup))
            }
            "definition" | "references" | "hover" => {
                let source = Source::read(root, args)?;
                let target = Target::find(&source, args)?;
                Ok(self.at(root, action, &source, &target, setup))
            }
            other => bail!(
                "unknown action {other:?}: use definition, references, hover, outline or search"
            ),
        }
    }

    /// The server for `source`, if one may be started; else why not, as
    /// the reason of a tier-2 note.
    fn server(&self, root: &Path, source: &Source, setup: &Setup) -> Result<ServerSpec, String> {
        let spec = lsp::choose(root, setup.configured, &source.ext, setup.has)?;
        match setup.trusted {
            true => Ok(spec),
            false => Err(untrusted(&spec.name)),
        }
    }

    /// `definition`, `references` or `hover` of `target`.
    fn at(
        &self,
        root: &Path,
        action: &str,
        source: &Source,
        target: &Target,
        setup: &Setup,
    ) -> String {
        let name = &target.name;
        let answered = self.server(root, source, setup).and_then(|spec| {
            let line = source.line(target.line);
            let mut params = json!({
                "textDocument": { "uri": lsp::uri_of_path(&source.abs) },
                "position": {
                    "line": target.line,
                    "character": position::utf16_of_byte(line, target.byte),
                },
            });
            let method = match action {
                "definition" => "textDocument/definition",
                "hover" => "textDocument/hover",
                _ => {
                    params["context"] = json!({ "includeDeclaration": false });
                    "textDocument/references"
                }
            };
            let answer = self.servers.with(root, &spec, |client, deadline| {
                source.sync(client)?;
                ask(client, deadline, method, params.clone())
            });
            match answer {
                Ok(value) => Ok((spec.name, value)),
                Err(why) => Err(format!("{} {why}", spec.name)),
            }
        });
        let (server, value) = match answered {
            Ok(answer) => answer,
            Err(why) => {
                let body = match action {
                    "definition" => textual_definition(root, source, name),
                    "hover" => textual_hover(root, source, name),
                    _ => textual_references(root, source, name),
                };
                return approximate(&source.language(), &why, &body);
            }
        };
        if action == "hover" {
            let place = format!(
                "{}:{}:{}",
                source.rel,
                target.line + 1,
                position::column_of_byte(source.line(target.line), target.byte)
            );
            return match hover_text(&value) {
                Some(text) => format!("hover for `{name}` at {place} ({server})\n{text}"),
                None => format!("no hover information for `{name}` at {place} ({server})"),
            };
        }
        let mut reader = Reader::new(root);
        let places = places(&value);
        let total = places.len();
        let entries: Vec<Entry> = (places.into_iter().take(MAX_ENTRIES))
            .map(|p| {
                // A definition's line says what kind of thing it is.
                let named = (action == "definition").then_some(name.as_str());
                reader.entry(&p, named)
            })
            .collect();
        let what = match action {
            "definition" => format!("definition of `{name}`"),
            _ => format!("references to `{name}` (its declaration aside)"),
        };
        render(&what, "location", &entries, total, Some(&server))
    }

    /// The symbols `source` declares.
    fn outline(&self, root: &Path, source: &Source, setup: &Setup) -> String {
        let what = format!("outline of {}", source.rel);
        let answered = self.server(root, source, setup).and_then(|spec| {
            let params = json!({ "textDocument": { "uri": lsp::uri_of_path(&source.abs) } });
            let answer = self.servers.with(root, &spec, |client, deadline| {
                source.sync(client)?;
                ask(
                    client,
                    deadline,
                    "textDocument/documentSymbol",
                    params.clone(),
                )
            });
            match answer {
                Ok(value) => Ok((spec.name, value)),
                Err(why) => Err(format!("{} {why}", spec.name)),
            }
        });
        match answered {
            Ok((server, value)) => {
                let mut symbols = Vec::new();
                flatten(&value, None, &lsp::uri_of_path(&source.abs), &mut symbols);
                let total = symbols.len();
                let mut reader = Reader::new(root);
                let entries: Vec<Entry> = (symbols.iter().take(MAX_OUTLINE))
                    .map(|s| reader.symbol(s))
                    .collect();
                render(&what, "symbol", &entries, total, Some(&server))
            }
            Err(why) => {
                let body = match textual::lang_of(&source.ext) {
                    Some(lang) => {
                        let decls = lang.declarations(&source.text);
                        let total = decls.len();
                        let entries: Vec<Entry> = (decls.iter().take(MAX_OUTLINE))
                            .map(|d| Entry {
                                place: place(&source.rel, d.line, source.line(d.line), d.byte),
                                label: Some(format!("{}  {}", d.kind, d.name)),
                                context: Some(context(source.line(d.line))),
                            })
                            .collect();
                        render(&what, "symbol", &entries, total, None)
                    }
                    None => format!(
                        "no {what}: no declaration forms are known for .{} files either; \
                         read the file instead",
                        source.ext
                    ),
                };
                approximate(&source.language(), &why, &body)
            }
        }
    }

    /// Symbols anywhere in the project whose name matches `query`.
    fn search(&self, root: &Path, query: &str, setup: &Setup) -> String {
        let what = format!("symbols matching `{query}`");
        let specs = lsp::all(root, setup.configured, setup.has);
        let why = match (specs.as_slice(), setup.trusted) {
            ([], _) => Some("none is configured or detected for it".to_string()),
            ([first, ..], false) => Some(untrusted(&first.name)),
            (_, true) => None,
        };
        // Every server of the project is asked: a name is not in a
        // language. What the ones that answer found is the result; with
        // none answering, the first one's excuse is the note.
        let mut failures = Vec::new();
        let mut answers = Vec::new();
        for spec in specs.iter().filter(|_| why.is_none()) {
            let params = json!({ "query": query });
            let answer = self.servers.with(root, spec, |client, deadline| {
                ask(client, deadline, "workspace/symbol", params.clone())
            });
            match answer {
                Ok(value) => answers.push((spec.name.as_str(), value)),
                Err(why) => failures.push(format!("{} {why}", spec.name)),
            }
        }
        if answers.is_empty() {
            let why = why.unwrap_or_else(|| failures.join("; "));
            let lowered = query.to_lowercase();
            let found = textual::find_declarations(
                root,
                None,
                &|name| name.to_lowercase().contains(&lowered),
                MAX_ENTRIES,
            );
            let entries: Vec<Entry> = found.hits.iter().map(|h| hit_entry(root, h)).collect();
            let body = render(&what, "declaration", &entries, found.total, None);
            return approximate("this project", &why, &body);
        }
        let mut symbols = Vec::new();
        let mut servers = Vec::new();
        for (server, value) in &answers {
            servers.push(*server);
            flatten(value, None, "", &mut symbols);
        }
        let total = symbols.len();
        let mut reader = Reader::new(root);
        let entries: Vec<Entry> = (symbols.iter().take(MAX_ENTRIES))
            .map(|s| reader.symbol(s))
            .collect();
        let mut out = render(&what, "symbol", &entries, total, Some(&servers.join(", ")));
        for failure in failures {
            out.push_str(&format!("\nnot asked: {failure}"));
        }
        out
    }
}

/// Why a server that exists is not started in this project.
fn untrusted(server: &str) -> String {
    format!(
        "this project is not trusted, so {server} is not started — a language server runs \
         the project's code; the user can accept it with /trust"
    )
}

/// A query: wait for the server to be done loading, then ask.
fn ask(client: &Client, deadline: Instant, method: &str, params: Value) -> Result<Value, Failure> {
    if !client.wait_ready(deadline) {
        return Err(Failure::Timeout);
    }
    client.request(method, params, deadline)
}

/// A tier-2 result: the note that it is one, then what was found.
fn approximate(language: &str, why: &str, body: &str) -> String {
    format!(
        "textual approximation (no language server for {language}: {why}) — names and \
         declaration shapes matched as text, not analysed\n{body}"
    )
}

/// The file a call is about.
struct Source {
    abs: PathBuf,
    /// Relative to the project root, as shown.
    rel: String,
    /// Its extension, lower-cased, without the dot.
    ext: String,
    text: String,
}

impl Source {
    fn read(root: &Path, args: &Value) -> Result<Source> {
        let path = path_arg(args)?;
        let abs = confine(root, path)?;
        let text = std::fs::read_to_string(&abs)
            .map_err(|e| anyhow::anyhow!("cannot read {path:?}: {e}"))?;
        let ext = abs.extension().and_then(|e| e.to_str()).unwrap_or("");
        Ok(Source {
            rel: abs.strip_prefix(root).unwrap_or(&abs).display().to_string(),
            ext: ext.to_lowercase(),
            abs,
            text,
        })
    }

    /// Line `index` (from 0), without its ending; empty past the end.
    fn line(&self, index: usize) -> &str {
        self.text.lines().nth(index).unwrap_or("")
    }

    /// What its language is called in a tier-2 note.
    fn language(&self) -> String {
        let known = textual::lang_of(&self.ext).map(|l| l.name);
        match known.or_else(|| lsp::language_of(&self.ext)) {
            Some(name) => name.to_string(),
            None => format!(".{} files", self.ext),
        }
    }

    /// Give the server this file as it is now.
    fn sync(&self, client: &Client) -> Result<(), Failure> {
        client.sync(&self.abs, lsp::language_id(&self.ext), &self.text)
    }
}

/// The symbol a call points at.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    /// From 0.
    line: usize,
    /// Byte offset in the line, inside the name.
    byte: usize,
    name: String,
}

impl Target {
    fn find(source: &Source, args: &Value) -> Result<Target> {
        let Some(number) = args.get("line").and_then(Value::as_u64) else {
            bail!("this action needs 'line': the line the symbol is on, counted from 1");
        };
        let count = source.text.lines().count();
        if number == 0 || number as usize > count {
            bail!(
                "{} has {count} lines; there is no line {number}",
                source.rel
            );
        }
        let index = number as usize - 1;
        let line = source.line(index);
        let symbol = args.get("symbol").and_then(Value::as_str).map(str::trim);
        let column = args.get("column").and_then(Value::as_u64);
        let byte = match (symbol.filter(|s| !s.is_empty()), column) {
            (Some(symbol), _) => locate(line, symbol).ok_or_else(|| {
                anyhow::anyhow!(
                    "{symbol:?} is not on line {number} of {}, which reads: {}",
                    source.rel,
                    context(line)
                )
            })?,
            (None, Some(column)) => position::byte_of_column(line, column as usize),
            (None, None) => {
                let declared = textual::lang_of(&source.ext)
                    .and_then(|lang| lang.declaration(index, line))
                    .map(|decl| decl.byte);
                declared.ok_or_else(|| {
                    anyhow::anyhow!(
                        "which symbol? Pass 'symbol' (a name on the line) or 'column'. Line \
                         {number} of {} reads: {}",
                        source.rel,
                        context(line)
                    )
                })?
            }
        };
        let Some((start, name)) = textual::word_at(line, byte) else {
            bail!(
                "there is no identifier at column {} of line {number} of {}, which reads: {}",
                position::column_of_byte(line, byte),
                source.rel,
                context(line)
            );
        };
        Ok(Target {
            line: index,
            byte: start,
            name: name.to_string(),
        })
    }
}

/// Where `symbol` is on `line`: the byte offset of its first occurrence —
/// as a whole word when there is one — and, for a path like `a::b` or
/// `self.b`, of the last name in it, which is what the path names.
fn locate(line: &str, symbol: &str) -> Option<usize> {
    let is_name = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    // Where the last name starts within the symbol.
    let tail = symbol.trim_end_matches(|c| !is_name(c));
    let last = tail
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_name(*c))
        .last()
        .map_or(0, |(i, _)| i);
    let whole = textual::find_word(line, symbol).or_else(|| line.find(symbol));
    match whole {
        Some(start) => Some(start + last),
        // Written differently on the line (`Foo::bar` for `foo.bar`): the
        // last name alone.
        None => textual::find_word(line, &tail[last..]),
    }
}

/// `path:line:col`, with the line counted from 1 and the column in
/// characters.
fn place(path: &str, line: usize, text: &str, byte: usize) -> String {
    let column = position::column_of_byte(text, byte);
    format!("{path}:{}:{column}", line + 1)
}

/// A source line as context: trimmed, and cut if very long.
fn context(line: &str) -> String {
    let line = line.trim();
    match line.char_indices().nth(MAX_CONTEXT_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

/// One line of a result.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    /// `path:line:col`.
    place: String,
    /// `kind  name`, where known.
    label: Option<String>,
    /// The source line, or a remark in its place.
    context: Option<String>,
}

/// A result: what it is and how many, the entries, and how many more there
/// were. `server` names the language server that answered.
fn render(what: &str, noun: &str, entries: &[Entry], total: usize, server: Option<&str>) -> String {
    let by = server.map_or_else(String::new, |s| format!(" ({s})"));
    if total == 0 {
        return format!("no {what} found{by}");
    }
    let plural = if total == 1 { "" } else { "s" };
    let mut out = format!("{what} — {total} {noun}{plural}{by}");
    for entry in entries {
        out.push('\n');
        out.push_str(&entry.place);
        if let Some(label) = &entry.label {
            out.push_str("  ");
            out.push_str(label);
        }
        if let Some(context) = &entry.context {
            out.push_str("  — ");
            out.push_str(context);
        }
    }
    if total > entries.len() {
        out.push_str(&format!("\n… {} more", total - entries.len()));
    }
    out
}

/// A location in a server's answer.
struct Place {
    uri: String,
    line: usize,
    /// In UTF-16 units, as the server counts.
    character: u32,
}

/// The locations in the answer to `definition` or `references`: a
/// `Location`, a list of them, or a list of `LocationLink`s.
fn places(value: &Value) -> Vec<Place> {
    let one = |v: &Value| {
        let uri = v.get("targetUri").or_else(|| v.get("uri"))?.as_str()?;
        let range = v.get("targetSelectionRange").or_else(|| v.get("range"))?;
        Some(Place {
            uri: uri.to_string(),
            line: range["start"]["line"].as_u64()? as usize,
            character: range["start"]["character"].as_u64()? as u32,
        })
    };
    match value {
        Value::Array(items) => items.iter().filter_map(one).collect(),
        other => one(other).into_iter().collect(),
    }
}

/// A symbol in a server's answer to `documentSymbol` or `workspace/symbol`.
struct Symbol {
    /// With what contains it: `Widget.draw`.
    name: String,
    kind: &'static str,
    place: Place,
}

/// What LSP's `SymbolKind` numbers mean.
fn kind_name(kind: u64) -> &'static str {
    const KINDS: [&str; 26] = [
        "file",
        "module",
        "namespace",
        "package",
        "class",
        "method",
        "property",
        "field",
        "constructor",
        "enum",
        "interface",
        "function",
        "variable",
        "constant",
        "string",
        "number",
        "boolean",
        "array",
        "object",
        "key",
        "null",
        "enum-member",
        "struct",
        "event",
        "operator",
        "type-parameter",
    ];
    (kind.checked_sub(1))
        .and_then(|i| KINDS.get(i as usize))
        .copied()
        .unwrap_or("symbol")
}

/// The symbols in `value`, depth first: a tree of `DocumentSymbol`s (which
/// are in the document `uri`), or a flat list of `SymbolInformation` or
/// `WorkspaceSymbol` (which say where they are).
fn flatten(value: &Value, container: Option<&str>, uri: &str, out: &mut Vec<Symbol>) {
    for item in value.as_array().into_iter().flatten() {
        let Some(name) = item["name"].as_str() else {
            continue;
        };
        let container = container.or_else(|| item["containerName"].as_str());
        let name = match container.filter(|c| !c.is_empty()) {
            Some(container) => format!("{container}.{name}"),
            None => name.to_string(),
        };
        let location = &item["location"];
        let range = match location.is_object() {
            true => &location["range"],
            false => &item["selectionRange"],
        };
        out.push(Symbol {
            kind: kind_name(item["kind"].as_u64().unwrap_or(0)),
            place: Place {
                uri: location["uri"].as_str().unwrap_or(uri).to_string(),
                // A `WorkspaceSymbol` may leave the range to be resolved
                // later; its file is still worth a line.
                line: range["start"]["line"].as_u64().unwrap_or(0) as usize,
                character: range["start"]["character"].as_u64().unwrap_or(0) as u32,
            },
            name: name.clone(),
        });
        flatten(&item["children"], Some(&name), uri, out);
    }
}

/// The text of a `hover` answer — a `MarkupContent`, a `MarkedString` or a
/// list of those — trimmed and capped. `None` when there is none.
fn hover_text(value: &Value) -> Option<String> {
    fn text(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o.get("value")?.as_str().map(str::to_string),
            Value::Array(items) => {
                let parts: Vec<String> = items.iter().filter_map(text).collect();
                Some(parts.join("\n\n"))
            }
            _ => None,
        }
    }
    let text = text(value.get("contents")?)?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    Some(match lines.len() > MAX_HOVER_LINES {
        true => format!(
            "{}\n… {} more lines",
            lines[..MAX_HOVER_LINES].join("\n"),
            lines.len() - MAX_HOVER_LINES
        ),
        false => text.to_string(),
    })
}

/// Turns a server's locations into entries, reading the project files they
/// point into (each once) for the line and the column in characters.
struct Reader<'a> {
    root: &'a Path,
    /// The root with symlinks resolved, which is how servers spell paths.
    canonical: Option<PathBuf>,
    files: HashMap<PathBuf, Option<String>>,
}

impl<'a> Reader<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            canonical: std::fs::canonicalize(root).ok(),
            files: HashMap::new(),
        }
    }

    /// The entry for `place`. With `named`, the entry is labelled when the
    /// line there declares that name.
    fn entry(&mut self, place: &Place, named: Option<&str>) -> Entry {
        let Some(path) = lsp::path_of_uri(&place.uri) else {
            // Not a file (a server's virtual document): as it was given.
            return Entry {
                place: format!("{}:{}:{}", place.uri, place.line + 1, place.character + 1),
                label: None,
                context: None,
            };
        };
        let roots = [Some(self.root), self.canonical.as_deref()];
        let inside = roots
            .into_iter()
            .flatten()
            .find_map(|root| path.strip_prefix(root).ok());
        let Some(rel) = inside else {
            // Not read, so the column stays in the server's units.
            return Entry {
                place: format!(
                    "{}:{}:{}",
                    path.display(),
                    place.line + 1,
                    place.character + 1
                ),
                label: None,
                context: Some("outside the project, not read".to_string()),
            };
        };
        let rel_text = rel.display().to_string();
        let ext = rel.extension().and_then(|e| e.to_str()).unwrap_or("");
        let ext = ext.to_lowercase();
        let abs = self.root.join(rel);
        let text =
            (self.files.entry(abs.clone())).or_insert_with(|| std::fs::read_to_string(&abs).ok());
        let Some(line) = text.as_deref().and_then(|t| t.lines().nth(place.line)) else {
            return Entry {
                place: format!("{rel_text}:{}:{}", place.line + 1, place.character + 1),
                label: None,
                context: None,
            };
        };
        let byte = position::byte_of_utf16(line, place.character);
        let label = named.and_then(|name| {
            let decl = textual::lang_of(&ext)?.declaration(place.line, line)?;
            (decl.name == name).then(|| format!("{}  {}", decl.kind, decl.name))
        });
        Entry {
            place: self::place(&rel_text, place.line, line, byte),
            label,
            context: Some(context(line)),
        }
    }

    /// The entry for a symbol the server listed.
    fn symbol(&mut self, symbol: &Symbol) -> Entry {
        Entry {
            label: Some(format!("{}  {}", symbol.kind, symbol.name)),
            ..self.entry(&symbol.place, None)
        }
    }
}

/// The entry for a line a textual search landed on.
fn hit_entry(root: &Path, hit: &textual::Hit) -> Entry {
    let rel = hit.path.strip_prefix(root).unwrap_or(&hit.path);
    Entry {
        place: place(&rel.display().to_string(), hit.line, &hit.text, hit.byte),
        label: (hit.decl.as_ref()).map(|(kind, name)| format!("{kind}  {name}")),
        context: Some(context(&hit.text)),
    }
}

/// The declarations named `name` in `source`'s language, those in `source`
/// itself first: the likeliest is the nearest.
fn declarations_of(root: &Path, source: &Source, name: &str, keep: usize) -> textual::Found {
    let lang = textual::lang_of(&source.ext);
    // Everything is kept, then cut: the ones to put first may come last.
    let mut found = textual::find_declarations(root, lang, &|n| n == name, usize::MAX);
    found.hits.sort_by_key(|hit| hit.path != source.abs);
    found.hits.truncate(keep);
    found
}

/// Tier-2 `definition`: declarations with the name.
fn textual_definition(root: &Path, source: &Source, name: &str) -> String {
    let found = declarations_of(root, source, name, MAX_ENTRIES);
    let entries: Vec<Entry> = found.hits.iter().map(|h| hit_entry(root, h)).collect();
    let what = format!("definition of `{name}`");
    render(&what, "candidate", &entries, found.total, None)
}

/// Tier-2 `references`: lines with the name on them.
fn textual_references(root: &Path, source: &Source, name: &str) -> String {
    let lang = textual::lang_of(&source.ext);
    let found = textual::find_references(root, lang, &source.ext, name, MAX_ENTRIES);
    let entries: Vec<Entry> = found.hits.iter().map(|h| hit_entry(root, h)).collect();
    let what = format!("references to `{name}` (lines declaring it aside)");
    render(&what, "line", &entries, found.total, None)
}

/// Tier-2 `hover`: each declaration with the name, under its comment.
fn textual_hover(root: &Path, source: &Source, name: &str) -> String {
    let found = declarations_of(root, source, name, MAX_HOVER_DECLS);
    if found.total == 0 {
        return format!("no declaration of `{name}` found");
    }
    let mut out = Vec::new();
    for hit in &found.hits {
        let entry = hit_entry(root, hit);
        out.push(format!(
            "{}  {}",
            entry.place,
            entry.label.unwrap_or_default()
        ));
        let ext = hit.path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let text = match hit.path == source.abs {
            true => Some(source.text.clone()),
            false => std::fs::read_to_string(&hit.path).ok(),
        };
        if let (Some(lang), Some(text)) = (textual::lang_of(&ext.to_lowercase()), text) {
            let lines: Vec<&str> = text.lines().collect();
            if hit.line < lines.len() {
                let shown = lang.documented(&lines, hit.line);
                out.extend(shown.iter().map(|l| format!("    {}", l.trim_end())));
            }
        }
    }
    if found.total > found.hits.len() {
        out.push(format!(
            "… {} more declarations of `{name}`",
            found.total - found.hits.len()
        ));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests;
