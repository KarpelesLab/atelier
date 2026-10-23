# Built-in tools

All tools live in `src/tools/` (registered in `builtin_registry`) plus the
`node` tool in `src/js/`. Every tool call's result string is fed back to the
model verbatim as the tool result.

| Tool    | Approval | Confined to project root |
|---------|----------|----------------------------|
| `read`      | auto; **required** outside the project | yes, unless approved |
| `write`     | auto     | yes |
| `edit`      | auto     | yes |
| `multiedit` | auto     | yes |
| `apply_patch` | auto   | yes |
| `grep`      | auto     | yes |
| `glob`      | auto     | yes |
| `ls`        | auto     | yes |
| `tree`      | auto     | yes |
| `todo`      | auto     | yes |
| `notebook_edit` | auto | yes |
| `bash`      | **required** | no (arbitrary shell) |
| `bash_output`, `bash_kill` | auto (act on jobs `bash` started) | n/a |
| `web_fetch` | **required** | no (network) |
| `node`      | auto, unless `network: true` (then required) | fs yes; network no |

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
| `path`   | string  | yes      | File path, relative to the project root (or absolute) |
| `offset` | integer | no       | 1-based line to start from (default 1) |
| `limit`  | integer | no       | Max lines to return (default: rest of file) |
| `pages`  | string  | no       | PDF only: page range, e.g. `"3"` or `"1-5"` |

By file type:

- **Images** (`png`, `jpg`/`jpeg`, `gif`, `webp`, up to ~3.75MB) come back as
  images the model sees (an MCP image result for claude; for an
  OpenAI-compatible endpoint, a follow-up user message carrying them).
- **PDFs** come back as their text, via poppler's `pdftotext` (an error says
  so if it isn't installed); page breaks are marked.
- **Notebooks** (`.ipynb`) come back rendered: each cell as
  `<cell N id="…" type=…>` with its source and outputs.
- Anything else is read as UTF-8 text.

**Outside the project**, `read` asks for approval first (a pasted screenshot
path, a temp file another tool wrote); inside it, it never does.

Errors if the path is a directory or doesn't exist. **Every successful text
or notebook read records the file's full content in `FileState`**, which
`edit`/`notebook_edit` later check for staleness. Like the other file tools,
`read` also accepts `file_path` for `path`.

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

## `multiedit`

Apply several exact-string edits to one file in a single call — the same
rules as `edit`, but batched, so a file needing multiple changes doesn't cost
one round-trip per change.

| Param   | Type    | Required | Meaning |
|---------|---------|----------|---------|
| `path`  | string  | yes      | File path |
| `edits` | array   | yes      | The edits to apply, in order (at least one); each is `{old_string, new_string, replace_all?}` — same fields as `edit`'s |

Same file-state rules as `edit` (must have been read this session; rejected
if the file changed on disk since). Edits are applied **in order** to an
in-memory copy, each seeing the result of the ones before it — so a later
edit's `old_string` may only exist because an earlier edit in the same call
produced it. The whole call is **atomic**: if any edit in the sequence fails
(its `old_string` is missing, or appears more than once without
`replace_all`), none of the edits are written and the file is left
untouched. Returns `"applied N edit(s) to <path>"` and re-records the final
content, same as `edit`.

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

With `background: true` (or `run_in_background: true`) the command starts as
a **background job** instead and the call returns its id (`job-N`) at once —
for servers, watchers, and long builds. Jobs run in their own process group
and are stopped when atelier exits.

## `bash_output`

Read a background job's output since the last call, and its status — or
wait on it.

| Param     | Type    | Required | Meaning |
|-----------|---------|----------|---------|
| `id`      | string  | no       | The job (`job-N`). Omitted: with `wait_ms`/`until`, the latest job; otherwise list all jobs |
| `wait_ms` | integer | no       | Block up to this long (max 600000) until the job exits… |
| `until`   | string  | no       | …or its new output matches this regex |

Returns `[job-N: running|exit status: … after Ns]` then the new output (its
last 20,000 bytes if longer). This is how to wait for a build to finish or a
server to print "listening".

## `bash_kill`

Stop a background job and everything it spawned (`TERM` to its process
group, then `KILL` if it lingers). Param: `id`.

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

## `tree`

Print an indented directory tree, respecting `.gitignore`.

| Param   | Type    | Required | Meaning |
|---------|---------|----------|---------|
| `path`  | string  | no       | Root to print (default: project root) |
| `depth` | integer | no       | Max depth (default 3) |

Directories get a trailing `/`; capped at 200 entries with a truncation note.
Confined to the project root and auto-approved.

## `web_fetch`

Fetch a URL over HTTP(S) and return the response body as text. **Requires
approval** (network access, unconfined).

| Param    | Type   | Required | Meaning |
|----------|--------|----------|---------|
| `url`    | string | yes      | `http(s)://` URL (redirects followed) |
| `method` | string | no       | HTTP method (default `GET`) |

| `offset` | integer | no | Character offset to start from (default 0) |
| `max_chars` | integer | no | Characters to return (default 30000) |
| `raw` | boolean | no | Return HTML as-is instead of converting it |

Returns a `HTTP <status> <reason>` line then the body. **HTML is converted to
readable text** — scripts, styles and markup dropped, headings as `#` lines,
list items as `- `, links as `[text](url)`, entities decoded. A longer result
says `[characters A-B of N]` and ends with how to fetch the next page.

## `notebook_edit`

Edit a Jupyter notebook cell. The notebook must have been `read` this session
and be unchanged on disk since (like `edit`).

| Param        | Type   | Required | Meaning |
|--------------|--------|----------|---------|
| `path`       | string | yes      | The `.ipynb` file |
| `cell`       | string | for replace/delete | Cell id, or 0-based index (`cell_id` also accepted) |
| `new_source` | string | for replace/insert | The cell's new source |
| `cell_type`  | string | no       | `code` or `markdown` (insert default `code`) |
| `edit_mode`  | string | no       | `replace` (default), `insert` (after `cell`, or at the top), or `delete` |

Replacing a code cell's source clears its stale outputs and execution count.
The file is written as Jupyter writes it (sorted keys, one-space indent).

## `apply_patch`

Apply a **unified diff** spanning one or more files in a single atomic call —
the natural way to express a multi-file change.

| Param   | Type   | Required | Meaning |
|---------|--------|----------|---------|
| `patch` | string | yes      | A unified diff (`--- a/… / +++ b/…` headers, `@@` hunks) |

Supports modify, create (`--- /dev/null`), and delete (`+++ /dev/null`).
Hunks are located by the `@@` line hint, falling back to a unique content
search, so slightly-off line numbers still apply. Modified files must have
been read this session and not be stale (same `FileState` rules as `edit`).
**Atomic**: if any hunk of any file fails, nothing is written. Confined and
auto-approved.

## `todo`

A durable checklist stored at `.atelier/todos.json`, for tracking multi-step
work across turns (and across compaction).

| Param    | Type   | Required | Meaning |
|----------|--------|----------|---------|
| `action` | string | yes      | `list` · `add` · `complete` · `set` · `clear` |
| `items`  | array  | for add/set | task strings |
| `ids`    | array  | for complete | 1-based ids to mark done |

Renders as `[x] 1. done` / `[ ] 2. pending` with a count summary. Confined and
auto-approved.
