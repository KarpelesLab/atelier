//! The tool against real language servers. Ignored by default: each test
//! needs its server on `PATH` (and says so, and passes, when it is not
//! there), and CI has none.
//!
//! ```sh
//! cargo test --lib symbols::live -- --ignored --nocapture
//! ```
//!
//! Each project is a few lines in a directory whose name has a space and an
//! accent (the URI is escaped both ways), with the symbol asked about
//! sitting after `"é𝄞日本"` on its line: 2-, 4- and 3-byte characters, the
//! second of which is two UTF-16 units — bytes, characters and units all
//! differ there. The first call of every test is a `search`, on a server
//! that has just started and been given no file.
//!
//! What these found when first run (rust-analyzer 1.97, clangd 22.1, gopls 0.23,
//! typescript-language-server 6.1, pyright 1.1.414) is in the tests beside
//! this file, against the fake server: they are the regression tests, these
//! are the way to find the next difference.

use super::*;

/// A project directory for `language`, spelled awkwardly.
fn project(language: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = crate::trust::testing::dir(&format!("symbols-live-{language}")).join("my proj é");
    for (path, text) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    root
}

/// The tool on real processes, or `None` — with a line saying so — when
/// `program` is not installed.
fn tool_with(program: &str) -> Option<SymbolsTool> {
    if !crate::context::on_path(program) {
        println!("skipped: {program} is not on PATH");
        return None;
    }
    Some(SymbolsTool {
        servers: Arc::new(Servers::processes()),
    })
}

/// One call, in a trusted project with no `[[lsp]]` entry: the server is
/// the detected one.
fn run(tool: &SymbolsTool, root: &Path, args: Value) -> String {
    let setup = Setup {
        configured: &[],
        trusted: true,
        has: &crate::context::on_path,
    };
    let out = tool.run(root, &args, &setup).unwrap();
    println!("{args}\n{out}\n");
    out
}

fn at(action: &str, path: &str, line: u64, symbol: &str) -> Value {
    json!({ "action": action, "path": path, "line": line, "symbol": symbol })
}

/// The server the tool was written against, for what the others changed:
/// a file opened before `search`, the declaration taken out here.
#[test]
#[ignore]
fn rust_analyzer() {
    let Some(tool) = tool_with("rust-analyzer") else {
        return;
    };
    let root = project(
        "rust",
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"greet\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            (
                "src/lib.rs",
                "/// Says hello.\npub fn greet(name: &str) -> String {\n    \
                 format!(\"hello {name}\")\n}\n\npub struct Greeter;\n\nimpl Greeter {\n    \
                 pub fn run(&self) -> String {\n        let s = \"é𝄞日本\"; greet(s)\n    }\n}\n",
            ),
            (
                "src/main.rs",
                "fn main() {\n    println!(\"{}\", greet::greet(\"x\"));\n}\n",
            ),
        ],
    );
    // It loads the standard library first, which can outlast one call: the
    // answer is then the textual one, and says so.
    let search = json!({ "action": "search", "query": "gree" });
    let mut found = run(&tool, &root, search.clone());
    for _ in 0..5 {
        if !found.starts_with("textual approximation") {
            break;
        }
        assert!(
            found.contains("rust-analyzer did not answer within 20s"),
            "{found}"
        );
        found = run(&tool, &root, search.clone());
    }
    assert!(found.starts_with("symbols matching `gree` — "), "{found}");
    assert!(found.contains("(rust-analyzer)\n"), "{found}");
    assert!(
        found.contains("\nsrc/lib.rs:2:8  function  greet  — "),
        "{found}"
    );
    assert!(
        found.contains("\nsrc/lib.rs:6:12  struct  Greeter  — "),
        "{found}"
    );

    let uses = "src/lib.rs:10:25  — let s = \"é𝄞日本\"; greet(s)\n\
                src/main.rs:2:27  — println!(\"{}\", greet::greet(\"x\"));";
    for line in [10, 2] {
        let found = run(&tool, &root, at("references", "src/lib.rs", line, "greet"));
        let mut lines: Vec<&str> = found.lines().collect();
        assert_eq!(
            lines.remove(0),
            "references to `greet` (its declaration aside) — 2 locations (rust-analyzer)"
        );
        lines.sort_unstable();
        assert_eq!(lines.join("\n"), uses, "asked at line {line}");
    }
    assert_eq!(
        run(
            &tool,
            &root,
            at("definition", "src/main.rs", 2, "greet::greet")
        ),
        "definition of `greet` — 1 location (rust-analyzer)\n\
         src/lib.rs:2:8  fn  greet  — pub fn greet(name: &str) -> String {"
    );
    let hover = run(&tool, &root, at("hover", "src/lib.rs", 10, "greet"));
    assert!(hover.contains("Says hello."), "{hover}");
    tool.servers.kill_all();
}

#[test]
#[ignore]
fn clangd() {
    let Some(tool) = tool_with("clangd") else {
        return;
    };
    let root = project(
        "cpp",
        &[
            (
                "src/greet.hpp",
                "#pragma once\n#include <string>\n\n/// Says hello.\n\
                 std::string greet(const std::string &name);\n\n\
                 struct Greeter {\n  std::string run() const;\n};\n",
            ),
            (
                "src/greet.cpp",
                "#include \"greet.hpp\"\n\n\
                 std::string greet(const std::string &name) { return \"hello \" + name; }\n\n\
                 std::string Greeter::run() const {\n  auto s = \"é𝄞日本\"; return greet(s);\n}\n",
            ),
            (
                "src/main.cpp",
                "#include \"greet.hpp\"\n\nint main() {\n  Greeter g;\n  \
                 return g.run().size() + greet(\"x\").size();\n}\n",
            ),
        ],
    );
    let command = |file: &str| json!({ "directory": root, "file": file, "command": format!("clang++ -std=c++17 -c {file}") });
    let commands = json!([command("src/greet.cpp"), command("src/main.cpp")]);
    std::fs::write(root.join("compile_commands.json"), commands.to_string()).unwrap();

    // Cold: clangd indexes nothing until it is shown a file. The standard
    // library's `greater_equal` is not the project's.
    let found = run(&tool, &root, json!({ "action": "search", "query": "gree" }));
    assert!(
        found.starts_with("symbols matching `gree` — 2 symbols (clangd)\n"),
        "{found}"
    );
    assert!(
        found.contains("\nsrc/greet.hpp:7:8  class  Greeter  — struct Greeter {"),
        "{found}"
    );
    assert!(
        found.contains("\nsrc/greet.cpp:3:13  function  greet  — "),
        "{found}"
    );
    assert!(found.contains("more outside the project"), "{found}");

    assert_eq!(
        run(&tool, &root, at("definition", "src/greet.cpp", 6, "greet")),
        "definition of `greet` — 1 location (clangd)\n\
         src/greet.cpp:3:13  function  greet  — \
         std::string greet(const std::string &name) { return \"hello \" + name; }"
    );
    // Across files, into one that was never opened.
    assert_eq!(
        run(&tool, &root, at("definition", "src/main.cpp", 5, "run")),
        "definition of `run` — 1 location (clangd)\n\
         src/greet.cpp:5:22  function  run  — std::string Greeter::run() const {"
    );
    // Asked at the body and at the prototype, neither of which is a use.
    let uses = "src/greet.cpp:6:27  — auto s = \"é𝄞日本\"; return greet(s);\n\
                src/main.cpp:5:27  — return g.run().size() + greet(\"x\").size();";
    for (path, line) in [("src/greet.cpp", 3), ("src/greet.hpp", 5)] {
        assert_eq!(
            run(&tool, &root, at("references", path, line, "greet")),
            format!("references to `greet` (its declaration aside) — 2 locations (clangd)\n{uses}")
        );
    }
    // Asked at a use, the prototype is still listed, and labelled.
    let from_use = run(&tool, &root, at("references", "src/greet.cpp", 6, "greet"));
    assert!(from_use.contains("— 3 locations (clangd)"), "{from_use}");
    assert!(
        from_use.contains("\nsrc/greet.hpp:5:13  function  greet  — "),
        "{from_use}"
    );

    let hover = run(&tool, &root, at("hover", "src/greet.cpp", 6, "greet"));
    assert!(
        hover.starts_with("hover for `greet` at src/greet.cpp:6:27 (clangd)\n"),
        "{hover}"
    );
    assert!(hover.contains("Says hello."), "{hover}");
    assert_eq!(
        run(
            &tool,
            &root,
            json!({ "action": "outline", "path": "src/greet.hpp" })
        ),
        "outline of src/greet.hpp — 3 symbols (clangd)\n\
         src/greet.hpp:5:13  function  greet  — std::string greet(const std::string &name);\n\
         src/greet.hpp:7:8  class  Greeter  — struct Greeter {\n\
         src/greet.hpp:8:15  method  Greeter.run  — std::string run() const;"
    );
    let outside = run(&tool, &root, at("definition", "src/greet.cpp", 3, "string"));
    assert!(
        outside.ends_with("— outside the project, not read"),
        "{outside}"
    );
    tool.servers.kill_all();
}

#[test]
#[ignore]
fn gopls() {
    let Some(tool) = tool_with("gopls") else {
        return;
    };
    let root = project(
        "go",
        &[
            ("go.mod", "module example.test/greet\n\ngo 1.22\n"),
            (
                "greet.go",
                "package main\n\n// Greet says hello.\nfunc Greet(name string) string {\n\t\
                 return \"hello \" + name\n}\n\ntype Greeter struct{}\n\n\
                 func (g Greeter) Run() string {\n\ts := \"é𝄞日本\"; return Greet(s)\n}\n",
            ),
            (
                "main.go",
                "package main\n\nimport \"fmt\"\n\nfunc main() {\n\t\
                 fmt.Println(Greet(\"x\"), Greeter{}.Run())\n}\n",
            ),
        ],
    );
    // Without `symbolScope`, 96 of gopls's 100 answers were `unicode.Greek`
    // and its neighbours.
    assert_eq!(
        run(&tool, &root, json!({ "action": "search", "query": "gree" })),
        "symbols matching `gree` — 4 symbols (gopls)\n\
         greet.go:4:6  function  example.test/greet.Greet  — func Greet(name string) string {\n\
         greet.go:8:6  struct  example.test/greet.Greeter  — type Greeter struct{}\n\
         greet.go:10:18  method  example.test/greet.Greeter.Run  — func (g Greeter) Run() string {\n\
         main.go:5:6  function  example.test/greet.main  — func main() {"
    );
    assert_eq!(
        run(&tool, &root, at("definition", "greet.go", 11, "Greet")),
        "definition of `Greet` — 1 location (gopls)\n\
         greet.go:4:6  func  Greet  — func Greet(name string) string {"
    );
    let uses = "greet.go:11:22  — s := \"é𝄞日本\"; return Greet(s)\n\
                main.go:6:14  — fmt.Println(Greet(\"x\"), Greeter{}.Run())";
    for line in [11, 4] {
        assert_eq!(
            run(&tool, &root, at("references", "greet.go", line, "Greet")),
            format!("references to `Greet` (its declaration aside) — 2 locations (gopls)\n{uses}")
        );
    }
    assert_eq!(
        run(&tool, &root, at("hover", "greet.go", 11, "Greet")),
        "hover for `Greet` at greet.go:11:22 (gopls)\n\
         func Greet(name string) string\nGreet says hello."
    );
    assert_eq!(
        run(
            &tool,
            &root,
            json!({ "action": "outline", "path": "greet.go" })
        ),
        "outline of greet.go — 3 symbols (gopls)\n\
         greet.go:4:6  function  Greet  — func Greet(name string) string {\n\
         greet.go:8:6  struct  Greeter  — type Greeter struct{}\n\
         greet.go:10:18  method  (Greeter).Run  — func (g Greeter) Run() string {"
    );
    let outside = run(&tool, &root, at("definition", "main.go", 6, "Println"));
    assert!(
        outside.ends_with("— outside the project, not read"),
        "{outside}"
    );

    // An edit between two calls is in the second one's answer.
    let text = std::fs::read_to_string(root.join("greet.go")).unwrap();
    std::fs::write(
        root.join("greet.go"),
        format!("{text}\nfunc Added() {{}}\n"),
    )
    .unwrap();
    let outline = run(
        &tool,
        &root,
        json!({ "action": "outline", "path": "greet.go" }),
    );
    assert!(
        outline.ends_with("\ngreet.go:14:6  function  Added  — func Added() {}"),
        "{outline}"
    );
    tool.servers.kill_all();
}

#[test]
#[ignore]
fn typescript_language_server() {
    let Some(tool) = tool_with("typescript-language-server") else {
        return;
    };
    let root = project(
        "ts",
        &[
            ("package.json", "{\"name\":\"greet\",\"private\":true}\n"),
            (
                "tsconfig.json",
                "{\"compilerOptions\":{\"strict\":true,\"target\":\"es2020\",\"module\":\"es2020\"},\
                 \"include\":[\"src\"]}\n",
            ),
            (
                "src/greet.ts",
                "/** Says hello. */\nexport function greet(name: string): string {\n  \
                 return \"hello \" + name;\n}\n\nexport class Greeter {\n  run(): string {\n    \
                 const s = \"é𝄞日本\"; return greet(s);\n  }\n}\n",
            ),
            (
                "src/main.ts",
                "import { greet, Greeter } from \"./greet\";\n\n\
                 console.log(greet(\"x\"), new Greeter().run());\n",
            ),
        ],
    );
    // Cold, tsserver answered "No Project" (and twenty lines of stack);
    // and it places a symbol at the start of its declaration.
    assert_eq!(
        run(&tool, &root, json!({ "action": "search", "query": "gree" })),
        "symbols matching `gree` — 2 symbols (typescript-language-server)\n\
         src/greet.ts:2:17  function  greet  — export function greet(name: string): string {\n\
         src/greet.ts:6:14  class  Greeter  — export class Greeter {"
    );
    // Asked right after a file is opened, it answered with the references
    // in that file alone; and it lists the declaration unless asked at it.
    let uses = "src/greet.ts:8:30  — const s = \"é𝄞日本\"; return greet(s);\n\
                src/main.ts:1:10  — import { greet, Greeter } from \"./greet\";\n\
                src/main.ts:3:13  — console.log(greet(\"x\"), new Greeter().run());";
    for line in [8, 2] {
        assert_eq!(
            run(
                &tool,
                &root,
                at("references", "src/greet.ts", line, "greet")
            ),
            format!(
                "references to `greet` (its declaration aside) — 3 locations \
                 (typescript-language-server)\n{uses}"
            )
        );
    }
    assert_eq!(
        run(&tool, &root, at("definition", "src/main.ts", 3, "run")),
        "definition of `run` — 1 location (typescript-language-server)\n\
         src/greet.ts:7:3  method  run  — run(): string {"
    );
    let hover = run(&tool, &root, at("hover", "src/greet.ts", 8, "greet"));
    assert!(
        hover.contains("function greet(name: string): string"),
        "{hover}"
    );
    assert!(hover.contains("Says hello."), "{hover}");
    let outline = run(
        &tool,
        &root,
        json!({ "action": "outline", "path": "src/greet.ts" }),
    );
    assert!(
        outline.contains("\nsrc/greet.ts:7:3  method  Greeter.run  — run(): string {"),
        "{outline}"
    );
    let outside = run(&tool, &root, at("definition", "src/main.ts", 3, "log"));
    assert!(
        outside.ends_with("— outside the project, not read"),
        "{outside}"
    );
    tool.servers.kill_all();
}

#[test]
#[ignore]
fn pyright() {
    let Some(tool) = tool_with("pyright-langserver") else {
        return;
    };
    let root = project(
        "py",
        &[
            ("requirements.txt", "\n"),
            (
                "greet.py",
                "def greet(name: str) -> str:\n    \"\"\"Says hello.\"\"\"\n    \
                 return \"hello \" + name\n\n\nclass Greeter:\n    def run(self) -> str:\n        \
                 s = \"é𝄞日本\"; return greet(s)\n",
            ),
            (
                "main.py",
                "from greet import greet, Greeter\n\nprint(greet(\"x\"), Greeter().run())\n",
            ),
        ],
    );
    assert_eq!(
        run(&tool, &root, json!({ "action": "search", "query": "gree" })),
        "symbols matching `gree` — 2 symbols (pyright-langserver)\n\
         greet.py:1:5  function  greet  — def greet(name: str) -> str:\n\
         greet.py:6:7  class  Greeter  — class Greeter:"
    );
    // Asked to leave the declaration out, pyright left out the place it
    // was asked at: three different lists for one symbol.
    let uses = "greet.py:8:28  — s = \"é𝄞日本\"; return greet(s)\n\
                main.py:1:19  — from greet import greet, Greeter\n\
                main.py:3:7  — print(greet(\"x\"), Greeter().run())";
    for (path, line) in [("greet.py", 8), ("greet.py", 1), ("main.py", 3)] {
        let found = run(&tool, &root, at("references", path, line, "greet"));
        let mut lines: Vec<&str> = found.lines().collect();
        assert_eq!(
            lines.remove(0),
            "references to `greet` (its declaration aside) — 3 locations (pyright-langserver)"
        );
        // In the order the server walked the files, which varies.
        lines.sort_unstable();
        assert_eq!(lines.join("\n"), uses, "asked at {path}:{line}");
    }
    assert_eq!(
        run(&tool, &root, at("definition", "main.py", 3, "run")),
        "definition of `run` — 1 location (pyright-langserver)\n\
         greet.py:7:9  def  run  — def run(self) -> str:"
    );
    assert_eq!(
        run(&tool, &root, at("hover", "greet.py", 8, "greet")),
        "hover for `greet` at greet.py:8:28 (pyright-langserver)\n\
         (function) def greet(name: str) -> str\n\nSays hello."
    );
    let outline = run(
        &tool,
        &root,
        json!({ "action": "outline", "path": "greet.py" }),
    );
    assert!(
        outline.contains("\ngreet.py:7:9  method  Greeter.run  — def run(self) -> str:"),
        "{outline}"
    );
    let outside = run(&tool, &root, at("definition", "main.py", 3, "print"));
    assert!(
        outside.ends_with("— outside the project, not read"),
        "{outside}"
    );
    tool.servers.kill_all();
}
