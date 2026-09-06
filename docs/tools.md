# Built-in tools

All tools live in `src/tools/` (registered in `builtin_registry`) plus the
`node` tool in `src/js/`. Every tool call's result string is fed back to the
model verbatim as the tool result.

| Tool    | Approval | Confined to project root |
|---------|----------|----------------------------|
| `read`  | auto     | yes |
| `write` | auto     | yes |
| `edit`  | auto     | yes |
| `grep`  | auto     | yes |
| `glob`  | auto     | yes |
| `ls`    | auto     | yes |
| `bash`  | **required** | no (arbitrary shell) |
| `node`  | auto, unless `network: true` (then required) | fs yes; network no |

"Confined" tools resolve every path with `ToolCtx::resolve` /
`tools::confine`: a relative path is joined against the project root, `.`/`..`
are normalized away, and anything that still doesn't start with the root is
rejected with an error (`path ".." escapes the project root`) — including
absolute paths outside it. See [Permissions](permissions.md) for the full
approval model and why confinement is the deciding factor, not "read vs.
write".

## `read`

Read a file, returned with 1-based `cat -n`-style line numbers (`"{line}\t{text}\n"`
per line).

| Param    | Type    | Required | Meaning |
|----------|---------|----------|---------|
| `path`   | string  | yes      | File path, relative to the project root (or absolute within it) |
| `offset` | integer | no       | 1-based line to start from (default 1) |
| `limit`  | integer | no       | Max lines to return (default: rest of file) |

Errors if the path is a directory or doesn't exist. **Every successful read
records the file's full content in `FileState`**, which `edit` later checks
for staleness.

## `write`

Create or overwrite a file with new content, creating parent directories as
needed.

| Param     | Type   | Required | Meaning |
|-----------|--------|----------|---------|
| `path`    | string | yes      | File path |
| `content` | string | yes      | Full contents to write |

Returns `"wrote N bytes to <path>"`. Also records the written content in
`FileState` (so a subsequent `edit` doesn't need a separate `read` first).

## `edit`

Exact-string replacement in a file that was read earlier in the session.

| Param          | Type    | Required | Meaning |
|----------------|---------|----------|---------|
| `path`         | string  | yes      | File path |
| `old_string`   | string  | yes      | Exact text to find |
| `new_string`   | string  | yes      | Replacement text |
| `replace_all`  | boolean | no       | Replace every occurrence instead of requiring exactly one (default false) |

**File-state rules** (via `FileState`, tracked by content hash per path):

- The file must have been `read` (or `write`) at least once this session, or
  `edit` fails with *"has not been read in this session yet"*.
- If the file's on-disk content no longer matches what was last observed
  (i.e. it changed outside the agent's control since the last read/write),
  `edit` fails with *"has changed on disk since it was last read"* rather
  than silently clobbering it.
- `old_string` must match **exactly once** unless `replace_all: true`; zero
  matches or (without `replace_all`) more than one match is an error naming
  the count found.

A successful edit re-records the new content, so consecutive edits to the
same file don't need re-reading in between.

## `bash`

Run a shell command via `sh -c` in the project root, capturing combined
stdout/stderr and the exit status.

| Param        | Type    | Required | Meaning |
|--------------|---------|----------|---------|
| `command`    | string  | yes      | The shell command |
| `timeout_ms` | integer | no       | Max run time in ms (default **120000**) |

The child is polled every 20ms and killed if it exceeds the timeout (output
then reads `[command timed out after Nms and was killed]`). Output is capped
at 200 lines and 20,000 bytes combined, with `[output truncated]` appended if
either limit is hit. **`bash` is the one built-in tool that is not confined**
— it can run anything — so it always requires approval (see
[Permissions](permissions.md), including the risk-signal warnings shown
alongside the prompt).

## `grep`

Regex search over file contents, respecting `.gitignore` (via the `ignore`
crate's directory walker).

| Param     | Type   | Required | Meaning |
|-----------|--------|----------|---------|
| `pattern` | string | yes      | Regular expression (Rust `regex` syntax) |
| `path`    | string | no       | Directory to search under (default: project root) |
| `glob`    | string | no       | Filename glob filter, e.g. `*.rs` |

Matches are returned as `path:line:text`, one per line, capped at 100 with a
`[truncated at 100 matches]` note. Binary/unreadable files are silently
skipped. Returns `"no matches found"` if nothing hits.

## `glob`

List files matching a glob pattern, respecting `.gitignore`.

| Param     | Type   | Required | Meaning |
|-----------|--------|----------|---------|
| `pattern` | string | yes      | Glob, e.g. `src/**/*.rs` |
| `path`    | string | no       | Directory to search under (default: project root) |

Returns sorted, project-relative paths, one per line, capped at 200 with a
`[truncated at 200 results]` note, or `"no files matched"`.

## `ls`

List one directory's entries (not recursive).

| Param  | Type   | Required | Meaning |
|--------|--------|----------|---------|
| `path` | string | no       | Directory to list (default: project root) |

Entries are sorted; directories get a trailing `/`. Errors if the path
doesn't exist or isn't a directory. Returns `"(empty directory)"` if empty.

## `node`

Runs a JavaScript snippet on an embedded interpreter with a mediated,
project-confined `fs` and an optional network capability. See
[Scripting](scripting.md) for the full reference — it's substantial enough to
warrant its own page.
