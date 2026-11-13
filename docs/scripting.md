# Scripting: the `node` tool

`node` runs a JavaScript snippet on an embedded, pure-Rust interpreter
([kataan](https://github.com/KarpelesLab/kataan)) with host APIs atelier
implements and mediates itself — a safer alternative to `bash` for logic
that's awkward as a shell one-liner (loops, parsing, small transforms).
Because the engine core is sans-I/O, every effect (`fs`, network) is a
Rust-implemented global that checks the project boundary on every call.

## Parameters

| Param        | Type    | Required | Meaning |
|--------------|---------|----------|---------|
| `code`       | string  | yes      | JavaScript source to run |
| `network`    | boolean | no       | Install `fetch`/`httpGet`/`httpRequest` (default false). Setting this `true` makes the call require approval |
| `tools`      | boolean | no       | Install the [`tools`](#tools--calling-other-tools-code-mode) object, to call other tools from the script (default false) |
| `timeout_ms` | integer | no       | Wall-clock budget in ms (default **5000**, clamped to at most 600000) |

The tool result is the script's captured `console` output; if nothing was
logged, it falls back to the display form of the script's final expression
value (or is empty for `undefined`). Output past 10KB comes back as its start and its end, with all of it
saved to a file ([spilled output](tools.md#long-output-goes-to-a-file)). A thrown
error is appended as `Uncaught <message>` (a syntax error is reported the
same way, without running anything).

## No modules

There is **no `require`/`import`** — CommonJS and ES modules are
unavailable. `fs` and `console` are plain globals already in scope; just use
them directly.

## `fs` — synchronous, project-confined

Every method is synchronous (no promises/callbacks/await) and every path
argument is resolved against the project root the same way built-in file
tools are (`tools::confine`): relative paths are joined to the root, `.`/`..`
are normalized, and anything that still doesn't stay under the root throws a
catchable JS error instead of touching the filesystem.

| Method | Signature | Behavior |
|--------|-----------|----------|
| `fs.readFile` | `(path) -> string` | Reads a UTF-8 text file |
| `fs.writeFile` | `(path, content)` | Writes (creating parent directories as needed); no return value |
| `fs.readdir` | `(path) -> string[]` | Directory entry names |
| `fs.exists` | `(path) -> boolean` | Whether the path exists |
| `fs.mkdir` | `(path)` | Creates the directory (and parents); no return value |
| `fs.stat` | `(path) -> {isFile, isDirectory, size, mtimeMs}` | File metadata (follows symlinks) |
| `fs.appendFile` | `(path, content)` | Appends text, creating the file/parents if needed |
| `fs.rm` | `(path)` | Removes a file (throws on a directory) |
| `fs.rmdir` | `(path)` | Removes an empty directory (non-recursive) |
| `fs.rename` | `(from, to)` | Renames/moves within the project root |
| `fs.readFileBytes` | `(path) -> Uint8Array` | Reads a file as raw bytes |
| `fs.writeFileBytes` | `(path, data)` | Writes a `Uint8Array` or plain byte array |
| `fs.glob` | `(pattern) -> string[]` | Project files matching a glob (respects .gitignore) |
| `fs.copyFile` | `(src, dst)` | Copy a file (both confined; creates dst dirs) |

```js
fs.writeFile("notes/todo.txt", "buy milk\n");
var text = fs.readFile("notes/todo.txt");
console.log("wrote and read back:", text);
```

```js
try {
  fs.readFile("../../etc/passwd");
} catch (e) {
  console.log("blocked:", e.message);
}
```

## `console`

`console.log(...)` and `console.error(...)` stringify and space-join their
arguments and append a line to the captured output buffer (`console.error`
lines are prefixed `error: `). That's the entire API — no `console.warn`,
`console.table`, etc.

```js
console.log("sum:", 2 + 3);
```

## `path` and `os`

Two more always-available globals (pure, no I/O — not gated by `network`):

- **`path`** — POSIX path helpers: `path.join(...parts)`, `path.dirname(p)`,
  `path.basename(p[, ext])`, `path.extname(p)`, `path.normalize(p)`,
  `path.isAbsolute(p)`, and `path.sep` (`"/"`).
- **`os`** — host facts (Node naming): `os.platform()` (e.g. `"darwin"`,
  `"linux"`, `"win32"`), `os.arch()`, `os.type()` (e.g. `"Darwin"`), and
  `os.EOL` (`"\n"`).

```js
console.log(path.join("a", "b", "..", "c.txt")); // a/c.txt
console.log(os.platform(), os.arch());
```

## `encoding` and `hash`

Pure, always-available (not gated by `network`):

- **`encoding`** — `base64Encode(str)`, `base64Decode(b64)`, `hexEncode(str)`,
  `hexDecode(hex)` (operate on the string's UTF-8 bytes; decode throws on
  invalid input).
- **`hash`** — `hash.sha256(str)` → lowercase hex digest.

```js
console.log(encoding.base64Encode("hi"), hash.sha256("abc"));
```

## `tools` — calling other tools (code mode)

With `tools: true` a script can call the session's other tools and work on
what they return. Only what the script prints goes back to the model, so the
intermediate results never enter the conversation:

```js
const issues = tools.call("mcp__tracker__search", { query: "label:bug" });
const files = issues.json.items.map(i => tools.call("read", { path: i.file }).text);
console.log(files.filter(f => f.includes("TODO")).length);
```

That is one tool result of one line, where the same work done call by call
would have put the search result and every file into the context. It is the
thing to reach for when a task takes many calls whose results only feed the
next step, or when a large result is to be filtered, joined or counted.

| Function | Signature | Behavior |
|----------|-----------|----------|
| `tools.call` | `(name[, args]) -> {ok, text, json?}` | Runs the tool and returns its result. `text` is the result the model would have been given. `json` is present when that text is a JSON object or array (what MCP tools commonly return), parsed. `args` defaults to `{}` |
| `tools.list` | `() -> [{name, description}]` | The tools a script can call |
| `tools.schema` | `(name) -> object` | A tool's input schema (JSON Schema); throws for a name `tools.list()` does not have |

All three are synchronous, like `fs`.

### Every call is checked as if the model had made it

`tools.call` does not run anything itself. The script's thread sends the
name and the arguments to the session, which treats them as a call of that
tool by the model and takes it through the same steps, in the same order:
`pre_tool` [hooks](hooks.md) (matched on and told the real tool name),
the [permission rules](permissions.md) and the mode, the approval prompt
when one is due, the tool, `post_tool` hooks. So:

- a `deny` rule on a tool refuses it from a script too, and an `allow` rule
  lets it run unasked;
- a tool that asks (`bash`, `web_fetch`, an MCP tool) asks — the prompt names
  the real tool and its arguments, after a line saying a script is asking,
  and "always" saves the usual rule for that tool;
- plan mode refuses the calls that change something, `ask-edits` asks
  before a `write`; (`node` itself is not available in plan mode, as its own
  `fs` can write);
- the tool's own limits apply: `read` pages a long file, `bash` output past
  its cap is [spilled](tools.md#long-output-goes-to-a-file), and files a
  script reads are tracked for `edit` like any other read.

`node` with `tools: true` is approved exactly as `node` without it — it
gives a script nothing a call would not have had.

A call that is refused — by a rule, a hook, the mode or the user — or that
fails **does not throw**: it returns `{ok: false, text: "<why>"}`, and the
script goes on. Check `ok` where it matters.

```js
var r = tools.call("bash", { command: "cargo metadata --format-version 1" });
if (!r.ok) { console.log("could not run cargo:", r.text); }
else { console.log(r.json.packages.map(function (p) { return p.name; }).join("\n")); }
```

### What can be called

Every tool the session has, by its real name — MCP tools as
`mcp__<server>__<tool>`, including [deferred](mcp.md#deferred-tools) ones,
which need no `mcp_search`/`mcp_call` from a script. Not callable: `node`
(no scripts starting scripts), `ask` and `task` (they need the interface and
the agent loop), and `mcp_search`/`mcp_call`. In a subagent, a script can
call the tools that agent was given.

### Limits

- **50 tool calls per script.** The calls past that are not run and return
  `{ok: false}` with a message saying so.
- **Results are text.** An image a tool returns (`read` on a picture) is
  left out, with a note in `text`.
- **The timeout covers the script, not the tools** — see below.
- Each inner call is shown as a dim line under the `node` call
  (`⤷ read src/x.rs`); its result is not.

With the [Claude backend](claude.md), `tools: true` works the same way for
the tools atelier offers `claude` (the MCP servers there are connected by
`claude` itself, so their tools are not among them).

## Timeouts and interruption

A script gets `timeout_ms` (default 5000ms, hard ceiling 600000ms) of
wall-clock time on a dedicated worker thread. On timeout, atelier trips a
cooperative interrupt flag the interpreter checks on every loop back-edge, so
a runaway `while (true) {}` is actually halted (not just abandoned) and the
worker thread is reaped. A pathological script with no loop back-edge to
observe the flag is abandoned after a short grace period rather than
blocking the harness. The tool result for a timeout is a plain notice:
`node: script exceeded the <N>ms time limit and was aborted`.

With `tools: true` the budget is the script's own running time: the clock
runs while atelier waits for the script — to make its next tool call or to
finish — and is stopped while a call is being served, which is when the
user may be reading an approval prompt and the tool runs (under its own
timeout, if it has one). A script that makes one slow `bash` call is not
timed out by it; a script that spins between calls still is. Stopping the
turn (Ctrl-C) also stops such a script: no further call is run and the
result is `node: the script was stopped by the user`.

## Network (opt-in, requires approval)

By default there is no network access at all (`typeof fetch ===
"undefined"`). Passing `network: true` installs three synchronous globals
backed by the same HTTP client (`rsurl`) the provider uses, and **flips the
call to require user approval** — network is the one thing `node` can do
that isn't confined to the project.

| Function | Signature | Behavior |
|----------|-----------|----------|
| `fetch` | `(url[, {method, headers, body}]) -> {status, ok, body, headers}` | `options.method` defaults to `GET` |
| `httpRequest` | `({method, url, headers, body}) -> {status, ok, body, headers}` (also accepts a bare URL string) | Lower-level form `fetch` is built on |
| `httpGet` | `(url) -> string` | Shorthand for `httpRequest({url}).body` |

All three are synchronous — no promises, no `await`. `ok` is `true` for
2xx status codes.

```js
var res = fetch("https://api.example.com/data", {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify({ q: 1 }),
});
console.log(res.status, res.body);
```

```js
console.log(httpGet("https://example.com/robots.txt"));
```

## What's not there yet

Per the project roadmap: async `fs`, out-of-project `fs` access mediated by
approval (today it's a hard reject, not a prompt), and typed-array/binary
support for request/response bodies (everything is currently string-based).
