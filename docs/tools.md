# Built-in tools

Most tools live in `src/tools/` and are registered in `builtin_registry`;
the `node` tool is in `src/js/`, `task` is added at startup (not with the
[Claude backend](claude.md), where claude has its own), and `peers` /
`peer_send` come with the [coordinator](coordinator.md). The tools of MCP
servers, and `mcp_search` / `mcp_call` that stand in for them when there are
many, are in [MCP](mcp.md). Every tool call's result string is fed back to
the model verbatim as the tool result.

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
| `ask`       | auto (it only talks to you) | n/a |
| `memory`    | auto     | to the memory directories (the project's `.atelier/memory/` and the user-global one) |
| `skill`     | auto     | to the skill directories (read-only; a skill's files only from inside that skill's directory) |
| `task`      | auto to start; each call the subagent makes is approved like the agent's own | as the tools the subagent uses |
| `notebook_edit` | auto | yes |
| `bash`      | **required** — except a foreground command the experimental [sandbox](sandbox.md) runs, when that is on | no (arbitrary shell), unless sandboxed |
| `bash_output`, `bash_kill` | auto (act on jobs `bash` started) | n/a |
| `web_fetch` | **required** | no (network) |
| `web_search` | **required** | no (network) |
| `node`      | auto, unless `network: true` (then required); with `tools: true`, each tool the script calls is approved as that tool | fs yes; network no |
| `peers`     | auto     | n/a (lists your atelier instances) |
| `peer_send` | **required** | no (reaches other instances) |
| `peer_ask`  | **required** | no (sets another instance's agent to work) |
| `peer_answer` | auto (answers a question already admitted) | n/a |

"Confined" tools resolve every path with `ToolCtx::resolve` /
`tools::confine`: a relative path is joined against the project root, `.`/`..`
are normalized away, and anything that still doesn't start with the root is
rejected with an error (`path ".." escapes the project root`) — including
absolute paths outside it. See [Permissions](permissions.md) for the full
approval model and why confinement is the deciding factor, not "read vs.
write".

## Read-only calls run in parallel

When the model asks for several tool calls in one step, the ones that only
look run at the same time (`src/agent/batch.rs`): a run of consecutive calls
to `read`, `grep`, `glob`, `ls`, `tree`, `web_fetch`, `web_search`, `skill`,
`memory` (`read` and `list` only) and `bash_output`, up to 8 at once. Any
other call — one that writes, `bash`, `ask`, `task`, `todo`, an MCP tool —
ends the run and is executed by itself, in its place: `[read a, grep x,
write b, read c]` is `read a` and `grep x` together, then `write b`, then
`read c`.

What does not change:

- **Approval and hooks come first, in order.** Each call of a batch gets its
  `pre_tool` hooks and its approval (rules, the mode, a prompt if needed) one
  after the other before any of them runs. A refused call gets its refusal
  as its result; the others still run. `post_tool` hooks run in call order
  once the batch is done.
- **The model sees one result per call, in the order of its calls**,
  whichever finished first.
- **Read tracking**: a file read in a batch counts as read for a later
  `edit`, and a subdirectory's own instruction file still arrives exactly
  once — with the first call, in call order, that went there.

On screen, the calls of a batch are announced together (`⚙ read …`,
`⚙ grep …`) and their results follow in the same order. Stopping the turn
(Ctrl-C) lets the calls that are running finish and skips the ones that have
not started.

`[defaults] parallel_tools = false` in `atelier.toml`, or
`ATELIER_PARALLEL_TOOLS=0`, runs every call one at a time. The Claude backend
is not concerned: claude asks for atelier's tools one call at a time.

## Long output goes to a file

Tools whose output can be any size — `bash`, `bash_output`, `node`, MCP tools
and `read_resource` — don't cut it off at a limit the agent can't see past.
When a result is over the tool's budget (20,000 bytes for `bash` and
`bash_output`, 10 KiB for `node`, 30,000 bytes for MCP), the whole text is
saved under `.atelier/tool-output/` in the project and the result is its
start (about 60% of the budget) and its end (about 25%), cut on line
boundaries, around one line saying what is missing and where it is:

```
… 1,842 lines omitted (lines 121-1962) — full output: .atelier/tool-output/ab12cd.txt (use read with offset/limit, or grep) …
```

The file is in the project, so `read` (`offset`/`limit`) and `grep` reach it
without approval. Output with no line breaks is cut by bytes instead, and the
marker counts bytes.

The directory keeps the 50 most recent files and at most 20 MiB; older ones
are deleted when a new one is written (the new one always stays). Searches
and listings of the project — `grep`, `glob`, `tree`, `fs.glob` in `node`,
the layout context — leave `.atelier/tool-output/` out, whether or not the
project git-ignores `.atelier/`, unless the directory or a file in it is
named as the `path`. The same goes for `.atelier/worktrees/`, where
[isolated subagents](agents.md#worktree-isolation) work in copies of the
repository.

`read`, `grep`, `glob` and `web_fetch` page or cap by themselves and are not
affected.

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
- **PDFs** come back as their text, page by page (`[page N]`), extracted in
  pure Rust by `oxideav-pdf`: lines in the order the page draws them,
  ligatures spelled out. Scanned PDFs without a text layer have nothing to
  extract.
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
| `background` | boolean | no       | Start as a background job (also `run_in_background`) |
| `sandbox`    | boolean | no       | Only with the [sandbox](sandbox.md) on: `false` runs this command on the host instead, which needs approval |

The child is polled every 20ms and killed if it exceeds the timeout (output
then reads `[command timed out after Nms and was killed]`). Output over
20,000 bytes comes back as its start and its end, with all of it saved to a
file ([below](#long-output-goes-to-a-file)). **`bash` is the one built-in tool that is not confined**
— it can run anything — so it requires approval (see
[Permissions](permissions.md), including the risk-signal warnings shown
alongside the prompt), unless the [sandbox](sandbox.md) will run the
command.

With `background: true` (or `run_in_background: true`) the command starts as
a **background job** instead and the call returns its id (`job-N`) at once —
for servers, watchers, and long builds. Jobs run in their own process group
and are stopped when atelier exits.

**A job that ends says so**; the agent does not have to poll it.

- The model gets a notice at its next step — between two steps of the
  running turn, the way a message you type mid-turn is delivered — as a
  marked block, followed by a line saying that it comes from the harness and
  not from you:

  ```
  <background-job id="job-3" exit="0" runtime="42s">
  command: cargo test
  last 10 of 214 lines of output:
  …
  </background-job>
  ```

  It carries the exit code (or the signal), the runtime, the command (its
  first line, clipped) and the last 10 lines of output; `bash_output` with
  the job's id returns everything not read yet.
- You see one line: `⚙ job 3 finished (exit 0, 42s): cargo test`.
- If the session is idle when the job ends, the line appears at the prompt
  and the notice goes to the model **with your next prompt**. A finished job
  never starts a turn by itself.
- No notice for a job the model stopped with `bash_kill`, nor for one whose
  exit it already saw in a `bash_output` result.
- A job started by a [subagent](agents.md) is announced to the main
  conversation: the subagent may be gone by the time it ends.

With the [sandbox](sandbox.md) on (`[sandbox] enabled = true`, a build with
the `sandbox` feature), a foreground command runs in a nixvm guest that sees
the project and nothing else of the machine, and is not put to the user. A
result whose failure looks like the sandbox's doing says so, and the model
can ask for the host with `"sandbox": false`. Background jobs always run on
the host.

## `bash_output`

Read a background job's output since the last call, and its status — or
wait on it.

| Param     | Type    | Required | Meaning |
|-----------|---------|----------|---------|
| `id`      | string  | no       | The job (`job-N`). Omitted: with `wait_ms`/`until`, the latest job; otherwise list all jobs |
| `wait_ms` | integer | no       | Block up to this long (max 600000) until the job exits… |
| `until`   | string  | no       | …or its new output matches this regex |

Returns `[job-N: running|exit status: … after Ns]` then the new output (its
start and its end if over 20,000 bytes, [the rest in a
file](#long-output-goes-to-a-file)). This is how to wait for a server to print
"listening", or to block on a build when there is nothing else to do. Just
learning that a job has ended needs no call: [it says so](#bash).

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
project-confined `fs` and an optional network capability. With `tools: true`
the script can also call the other tools (`tools.call("read", {path})`) and
work on their results, so that only what it prints reaches the model — "code
mode", for many calls whose intermediate results the model need not read.
Each such call goes through the hooks, permission rules and approval of the
tool it names. See [Scripting](scripting.md) for the full reference — it's
substantial enough to warrant its own page.

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

## `web_search`

Search the web and return a numbered list of results. **Requires approval**
(network access, unconfined — the query leaves the machine).

| Param         | Type     | Required | Meaning |
|---------------|----------|----------|---------|
| `query`       | string   | yes      | What to search for |
| `max_results` | integer  | no       | How many results (default 8, at most 20) |
| `domains`     | string[] | no       | Only return results from these domains, subdomains included (e.g. `["docs.rs"]`) |

Each result is its title, its URL and a snippet of at most 300 characters:

```
1. References and Borrowing - The Rust Programming Language
   https://doc.rust-lang.org/book/ch04-02-references-and-borrowing.html
   The rules of references: at any given time, you can have either one…
```

Nothing found is `no results for "<query>"`. Read a result with `web_fetch`.

The search goes to the provider set by `ATELIER_SEARCH_PROVIDER` or the
`[search]` table of `atelier.toml` — `searxng`, `brave`, `tavily`, or
`duckduckgo` — see [Configuration](configuration.md#search). The default,
with nothing configured, is DuckDuckGo's HTML results page, which needs no
key but may refuse automated clients; when the page it returns isn't a
results page the call fails with an error saying to configure a provider,
rather than returning made-up or empty results. A failure reads
`<provider> search failed: <reason>` on one line (host unreachable, HTTP
status, unexpected response); the API key is never part of it.

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

## `ask`

Put a question to the user, with a few options to choose from, and wait for
the answer — instead of guessing, or ending the turn with a question in
prose.

| Param          | Type    | Required | Meaning |
|----------------|---------|----------|---------|
| `question`     | string  | yes      | The question |
| `options`      | array   | yes      | 2 to 6 choices, each `{ "label": "…", "description": "…" }` (`description` optional; a bare string is taken as a label). Labels must be distinct |
| `multi_select` | boolean | no       | Several options may be chosen (default `false`) |
| `allow_other`  | boolean | no       | The user may type an answer of their own instead (default `true`) |

Nothing pops up. The question is printed like any other output and answered
on the input line:

```
? Which database should the cache use?
  1. SQLite — one file, no server
  2. Postgres — already runs in production
  3. Plain files
  (answer with a number, or type your own answer)
› 2
```

While it waits, the status strip reads `? waiting for your answer` and the
line you submit is the answer rather than a queued message. What you can
type:

- a number (`2`), or for a multi-select question several: `1 3`, `1,3`, a
  range `2-4`;
- a label, or the start of one if no other label starts the same way
  (`post`, or `sqlite, plain` for several) — case doesn't matter;
- anything else is your own answer where `allow_other` is on — including a
  number past the last option. Where it is off, an answer that isn't an
  option prints the hint again and the question keeps waiting.

Ctrl-C on an empty line dismisses the question: the turn goes on and the
model is told you declined to answer.

The result says which it was, with labels and text quoted, so a typed
answer is never taken for an option: `The user chose the option
"Postgres".`, `The user chose 2 options: "SQLite", "Plain files".`, or `The
user chose none of the options and answered in their own words: "…"`.

Where there is no one to ask — `--print`, except with `stream-json` both
ways, where the parent process answers
([Questions](headless.md#questions)) — the call fails with `cannot ask:
this is a non-interactive session…` and the model is told to decide for
itself and state its assumption. Auto-approved: it only talks to you.

## `memory`

Notes that persist across sessions, one Markdown file each, in
`.atelier/memory/` (project) or the user-global memory directory. The index
of saved memories is injected as context every turn; this tool reads the
notes and maintains them. See [memory](memory.md) for the file format and
what the agent is told to save.

| Param         | Type   | Required | Meaning |
|---------------|--------|----------|---------|
| `action`      | string | yes      | `save` · `read` · `list` · `delete` |
| `name`        | string | for save/read/delete | kebab-case slug (lower-case letters, digits, single hyphens; max 64 chars) |
| `description` | string | for save | one line (max 200 chars), shown in the index |
| `type`        | string | for save | `user` · `feedback` · `project` · `reference` |
| `content`     | string | for save | the note, Markdown, max 8 KiB |
| `scope`       | string | no       | `project` (default for save) · `global` (type `user` only). For read/delete the default is whichever has the name, project first |

`save` overwrites a memory of the same name and regenerates `MEMORY.md`.
Errors the model can act on: an invalid name, a note over 8 KiB, a full
directory (200 memories), and content that looks like a secret (private-key
blocks, `sk-…`/`ghp_…`/AWS-style keys, `password=…` and the like) — the save
is refused. Confined to the memory directories and auto-approved.

## `skill`

Load a [skill](skills.md): instructions (often with scripts and reference
files) for one kind of task. The skills' names and descriptions are injected
as context every turn; this tool returns the rest when a task calls for it.

| Param  | Type   | Required | Meaning |
|--------|--------|----------|---------|
| `name` | string | no       | the skill to load, as listed. Omitted: list every skill (`- name — description`) |
| `file` | string | no       | with `name`: return this file of the skill instead, by its path relative to the skill's directory |

Loading a skill returns:

```
skill: release
directory: /path/to/project/.atelier/skills/release
files (read one with {"name": "release", "file": <path>}; run scripts by their path under the directory):
  checklist.md
  scripts/bump-version.sh

# Releasing
…the body of SKILL.md, without its front matter…
```

Up to 100 files are listed (hidden files left out, four levels deep). `file`
returns a text file of at most 256 KiB, and only from inside the skill's own
directory: the path is resolved first, so `..`, an absolute path, or a
symlink leading out of the skill is an error. That confinement is what lets
the tool be auto-approved even though a user-global skill is outside the
project, where `read` would ask. An unknown name is an error that lists the
available ones.

## `task`

Run a [subagent](agents.md) on a task and return its final report. The
subagent is a fresh conversation: it sees nothing of the caller's, only
`prompt`, so the prompt must be complete in itself. The description the
model is given lists the agents available (built-in and file-defined) with
their descriptions.

| Param         | Type   | Required | Meaning |
|---------------|--------|----------|---------|
| `agent`       | string | yes      | Which agent runs it: `explore`, `general`, or one defined in `.atelier/agents/` |
| `prompt`      | string | yes      | The complete task — goal, known facts and paths, what to report |
| `description` | string | no       | The task in 3–6 words, shown while it runs (default: the prompt's first line) |
| `background`  | boolean | no      | Start the agent and return its id at once instead of waiting for its report (default false; `run_in_background` is accepted too) |
| `isolation`   | string | no       | `"worktree"`: the agent works in a fresh git worktree at HEAD instead of the project directory. Uncommitted changes are not carried over. An agent whose definition says `isolation: worktree` is isolated whatever the call says |

The result is the subagent's final message followed by a footer,
`[explore: 7 steps · 412 output tokens]`. A subagent that reaches its step
cap is made to wrap up without tools, and its report says so. An unknown
agent is an error that lists the available ones; a subagent that fails (the
endpoint errored) is an error result with the cause.

With `background: true` the result is the agent's id (`started background
agent agent-1 (explore). …`), and the report arrives later as a
`<subagent id="agent-1" …>` block — between two steps of a running turn, or
in front of your next prompt — exactly as a finished background job's notice
does. A background agent has no user to ask, so a call the permission rules
would ask about is denied to it; see [in the
background](agents.md#in-the-background) for that, `/agents stop`, and the
limit of 4 running at once.

With `isolation: "worktree"` the agent's project root is its own [git
worktree](agents.md#worktree-isolation) under `.atelier/worktrees/<id>/`. If
it changed anything, its report ends with the worktree's path, a `git diff
--stat` against the base commit and the command that brings the changes over
(`git -C <worktree> diff --binary <base> | git apply`); nothing is merged
automatically. An untouched worktree is removed. Outside a git repository,
or in one without a commit, the call is an error result.

`task` calls that follow one another in one step run at the same time; their
results come back in call order. Subagents have every limit the caller has —
the same project root, the same permission rules, the same approval prompts
— and never `task` itself. The tool is run by the agent loop rather than by
`Tool::call`, and is not offered to the [Claude backend](claude.md), which
has subagents of its own.

## `peers`, `peer_send`

Present when the instance is connected to the [coordinator](coordinator.md).

`peers` lists the atelier instances running on this machine (name, idle or
working, model, directory, topics), after applying any subscription changes.

| Param         | Type            | Required | Meaning |
|---------------|-----------------|----------|---------|
| `subscribe`   | array of string | no       | Topics to start receiving |
| `unsubscribe` | array of string | no       | Topics to stop receiving |

`peer_send` sends a message to other instances and reports how many it
reached. The recipient's agent gets it at its next step; an idle instance
only reads it with its user's next prompt.

| Param     | Type   | Required | Meaning |
|-----------|--------|----------|---------|
| `to`      | string | yes      | An instance name, `*` for every other instance, or `#topic` |
| `message` | string | yes      | The text to deliver |

## `peer_ask`, `peer_answer`

A question to another instance's agent, and its answer — see
[Asking another agent](coordinator.md#asking-another-agent).

`peer_ask` sends the question and blocks until the other agent answers; the
answer is the result. With no answer the result is `no answer: <why>` (the
other agent's turn ended without one, the timeout ran out, the instance
left), which is not an error. It counts as a modifying tool — the other
agent may act on what it is asked — so plan mode refuses it.

| Param          | Type    | Required | Meaning |
|----------------|---------|----------|---------|
| `to`           | string  | yes      | The instance to ask (a name from `peers`) |
| `question`     | string  | yes      | What to ask |
| `timeout_secs` | integer | no       | How long to wait (default 300, at most 1800) |

`peer_answer` answers a question that reached this agent as
`<peer-question id="…" from="…">`. It must be called in the turn the
question arrives in: a question still open when that turn ends is closed as
unanswered.

| Param    | Type   | Required | Meaning |
|----------|--------|----------|---------|
| `id`     | string | yes      | The question's id |
| `answer` | string | yes      | The answer |
