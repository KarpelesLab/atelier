//! The tool end to end: against a fake language server (tier 1), without
//! one (tier 2), and the ways the first becomes the second.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::lsp::testutil::{Act, FakeServer};

const LIB: &str = "\
/// Says hello.
pub fn greet(name: &str) -> String {
    format!(\"hello {name}\")
}

pub struct Greeter;

impl Greeter {
    pub fn run(&self) -> String {
        let s = \"é𝄞\"; greet(s)
    }
}
";

const MAIN: &str = "\
fn main() {
    println!(\"{}\", lib::greet(\"x\"));
}
";

/// A project with the two files above.
fn project(name: &str) -> PathBuf {
    let root = crate::trust::testing::dir(&format!("symbols-{name}"));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), LIB).unwrap();
    std::fs::write(root.join("src/main.rs"), MAIN).unwrap();
    root
}

fn fake_config(timeout_secs: u64) -> Vec<LspServerConfig> {
    vec![LspServerConfig {
        name: "fake".to_string(),
        command: "fake-ls".to_string(),
        extensions: vec!["rs".to_string()],
        timeout_secs,
    }]
}

/// The tool on `server`, which leaves a marker file in the project each
/// time it is started.
fn tool_on(server: &FakeServer) -> SymbolsTool {
    let server = server.clone();
    SymbolsTool {
        servers: Arc::new(Servers::new(Box::new(move |_, root| {
            std::fs::write(root.join("server-started"), "")?;
            Ok(server.transport())
        }))),
    }
}

/// A tool with no way to start a server: for calls that must not try.
fn tool_alone() -> SymbolsTool {
    SymbolsTool {
        servers: Arc::new(Servers::new(Box::new(|_, _| {
            panic!("no server should be started")
        }))),
    }
}

fn run(tool: &SymbolsTool, root: &Path, configured: &[LspServerConfig], args: Value) -> String {
    let setup = Setup {
        configured,
        trusted: true,
        has: &|_| false,
    };
    tool.run(root, &args, &setup).unwrap()
}

/// A server that knows the fixture project. Positions are in UTF-16 units:
/// `greet` on line 10 of lib.rs is at unit 23 (character 22, byte 26), and
/// the server answers only when asked about exactly that.
fn fixture_server(root: &Path) -> FakeServer {
    let lib = lsp::uri_of_path(&root.join("src/lib.rs"));
    let main = lsp::uri_of_path(&root.join("src/main.rs"));
    let range = |line: u64, character: u64| {
        json!({
            "start": { "line": line, "character": character },
            "end": { "line": line, "character": character + 5 },
        })
    };
    FakeServer::new(move |method, params| {
        let on_greet_call = params["textDocument"]["uri"] == lib.as_str()
            && params["position"] == json!({ "line": 9, "character": 23 });
        Act::Reply(match method {
            "initialize" => json!({ "capabilities": {} }),
            "textDocument/definition" if on_greet_call => json!([{
                "targetUri": lib,
                "targetRange": range(0, 0),
                "targetSelectionRange": range(1, 7),
            }]),
            "textDocument/references" if on_greet_call => {
                assert_eq!(params["context"]["includeDeclaration"], false);
                json!([
                    { "uri": lib, "range": range(9, 23) },
                    { "uri": main, "range": range(1, 24) },
                    { "uri": "file:///opt/rust/library/core/src/fmt.rs", "range": range(41, 11) },
                ])
            }
            "textDocument/hover" if on_greet_call => json!({
                "contents": {
                    "kind": "markdown",
                    "value": "```rust\npub fn greet(name: &str) -> String\n```\n\nSays hello.\n",
                },
            }),
            "textDocument/documentSymbol" => json!([
                { "name": "greet", "kind": 12, "range": range(0, 0), "selectionRange": range(1, 7) },
                { "name": "Greeter", "kind": 23, "range": range(5, 0), "selectionRange": range(5, 11) },
                {
                    "name": "impl Greeter",
                    "kind": 19,
                    "range": range(7, 0),
                    "selectionRange": range(7, 5),
                    "children": [
                        { "name": "run", "kind": 6, "range": range(8, 4), "selectionRange": range(8, 11) },
                    ],
                },
            ]),
            "workspace/symbol" => json!([
                {
                    "name": "greet",
                    "kind": 12,
                    "location": { "uri": lib, "range": range(1, 7) },
                },
                {
                    "name": "run",
                    "kind": 6,
                    "containerName": "Greeter",
                    "location": { "uri": lib, "range": range(8, 11) },
                },
            ]),
            _ => Value::Null,
        })
    })
}

#[test]
fn every_action_round_trips_through_a_language_server() {
    let root = project("server");
    let server = fixture_server(&root);
    let tool = tool_on(&server);
    let config = fake_config(10);
    let at = |action: &str| json!({ "action": action, "path": "src/lib.rs", "line": 10, "symbol": "greet" });

    // The symbol sits after a 2-byte and a 4-byte character: byte 26,
    // character 22, UTF-16 unit 23. Only the last gets an answer.
    assert_eq!(
        run(&tool, &root, &config, at("definition")),
        "definition of `greet` — 1 location (fake)\n\
         src/lib.rs:2:8  fn  greet  — pub fn greet(name: &str) -> String {"
    );
    // By column (in characters, from 1) it is the same place.
    let by_column =
        json!({ "action": "definition", "path": "src/lib.rs", "line": 10, "column": 23 });
    assert_eq!(
        run(&tool, &root, &config, by_column),
        run(&tool, &root, &config, at("definition"))
    );

    // Coming back, unit 23 is shown as character column 23, not 24; the
    // location outside the project is listed and not read.
    assert_eq!(
        run(&tool, &root, &config, at("references")),
        "references to `greet` (its declaration aside) — 3 locations (fake)\n\
         src/lib.rs:10:23  — let s = \"é𝄞\"; greet(s)\n\
         src/main.rs:2:25  — println!(\"{}\", lib::greet(\"x\"));\n\
         /opt/rust/library/core/src/fmt.rs:42:12  — outside the project, not read"
    );

    assert_eq!(
        run(&tool, &root, &config, at("hover")),
        "hover for `greet` at src/lib.rs:10:23 (fake)\n\
         ```rust\npub fn greet(name: &str) -> String\n```\n\nSays hello."
    );

    assert_eq!(
        run(
            &tool,
            &root,
            &config,
            json!({ "action": "outline", "path": "src/lib.rs" })
        ),
        "outline of src/lib.rs — 4 symbols (fake)\n\
         src/lib.rs:2:8  function  greet  — pub fn greet(name: &str) -> String {\n\
         src/lib.rs:6:12  struct  Greeter  — pub struct Greeter;\n\
         src/lib.rs:8:6  object  impl Greeter  — impl Greeter {\n\
         src/lib.rs:9:12  method  impl Greeter.run  — pub fn run(&self) -> String {"
    );

    assert_eq!(
        run(
            &tool,
            &root,
            &config,
            json!({ "action": "search", "query": "gree" })
        ),
        "symbols matching `gree` — 2 symbols (fake)\n\
         src/lib.rs:2:8  function  greet  — pub fn greet(name: &str) -> String {\n\
         src/lib.rs:9:12  method  Greeter.run  — pub fn run(&self) -> String {"
    );
    assert_eq!(
        server.params_of("workspace/symbol"),
        json!({ "query": "gree" })
    );

    // One server for all of it, opened with the file's text.
    assert_eq!(server.starts(), 1);
    let opened = server.params_of("textDocument/didOpen");
    assert_eq!(opened["textDocument"]["languageId"], "rust");
    assert_eq!(opened["textDocument"]["text"], LIB);

    // Nothing found is said plainly, not papered over with a text search.
    let nothing =
        json!({ "action": "definition", "path": "src/lib.rs", "line": 6, "symbol": "Greeter" });
    assert_eq!(
        run(&tool, &root, &config, nothing),
        "no definition of `Greeter` found (fake)"
    );
    let nothing = json!({ "action": "hover", "path": "src/lib.rs", "line": 6 });
    assert_eq!(
        run(&tool, &root, &config, nothing),
        "no hover information for `Greeter` at src/lib.rs:6:12 (fake)"
    );
}

#[test]
fn a_file_edited_between_calls_is_sent_again() {
    let root = project("resync");
    let server = fixture_server(&root);
    let tool = tool_on(&server);
    let config = fake_config(10);
    let outline = json!({ "action": "outline", "path": "src/lib.rs" });
    run(&tool, &root, &config, outline.clone());
    run(&tool, &root, &config, outline.clone());
    let synced = |server: &FakeServer| -> Vec<String> {
        let methods = server.methods().into_iter();
        methods
            .filter(|m| m.starts_with("textDocument/did"))
            .collect()
    };
    assert_eq!(synced(&server), ["textDocument/didOpen"]);

    // The agent edits the file; the next call tells the server first.
    let edited = format!("{LIB}\npub fn added() {{}}\n");
    std::fs::write(root.join("src/lib.rs"), &edited).unwrap();
    run(&tool, &root, &config, outline);
    assert_eq!(
        synced(&server),
        ["textDocument/didOpen", "textDocument/didChange"]
    );
    let change = server.params_of("textDocument/didChange");
    assert_eq!(change["contentChanges"][0]["text"], edited.as_str());
    assert_eq!(change["textDocument"]["version"], 2);
    // And the change arrived before the query it was sent for.
    let methods = server.methods();
    let last = |method: &str| methods.iter().rposition(|m| m == method).unwrap();
    assert!(last("textDocument/didChange") < last("textDocument/documentSymbol"));
}

#[test]
fn a_slow_server_times_out_into_a_labelled_fallback() {
    let root = project("slow");
    let server = FakeServer::new(|method, _| match method {
        // Still "indexing": never answers a definition.
        "textDocument/definition" => Act::Ignore,
        "textDocument/hover" => Act::Reply(json!({ "contents": "fn greet" })),
        _ => Act::Reply(Value::Null),
    });
    let tool = tool_on(&server);
    let config = fake_config(1);
    let at = |action: &str| json!({ "action": action, "path": "src/lib.rs", "line": 10, "symbol": "greet" });
    let started = Instant::now();
    let out = run(&tool, &root, &config, at("definition"));
    assert!(started.elapsed() < std::time::Duration::from_secs(8));
    assert_eq!(
        out,
        "textual approximation (no language server for Rust: fake did not answer within 1s \
         (it may still be indexing the project; a later call may get through)) — names and \
         declaration shapes matched as text, not analysed\n\
         definition of `greet` — 1 candidate\n\
         src/lib.rs:2:8  fn  greet  — pub fn greet(name: &str) -> String {"
    );
    // The server was not given up on: the next call is answered by it.
    assert_eq!(
        run(&tool, &root, &config, at("hover")),
        "hover for `greet` at src/lib.rs:10:23 (fake)\nfn greet"
    );
    assert_eq!(server.starts(), 1);
}

#[test]
fn a_server_that_is_still_loading_is_waited_for_and_then_given_up_on() {
    let root = project("loading");
    let server = FakeServer::new(|_, _| Act::Reply(Value::Null));
    let tool = tool_on(&server);
    let config = fake_config(1);
    let outline = json!({ "action": "outline", "path": "src/lib.rs" });
    // Start it, then have it say it is busy and never that it is done.
    run(&tool, &root, &config, outline.clone());
    server.tell("experimental/serverStatus", json!({ "quiescent": false }));
    let asked = server.methods().len();
    let out = run(&tool, &root, &config, outline);
    assert!(
        out.starts_with(
            "textual approximation (no language server for Rust: fake did not answer within 1s"
        ),
        "{out}"
    );
    assert!(out.contains("src/lib.rs:6:12  struct  Greeter"), "{out}");
    // It was not asked while busy: an answer then would have been empty.
    assert_eq!(server.methods().len(), asked);
}

#[test]
fn a_server_that_dies_is_restarted_once_then_replaced_by_the_fallback() {
    let root = project("dies");
    let asked = AtomicUsize::new(0);
    let symbols = json!([{
        "name": "greet",
        "kind": 12,
        "range": { "start": { "line": 1, "character": 7 }, "end": { "line": 1, "character": 12 } },
        "selectionRange": { "start": { "line": 1, "character": 7 }, "end": { "line": 1, "character": 12 } },
    }]);
    let server = FakeServer::new(move |method, _| match method {
        // Crashes on the first and the third outline.
        "textDocument/documentSymbol" => match asked.fetch_add(1, Ordering::SeqCst) {
            1 => Act::Reply(symbols.clone()),
            _ => Act::Exit,
        },
        _ => Act::Reply(Value::Null),
    });
    let tool = tool_on(&server);
    let config = fake_config(10);
    let outline = json!({ "action": "outline", "path": "src/lib.rs" });

    // The crash is not seen: the server is started again, given the file
    // again, and asked again.
    assert_eq!(
        run(&tool, &root, &config, outline.clone()),
        "outline of src/lib.rs — 1 symbol (fake)\n\
         src/lib.rs:2:8  function  greet  — pub fn greet(name: &str) -> String {"
    );
    assert_eq!(server.starts(), 2);
    let opens = server.methods().into_iter();
    assert_eq!(opens.filter(|m| m == "textDocument/didOpen").count(), 2);

    // A second death is not followed by a third start.
    for _ in 0..2 {
        let out = run(&tool, &root, &config, outline.clone());
        assert!(
            out.starts_with(
                "textual approximation (no language server for Rust: fake exited again after \
                 being restarted; it is not started a third time)"
            ),
            "{out}"
        );
        assert!(out.contains("outline of src/lib.rs — 4 symbols\n"), "{out}");
    }
    assert_eq!(server.starts(), 2);
}

#[test]
fn an_untrusted_project_starts_no_server() {
    let root = project("untrusted");
    let server = fixture_server(&root);
    let tool = tool_on(&server);
    let config = fake_config(10);
    let setup = Setup {
        configured: &config,
        trusted: false,
        has: &|_| true,
    };
    for args in [
        json!({ "action": "definition", "path": "src/lib.rs", "line": 10, "symbol": "greet" }),
        json!({ "action": "references", "path": "src/lib.rs", "line": 10, "symbol": "greet" }),
        json!({ "action": "hover", "path": "src/lib.rs", "line": 10, "symbol": "greet" }),
        json!({ "action": "outline", "path": "src/lib.rs" }),
        json!({ "action": "search", "query": "greet" }),
    ] {
        let out = tool.run(&root, &args, &setup).unwrap();
        let language = match args["action"] == "search" {
            true => "this project",
            false => "Rust",
        };
        let note = format!(
            "textual approximation (no language server for {language}: this project is not \
             trusted, so fake is not started — a language server runs the project's code; \
             the user can accept it with /trust) — names and declaration shapes matched as \
             text, not analysed\n"
        );
        assert!(out.starts_with(&note), "{out}");
        assert!(out.contains("src/lib.rs:"), "{out}");
    }
    // The same for a server that would be detected rather than configured.
    std::fs::write(root.join("Cargo.toml"), "").unwrap();
    let detected = Setup {
        configured: &[],
        ..setup
    };
    let outline = json!({ "action": "outline", "path": "src/lib.rs" });
    let out = tool.run(&root, &outline, &detected).unwrap();
    assert!(out.contains("so rust-analyzer is not started"), "{out}");

    assert_eq!(server.starts(), 0);
    assert!(!root.join("server-started").exists());
    // The marker is what a start leaves: trusted, there is one.
    let trusted = Setup {
        trusted: true,
        ..setup
    };
    tool.run(&root, &outline, &trusted).unwrap();
    assert!(root.join("server-started").exists());
}

/// The same through the real entry point: the project's `atelier.toml`
/// names a server, nobody accepted the project, and the command — which
/// would leave a file behind — is not run.
#[cfg(unix)]
#[test]
fn a_configured_server_is_a_real_command_that_waits_for_trust() {
    let root = project("command");
    std::fs::write(
        root.join("atelier.toml"),
        "[[lsp]]\nname = \"marker\"\ncommand = \"sh -c 'touch started; cat > /dev/null'\"\n\
         extensions = [\"rs\"]\ntimeout_secs = 1\n",
    )
    .unwrap();
    let tool = SymbolsTool {
        servers: Arc::new(Servers::processes()),
    };
    let outline = json!({ "action": "outline", "path": "src/lib.rs" });
    // No record of a decision anywhere: untrusted.
    let out = tool.call_with(&root, &outline, Store::default()).unwrap();
    assert!(
        out.contains("this project is not trusted, so marker is not started"),
        "{out}"
    );
    assert!(out.contains("src/lib.rs:2:8  fn  greet"), "{out}");
    assert!(!root.join("started").exists());

    // Trusted, the command runs — and, not being a language server, is
    // given up on, with the textual result in its place.
    let settings = Settings::load(&root).unwrap();
    let setup = Setup {
        configured: &settings.lsp,
        trusted: true,
        has: &|_| false,
    };
    let out = tool.run(&root, &outline, &setup).unwrap();
    assert!(root.join("started").exists());
    assert!(
        out.starts_with(
            "textual approximation (no language server for Rust: marker did not answer \
             `initialize`)"
        ),
        "{out}"
    );
    tool.servers.kill_all();
}

#[test]
fn without_a_server_every_action_answers_from_the_text() {
    let root = project("textual");
    let tool = tool_alone();
    // Nothing configured, no Cargo.toml: no server is even looked for.
    let note = "textual approximation (no language server for Rust: the project root has no \
                Cargo.toml) — names and declaration shapes matched as text, not analysed\n";
    let at = |action: &str| json!({ "action": action, "path": "src/main.rs", "line": 2, "symbol": "lib::greet" });
    assert_eq!(
        run(&tool, &root, &[], at("definition")),
        format!(
            "{note}definition of `greet` — 1 candidate\n\
             src/lib.rs:2:8  fn  greet  — pub fn greet(name: &str) -> String {{"
        )
    );
    assert_eq!(
        run(&tool, &root, &[], at("references")),
        format!(
            "{note}references to `greet` (lines declaring it aside) — 2 lines\n\
             src/lib.rs:10:23  — let s = \"é𝄞\"; greet(s)\n\
             src/main.rs:2:25  — println!(\"{{}}\", lib::greet(\"x\"));"
        )
    );
    assert_eq!(
        run(&tool, &root, &[], at("hover")),
        format!(
            "{note}src/lib.rs:2:8  fn  greet\n    \
             /// Says hello.\n    \
             pub fn greet(name: &str) -> String {{"
        )
    );
    assert_eq!(
        run(
            &tool,
            &root,
            &[],
            json!({ "action": "outline", "path": "src/lib.rs" })
        ),
        format!(
            "{note}outline of src/lib.rs — 4 symbols\n\
             src/lib.rs:2:8  fn  greet  — pub fn greet(name: &str) -> String {{\n\
             src/lib.rs:6:12  struct  Greeter  — pub struct Greeter;\n\
             src/lib.rs:8:1  impl  impl Greeter  — impl Greeter {{\n\
             src/lib.rs:9:12  fn  run  — pub fn run(&self) -> String {{"
        )
    );
    // Search is about the project, case-insensitively, in every language.
    std::fs::write(root.join("tool.py"), "class GreetingCard:\n    pass\n").unwrap();
    assert_eq!(
        run(
            &tool,
            &root,
            &[],
            json!({ "action": "search", "query": "GREET" })
        ),
        "textual approximation (no language server for this project: none is configured or \
         detected for it) — names and declaration shapes matched as text, not analysed\n\
         symbols matching `GREET` — 3 declarations\n\
         src/lib.rs:2:8  fn  greet  — pub fn greet(name: &str) -> String {\n\
         src/lib.rs:6:12  struct  Greeter  — pub struct Greeter;\n\
         tool.py:1:7  class  GreetingCard  — class GreetingCard:"
    );

    // Nothing found, and languages the fallback has no forms for.
    let missing =
        json!({ "action": "definition", "path": "src/main.rs", "line": 2, "symbol": "println" });
    assert_eq!(
        run(&tool, &root, &[], missing),
        format!("{note}no definition of `println` found")
    );
    std::fs::write(root.join("notes.zig"), "fn helper() void {}\n").unwrap();
    let out = run(
        &tool,
        &root,
        &[],
        json!({ "action": "outline", "path": "notes.zig" }),
    );
    assert_eq!(
        out,
        "textual approximation (no language server for .zig files: none is known for .zig \
         files, and no [[lsp]] entry of atelier.toml lists them) — names and declaration \
         shapes matched as text, not analysed\n\
         no outline of notes.zig: no declaration forms are known for .zig files either; \
         read the file instead"
    );

    // A server that is known and missing is named.
    std::fs::write(root.join("Cargo.toml"), "").unwrap();
    let out = run(&tool, &root, &[], at("definition"));
    assert!(
        out.starts_with(
            "textual approximation (no language server for Rust: rust-analyzer is not on PATH)"
        ),
        "{out}"
    );
}

#[test]
fn long_results_are_capped_with_a_count_of_the_rest() {
    let root = project("caps");
    let tool = tool_alone();
    let many: String = (0..250)
        .map(|i| format!("fn item_{i}() {{ shared(); }}\n"))
        .collect();
    std::fs::write(
        root.join("src/many.rs"),
        format!("fn shared() {{}}\n{many}"),
    )
    .unwrap();

    let out = run(
        &tool,
        &root,
        &[],
        json!({ "action": "search", "query": "item_" }),
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[1], "symbols matching `item_` — 250 declarations");
    assert_eq!(lines.len(), 2 + MAX_ENTRIES + 1);
    assert_eq!(lines[lines.len() - 1], "… 150 more");

    let out = run(
        &tool,
        &root,
        &[],
        json!({ "action": "references", "path": "src/many.rs", "line": 1 }),
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[1],
        "references to `shared` (lines declaring it aside) — 250 lines"
    );
    assert_eq!(lines[lines.len() - 1], "… 150 more");

    let out = run(
        &tool,
        &root,
        &[],
        json!({ "action": "outline", "path": "src/many.rs" }),
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[1], "outline of src/many.rs — 251 symbols");
    assert_eq!(lines.len(), 2 + MAX_OUTLINE + 1);
    assert_eq!(lines[lines.len() - 1], "… 51 more");

    // A very long line is cut, on a character boundary.
    let long = format!("fn wide() {{ let s = \"{}\"; }}\n", "é".repeat(400));
    std::fs::write(root.join("src/wide.rs"), long).unwrap();
    let out = run(
        &tool,
        &root,
        &[],
        json!({ "action": "outline", "path": "src/wide.rs" }),
    );
    let shown = out.lines().nth(2).unwrap().split_once("  — ").unwrap().1;
    assert_eq!(shown.chars().count(), MAX_CONTEXT_CHARS + 1);
    assert!(shown.ends_with('…'));

    // A server's long hover is cut too.
    let text = (0..100).map(|i| format!("line {i}")).collect::<Vec<_>>();
    let hover = hover_text(&json!({ "contents": text.join("\n") })).unwrap();
    assert_eq!(hover.lines().count(), MAX_HOVER_LINES + 1);
    assert!(hover.ends_with("… 40 more lines"));
}

#[test]
fn a_symbol_is_found_on_its_line_by_name() {
    //          0         1         2         3
    //          0123456789012345678901234567890123456789
    let line = "    let foo_bar = foo(Bar::new(), self.foo);";
    // A whole word first: not the `foo` inside `foo_bar`.
    assert_eq!(locate(line, "foo"), Some(18));
    assert_eq!(locate(line, "foo_bar"), Some(8));
    // A path names its last part.
    assert_eq!(locate(line, "Bar::new"), Some(27));
    assert_eq!(locate(line, "self.foo"), Some(39));
    // With the call's parentheses, as models write it.
    assert_eq!(locate(line, "new()"), Some(27));
    // Spelt another way than on the line: the last name alone.
    assert_eq!(locate(line, "Bar.new"), Some(27));
    // Part of a word only when there is nothing better.
    assert_eq!(locate(line, "foo_"), Some(8));
    assert_eq!(locate(line, "missing"), None);
    // Bytes, not characters, after non-ASCII text.
    assert_eq!(locate("\"é𝄞\"; greet(s)", "greet"), Some(10));
}

#[test]
fn a_position_is_resolved_or_explained() {
    let root = project("target");
    let source = Source::read(&root, &json!({ "path": "src/lib.rs" })).unwrap();
    let find = |args: Value| Target::find(&source, &args).map_err(|e| e.to_string());
    let greet_call = Target {
        line: 9,
        byte: 26,
        name: "greet".to_string(),
    };
    assert_eq!(
        find(json!({ "line": 10, "symbol": "greet" })),
        Ok(greet_call)
    );
    // A column anywhere in the name, in characters.
    for column in [23, 25, 28] {
        let found = find(json!({ "line": 10, "column": column })).unwrap();
        assert_eq!((found.byte, found.name.as_str()), (26, "greet"));
    }
    // Neither: the name the line declares.
    let declared = find(json!({ "line": 2 })).unwrap();
    assert_eq!((declared.byte, declared.name.as_str()), (7, "greet"));

    let err = find(json!({ "line": 10 })).unwrap_err();
    assert!(err.starts_with("which symbol? Pass 'symbol'"), "{err}");
    assert!(err.ends_with("reads: let s = \"é𝄞\"; greet(s)"), "{err}");
    let err = find(json!({ "line": 10, "symbol": "nope" })).unwrap_err();
    assert!(
        err.starts_with("\"nope\" is not on line 10 of src/lib.rs"),
        "{err}"
    );
    let err = find(json!({ "line": 10, "column": 1 })).unwrap_err();
    assert!(
        err.starts_with("there is no identifier at column 1"),
        "{err}"
    );
    let err = find(json!({ "line": 99, "symbol": "x" })).unwrap_err();
    assert_eq!(err, "src/lib.rs has 12 lines; there is no line 99");
    let err = find(json!({ "symbol": "x" })).unwrap_err();
    assert!(err.starts_with("this action needs 'line'"), "{err}");
}

#[test]
fn bad_calls_are_refused_with_what_to_do() {
    let root = project("args");
    let tool = tool_alone();
    let setup = Setup {
        configured: &[],
        trusted: true,
        has: &|_| false,
    };
    let err = |args: Value| tool.run(&root, &args, &setup).unwrap_err().to_string();
    assert_eq!(err(json!({})), "missing required argument 'action'");
    assert!(err(json!({ "action": "rename" })).starts_with("unknown action \"rename\""));
    assert!(err(json!({ "action": "search" })).starts_with("search needs a 'query'"));
    assert!(err(json!({ "action": "search", "query": "  " })).starts_with("search needs"));
    assert_eq!(
        err(json!({ "action": "outline" })),
        "missing required argument 'path'"
    );
    assert!(
        err(json!({ "action": "outline", "path": "../outside.rs" }))
            .contains("escapes the project root")
    );
    assert!(err(json!({ "action": "outline", "path": "src/none.rs" })).starts_with("cannot read"));
    // The name Claude models use for the argument works too.
    let out = tool
        .run(
            &root,
            &json!({ "action": "outline", "file_path": "src/main.rs" }),
            &setup,
        )
        .unwrap();
    assert!(out.contains("src/main.rs:1:4  fn  main"), "{out}");
}

#[test]
fn server_answers_in_every_shape_are_understood() {
    // A single Location, a list, LocationLinks, and nothing.
    let location = json!({
        "uri": "file:///p/a.rs",
        "range": { "start": { "line": 3, "character": 4 }, "end": { "line": 3, "character": 9 } },
    });
    let link = json!({
        "targetUri": "file:///p/b.rs",
        "targetRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 9, "character": 0 } },
        "targetSelectionRange": { "start": { "line": 1, "character": 2 }, "end": { "line": 1, "character": 5 } },
    });
    let spots = |v: &Value| -> Vec<(String, usize, u32)> {
        let found = places(v).into_iter();
        found.map(|p| (p.uri, p.line, p.character)).collect()
    };
    assert_eq!(spots(&location), [("file:///p/a.rs".to_string(), 3, 4)]);
    assert_eq!(
        spots(&json!([location, link])),
        [
            ("file:///p/a.rs".to_string(), 3, 4),
            ("file:///p/b.rs".to_string(), 1, 2)
        ]
    );
    assert!(spots(&Value::Null).is_empty());
    assert!(spots(&json!([])).is_empty());

    // Hover contents: markup, a marked string, a list of both, nothing.
    let hover = |contents: Value| hover_text(&json!({ "contents": contents }));
    assert_eq!(hover(json!("plain")), Some("plain".to_string()));
    assert_eq!(
        hover(json!({ "kind": "plaintext", "value": " text\n" })),
        Some("text".to_string())
    );
    assert_eq!(
        hover(json!([{ "language": "go", "value": "func F()" }, "Docs."])),
        Some("func F()\n\nDocs.".to_string())
    );
    assert_eq!(hover(json!("")), None);
    assert_eq!(hover_text(&Value::Null), None);

    // Symbol kinds, including ones no version of the protocol has.
    assert_eq!(kind_name(5), "class");
    assert_eq!(kind_name(26), "type-parameter");
    assert_eq!(kind_name(0), "symbol");
    assert_eq!(kind_name(99), "symbol");

    // A workspace symbol whose range is left for later, and a location
    // that is no file, still make a line each.
    let mut symbols = Vec::new();
    flatten(
        &json!([{ "name": "Lazy", "kind": 5, "location": { "uri": "jdt://x/Lazy.class" } }]),
        None,
        "",
        &mut symbols,
    );
    let root = PathBuf::from("/p");
    let entry = Reader::new(&root).symbol(&symbols[0]);
    assert_eq!(
        render("symbols", "symbol", &[entry], 1, Some("jdtls")),
        "symbols — 1 symbol (jdtls)\njdt://x/Lazy.class:1:1  class  Lazy"
    );
}

#[test]
fn the_tool_is_read_only_confined_and_registered() {
    let registry = crate::tools::builtin_registry();
    let tool = registry.get("symbols").expect("registered");
    let args = json!({ "action": "outline", "path": "src/lib.rs" });
    assert!(!tool.mutates(&args));
    assert!(!tool.requires_approval_in(&args, Path::new("/p")));
    let spec = tool.spec();
    assert!(spec.description.contains("textual approximation"));
    assert!(spec.description.contains("grep"));
    // The explore agent, and so `atelier review`, may use it.
    assert!(crate::agents::read_only("symbols"));
}
