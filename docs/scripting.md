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
| `timeout_ms` | integer | no       | Wall-clock budget in ms (default **5000**, clamped to at most 600000) |

The tool result is the script's captured `console` output; if nothing was
logged, it falls back to the display form of the script's final expression
value (or is empty for `undefined`). Output is truncated at 10KB. A thrown
error is appended as `Uncaught <message>` (a syntax error is reported the
same way, without running anything).

## No modules

There is **no `require`/`import`** — CommonJS and ES modules are
unavailable. `fs` and `console` are plain globals already in scope; just use
them directly.

## `fs` — synchronous, project-confined

All five methods are synchronous (no promises/callbacks/await) and every
path argument is resolved against the project root the same way built-in
file tools are (`tools::confine`): relative paths are joined to the root,
`.`/`..` are normalized, and anything that still doesn't stay under the root
throws a catchable JS error instead of touching the filesystem.

| Method | Signature | Behavior |
|--------|-----------|----------|
| `fs.readFile` | `(path) -> string` | Reads a UTF-8 text file |
| `fs.writeFile` | `(path, content)` | Writes (creating parent directories as needed); no return value |
| `fs.readdir` | `(path) -> string[]` | Directory entry names |
| `fs.exists` | `(path) -> boolean` | Whether the path exists |
| `fs.mkdir` | `(path)` | Creates the directory (and parents); no return value |

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

## Timeouts and interruption

A script gets `timeout_ms` (default 5000ms, hard ceiling 600000ms) of
wall-clock time on a dedicated worker thread. On timeout, atelier trips a
cooperative interrupt flag the interpreter checks on every loop back-edge, so
a runaway `while (true) {}` is actually halted (not just abandoned) and the
worker thread is reaped. A pathological script with no loop back-edge to
observe the flag is abandoned after a short grace period rather than
blocking the harness. The tool result for a timeout is a plain notice:
`node: script exceeded the <N>ms time limit and was aborted`.

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
