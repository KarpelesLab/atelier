# GitHub Action

The repository is also a GitHub Action: `uses: KarpelesLab/atelier@v0` runs
atelier in a workflow. It is a thin layer over the headless interface
([`--print` and `atelier review`](headless.md)) with three modes:

| `mode` | What it does |
|--------|--------------|
| `review` (default) | Runs [`atelier review`](headless.md#atelier-review) on a pull request and posts the findings as **one** review with inline comments |
| `prompt` | Runs a prompt you give (`atelier --print`); what it changes stays in the workspace for the next steps to commit |
| `comment` | Answers a comment that starts with `@atelier` on an issue or a pull request — from people with write access only |

It is a composite action: `action.yml` at the repository root and bash
scripts in `.github/action/`. No Node, no Docker image. It needs `bash`,
`curl`, `jq`, `gh`, `git`, `sha256sum` and `gunzip` on the runner (GitHub's
Linux runners have them all) and runs on **Linux x86_64 and arm64** — the
platforms atelier publishes binaries for; elsewhere the install step stops
with a message.

> **Status.** The scripts are tested against fixture events, a stub `gh` and
> a stub `atelier` (`tests/action.rs`), and checked with `shellcheck` in CI.
> They have **not yet been run on a real runner against the real GitHub
> API**; see [what is not verified](#what-is-not-verified-yet).

## Quick start: review pull requests

```yaml
# .github/workflows/atelier-review.yml
name: atelier review
on:
  pull_request:

permissions:
  contents: read          # check out the code
  pull-requests: write    # post the review

jobs:
  review:
    runs-on: ubuntu-latest
    # A pull request from a fork gets no secrets and a read-only token.
    if: github.event.pull_request.head.repo.full_name == github.repository
    steps:
      - uses: actions/checkout@v4
        with:
          ref: ${{ github.event.pull_request.head.sha }}
          fetch-depth: 0
          persist-credentials: false
      - uses: KarpelesLab/atelier@v0
        with:
          base-url: https://llm.example.com/v1
          model: my-coder
          api-key: ${{ secrets.LLM_API_KEY }}
          fail-on: P1
```

Three things in that checkout step matter:

- **`ref: …head.sha`** checks out the pull request's own head. By default
  `actions/checkout` builds a merge commit of the head and the base; its line
  numbers are not always the head's, and a review comment is attached to a
  line of the head. The action copes with the default (findings in files
  that differ between the two go in the review's body instead of inline),
  but checking out the head makes every finding land where it belongs.
- **`fetch-depth: 0`** fetches the history, which `atelier review --base`
  needs to find where the branch left its base. Without it the action
  fetches the base branch and un-shallows the checkout itself, which works
  but is slower than asking for it up front.
- **`persist-credentials: false`** keeps the job's token out of
  `.git/config`, where a file-reading tool could find it. Use it wherever
  the workflow does not push.

## Inputs

| Input | Default | Meaning |
|-------|---------|---------|
| `mode` | `review` | `review`, `prompt` or `comment` |
| `prompt` | | `prompt` mode: the prompt |
| `base-url` | | OpenAI-compatible endpoint base (`ATELIER_BASE_URL`). In practice required: atelier's built-in default is a private address |
| `model` | | Model (`ATELIER_MODEL`) |
| `api-key` | | Bearer token for the endpoint (`ATELIER_API_KEY`). Pass a secret; leave empty when the endpoint needs none |
| `version` | `latest` | Release tag to install (`v0.9.0`), or `latest` |
| `fail-on` | | `review` mode: fail the step when a finding is at least this severe (`P0`…`P3`). Empty: findings never fail the step |
| `max-turns` | `30` | Stop after this many model requests (exit status 3) |
| `post` | `comments` on a `pull_request` event, else `summary` | `review` mode: `comments` (a pull request review), `summary` (the step summary) or `none` |
| `trust-project` | `false` | `true` accepts the gated settings of the checkout's `atelier.toml` ([Project trust](trust.md)). See [Safety](#safety) |
| `permission-mode` | `plan` for `review`; `auto-edit` for `prompt` and `comment` | atelier's [mode](permissions.md#modes): `plan`, `ask-edits`, `auto-edit`, `yolo` |
| `working-directory` | | Directory to run in, relative to the workspace |
| `extra-args` | | More arguments for atelier, split on whitespace (no quoting): `--commit HEAD`, `--goal tests-pass` |
| `github-token` | `${{ github.token }}` | Reads the pull request, posts reviews and replies. Never given to atelier |
| `trigger` | `@atelier` | `comment` mode: the phrase a comment must start with |
| `allow-forks` | `false` | `comment` mode: run on pull requests from forks. See [Safety](#safety) |

## Outputs

| Output | Meaning |
|--------|---------|
| `result` | The final answer; in `review` mode, the findings as Markdown. Cut to **60 000 characters** (a comment's body may not exceed 65 536, and the same text is used for both) |
| `findings` | `review` mode: the findings as a JSON array of `{file, line, severity, summary, scenario}`, as [`atelier review`](headless.md#atelier-review) printed them (`[]` when the review produced no report) |
| `exit-code` | atelier's [exit status](headless.md#exit-status): 0 success, 1 failure, 3 `max-turns`, 4 an answer of the wrong shape, 5 a finding at or above `fail-on` |
| `triggered` | `comment` mode: `true` when a comment was acted on, `false` when it was ignored or refused |
| `head-ref` | `comment` mode on a pull request: the head branch's name. **Untrusted text**: give it to later steps through `env:`, never inside a `run:` script |

The step ends with atelier's exit status, so a non-zero `exit-code` fails
the step; use `continue-on-error: true` and the output to decide yourself.

## How each mode works

### Install

Every run starts by installing the binary: the release asset for the
runner's architecture (`atelier-x86_64-unknown-linux-fullrust.gz` or
`atelier-aarch64-unknown-linux-musl.gz`) and the release's `SHA256SUMS` are
downloaded, the asset is checked against the list, and only then
decompressed and made executable. A mismatch, an asset the list does not
name, or a list that cannot be fetched stops the step; nothing unverified is
installed. No script is piped into a shell.

The binary is kept in `$RUNNER_TOOL_CACHE/atelier/<version>/<target>/` and
reused when it is there — across jobs on a self-hosted runner; a
GitHub-hosted runner starts empty, so there it is one small download per
job. The installed binary's self-update is turned off (`ATELIER_NO_UPDATE`):
the version you pinned is the version that runs.

The checksum list comes from the same release as the binary, so it protects
against a corrupted or truncated download, not against a release that was
itself replaced. Pin `version` if that matters to you.

### `review`

On a `pull_request` event the action runs

```
atelier review --base origin/<base branch> --output-format json --max-turns N [--fail-on P]
```

in `plan` mode, then turns the findings into one pull request review:

- a finding on a line the pull request's diff shows (added, or context of a
  hunk) becomes an **inline comment** on that line, starting with its
  severity (`**P1** …`) and followed by the failure scenario;
- every other finding — a line outside the diff, a file the pull request
  does not touch, a file whose content at the checkout is not the head's —
  is listed in the **review's body**, so none is lost;
- if GitHub refuses the review because of an inline comment (its idea of
  the diff can differ from git's), it is posted again with every finding in
  the body.

The review is of type `COMMENT`: it never approves or requests changes.
Whether the job fails is `fail-on`'s decision alone — atelier exits 5, the
review is still posted, and the step fails.

**Re-runs replace, they do not pile up.** Everything the action posts ends
with a hidden marker (`<!-- atelier-review -->`). On the next push, the new
review is posted first; then, for each earlier review carrying the marker,
its inline comments are deleted and its body becomes a one-line "superseded"
note. (GitHub's API can neither delete nor dismiss a submitted `COMMENT`
review; emptying it is what is possible.) A run with no findings posts
nothing new and retires the earlier review the same way. Comments people
wrote, including replies in the action's threads, are never touched: only
comments carrying the marker are deleted.

With `post: summary` the same findings are written to the job's step summary
and nothing is posted; `post: none` only sets the outputs. If the review
cannot be posted (no `pull-requests: write`, a read-only token), the action
warns and falls back to the summary.

On other events (`push`, `workflow_dispatch`) the action runs `atelier
review` with its defaults and writes the summary; say what to review with
`extra-args`, e.g. `--commit HEAD`.

### `prompt`

Runs `atelier --print --output-format json --max-turns N` with the prompt on
stdin, in `auto-edit` mode unless you chose another. The answer becomes the
`result` output and the step summary.

Files atelier edits are **left in the workspace**. The action does not
commit, push or open pull requests: the next steps of your workflow do, with
the permissions you gave them (see the [scheduled example](#a-scheduled-prompt-that-opens-a-pull-request)).

In a headless run nobody can answer an approval prompt, so in `auto-edit`
mode file edits go through and anything that needs approval — `bash` above
all — is denied ([Permissions](permissions.md#headless-mode-atelier_approve-and---print)).
A prompt that must run commands needs either allow rules in the project's
`atelier.toml` (with `trust-project: true`) or `permission-mode: yolo`; both
are decisions about how much you trust the prompt and the checkout.

### `comment`

For `issue_comment` and `pull_request_review_comment` events. The action
acts only when **all** of these hold, and otherwise logs one line and ends
successfully with `triggered: false`:

1. the comment was just created (an edit is not a request);
2. it starts with the trigger phrase (`@atelier`; `@atelier-dev` or
   `please @atelier` do not count);
3. its author's `author_association` is `OWNER`, `MEMBER` or `COLLABORATOR`.

Then:

- **On an issue**: atelier runs in the checkout as it is (the default
  branch, for these events) with the rest of the comment as its request,
  and the answer is posted as a comment.
- **On a pull request**: the action fetches `refs/pull/<n>/head`, checks out
  the head commit (detached), runs the request there, and replies — in the
  thread for a review comment, as a new comment otherwise. A pull request
  from a **fork is refused** unless `allow-forks: true`: the comment gets a
  reply saying so and the step fails.

The prompt atelier gets is the request, followed by the issue's or pull
request's title and description (and, for a review comment, the file, line
and diff hunk) marked as context written by others, not as instructions.

As in `prompt` mode, edits stay in the workspace; push them from a later
step if you want that (see the [example](#atelier-in-comments)).

## Safety

An agent in CI holds a token, sometimes secrets, and reads text written by
strangers. These are the rules the action is built on; each is enforced in
the scripts, not just recommended here.

**Everything from the event is data.** Comment bodies, titles, branch names
and file contents are read from the event file with `jq` and reach programs
in files, on stdin or as quoted arguments — never as part of a script.
`action.yml` uses `${{ }}` only in `env:` blocks; every script runs under
`set -euo pipefail` with no `eval`. The prompt goes to atelier on stdin; a
branch name only ever appears inside a refspec or behind `origin/`; a pull
request is fetched by number. The tests feed the scripts comments, titles
and branch names made of `$(…)`, backticks, quotes and line breaks and check
that a canary file is never created.

Mind the same rule in your own steps: an output such as `head-ref` or
`result` is untrusted text. Pass it through `env:` and quote it.

**Only people with write access can command it.** In `comment` mode a
comment by anyone whose `author_association` is not `OWNER`, `MEMBER` or
`COLLABORATOR` is ignored. An agent that anyone can instruct from a public
issue is remote code execution with your token.

**Forks are refused in `comment` mode** unless `allow-forks: true`. A
comment workflow runs with the repository's token and secrets; a fork's head
is code and instruction files (`AGENTS.md`) written by someone outside, who
can also push again between the maintainer's comment and the run. Turn it on
only if the job's permissions and the secrets it sees are ones you would
hand to the fork's author. Even then, `trust-project` is ignored for a
fork's code.

**`pull_request_target` is not supported**, in any mode: the action detects
the event and stops. That event runs with the base repository's secrets and
a read/write token while the code of interest is the pull request's —
checking that code out there is the classic way a repository is compromised.
Use `pull_request`. The cost: pull requests from forks get no secrets under
`pull_request`, so they are not reviewed by a model that needs a key.

**The project file is not trusted by default.** With `trust-project: false`
a checkout's `atelier.toml` cannot start MCP servers, run hooks or checkers,
add providers or allow commands ([Project trust](trust.md)) — so a pull
request cannot make the action execute something by editing that file. The
action also clears `ATELIER_TRUST_PROJECT` and `ATELIER_TRUST_HOOKS` from
the job's environment: the input is the only switch. Set `trust-project:
true` only in workflows that run on branches you control — a scheduled job
or a `workflow_dispatch` on the default branch — and not for `pull_request`
or `comment` workflows, where the file under the agent is whatever the
branch's author pushed.

**Review is read-only.** `review` runs in `plan` mode, and `atelier review`
has no tool that writes or runs anything to begin with. The permission mode
is the `permission-mode` input's alone: `ATELIER_APPROVE` in the job's
environment is cleared.

**Secrets stay where they are needed.** The API key reaches atelier in its
environment and is never written to the log (it is registered with
`::add-mask::`, which the runner consumes);
`ATELIER_DEBUG`, which prints request bodies, is cleared. The GitHub token
is used by the action's own `gh` calls and is removed from atelier's
environment (`GH_TOKEN`, `GITHUB_TOKEN` and the input itself), so a command
the agent runs does not inherit it. While atelier runs, workflow commands
are switched off (`::stop-commands::`), so a line of model output starting
with `::` is just text in the log.

**Give the job no more than it needs:**

| Workflow | `permissions:` |
|----------|----------------|
| Review, posting comments | `contents: read`, `pull-requests: write` |
| Review, `post: summary` | `contents: read` |
| `@atelier` replies only | `contents: read`, `issues: write`, `pull-requests: write` |
| `@atelier` whose changes the workflow pushes | add `contents: write` to that job only |
| A prompt that opens a pull request | `contents: write`, `pull-requests: write` |

What remains is the model itself: a pull request's diff, an issue's text and
the files in the checkout are all read by it, and text can try to steer it
(prompt injection). The defaults bound what that can achieve — a read-only
review; no shell, no network scripts and no project-defined tools elsewhere
— and `yolo`, `trust-project` and `allow-forks` each remove one of those
bounds. Use them where the input is yours.

## Examples

These are examples to copy into your own repository; none of them is
installed in this one.

### Pull request review

The [quick start](#quick-start-review-pull-requests) above. To report
without commenting or failing:

```yaml
      - uses: KarpelesLab/atelier@v0
        id: review
        with:
          base-url: https://llm.example.com/v1
          api-key: ${{ secrets.LLM_API_KEY }}
          post: summary
      - name: Count findings
        env:
          FINDINGS: ${{ steps.review.outputs.findings }}
        run: jq length <<<"$FINDINGS"
```

### `@atelier` in comments

```yaml
# .github/workflows/atelier-comment.yml
name: atelier in comments
on:
  issue_comment:
    types: [created]
  pull_request_review_comment:
    types: [created]

permissions:
  contents: read
  issues: write
  pull-requests: write

jobs:
  answer:
    runs-on: ubuntu-latest
    # A cheap filter so most comments start no job at all. The action checks
    # both conditions again itself; this line is not the security boundary.
    if: >-
      startsWith(github.event.comment.body, '@atelier') &&
      contains(fromJSON('["OWNER","MEMBER","COLLABORATOR"]'), github.event.comment.author_association)
    steps:
      - uses: actions/checkout@v4
        with:
          persist-credentials: false
      - uses: KarpelesLab/atelier@v0
        with:
          mode: comment
          base-url: https://llm.example.com/v1
          model: my-coder
          api-key: ${{ secrets.LLM_API_KEY }}
```

That answers questions and proposes changes in its reply. To let a request
on a pull request change the branch, give the job `contents: write`, drop
`persist-credentials: false`, and push after the action:

```yaml
    permissions:
      contents: write
      issues: write
      pull-requests: write
    steps:
      - uses: actions/checkout@v4
      - uses: KarpelesLab/atelier@v0
        id: atelier
        with:
          mode: comment
          base-url: https://llm.example.com/v1
          api-key: ${{ secrets.LLM_API_KEY }}
      - name: Push what atelier changed
        if: steps.atelier.outputs.triggered == 'true' && steps.atelier.outputs.head-ref != ''
        env:
          HEAD_REF: ${{ steps.atelier.outputs.head-ref }}   # untrusted: env, quoted
        run: |
          if [ -n "$(git status --porcelain)" ]; then
            git config user.name "github-actions[bot]"
            git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
            git add -A
            git commit -m "atelier: changes requested in a comment"
            git push origin "HEAD:refs/heads/$HEAD_REF"
          fi
```

(`head-ref` is empty for a comment on an issue, so that step is skipped
there. A push made with the job's token does not start other workflows.)

### A scheduled prompt that opens a pull request

```yaml
# .github/workflows/atelier-weekly.yml
name: weekly tidy
on:
  schedule:
    - cron: "0 3 * * 1"
  workflow_dispatch:

permissions:
  contents: write
  pull-requests: write

jobs:
  tidy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: KarpelesLab/atelier@v0
        id: atelier
        with:
          mode: prompt
          prompt: |
            Find TODO comments that the code around them has already made
            obsolete and remove them. Change nothing else. End with a list
            of what you removed and why.
          base-url: https://llm.example.com/v1
          api-key: ${{ secrets.LLM_API_KEY }}
          max-turns: 60
          # The default branch is ours: its atelier.toml may be trusted.
          trust-project: true
      - uses: peter-evans/create-pull-request@v6
        with:
          branch: atelier/weekly-tidy
          title: "chore: remove obsolete TODOs"
          body: ${{ steps.atelier.outputs.result }}
          commit-message: "chore: remove obsolete TODOs"
```

`create-pull-request` commits whatever the workspace holds and opens (or
updates) the pull request; when atelier changed nothing, it does nothing.
The same with plain git, in place of that last step:

```yaml
      - name: Open a pull request
        env:
          GH_TOKEN: ${{ github.token }}
          BODY: ${{ steps.atelier.outputs.result }}
        run: |
          [ -n "$(git status --porcelain)" ] || exit 0
          git config user.name "github-actions[bot]"
          git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
          git checkout -B atelier/weekly-tidy
          git add -A
          git commit -m "chore: remove obsolete TODOs"
          git push --force origin atelier/weekly-tidy
          gh pr create --fill --body "$BODY" || gh pr edit --body "$BODY"
```

### A self-hosted runner and a local model

The common case for atelier: the model runs on your own server, which a
GitHub-hosted runner cannot reach. A self-hosted runner inside the network
can.

```yaml
name: atelier review
on:
  pull_request:

permissions:
  contents: read
  pull-requests: write

jobs:
  review:
    runs-on: [self-hosted, linux]
    if: github.event.pull_request.head.repo.full_name == github.repository
    steps:
      - uses: actions/checkout@v4
        with:
          ref: ${{ github.event.pull_request.head.sha }}
          fetch-depth: 0
          persist-credentials: false
      - uses: KarpelesLab/atelier@v0
        with:
          base-url: http://llm.internal:11400/v1
          model: qwen3-coder:30b
          version: v0.9.0
```

No `api-key` when the endpoint asks for none. The runner needs `bash`,
`curl`, `jq`, `gh`, `git`, `sha256sum` and `gunzip`; the binary is
downloaded once per version and kept in the runner's tool cache. Do not
attach a self-hosted runner to a public repository that runs workflows for
pull requests from forks: the job runs on your machine, in your network.

## Versions

- `KarpelesLab/atelier@v0` — the moving tag for the current major version;
  it is moved to each release. (Releases are tagged `vX.Y.Z` by
  [release-plz](../README.md#install); the maintainers move `v0` after one.)
- `KarpelesLab/atelier@v0.9.0` — the action's scripts as of that release.
- `KarpelesLab/atelier@<commit sha>` — what GitHub recommends for
  third-party actions when you want the scripts themselves pinned.

The ref after `@` selects the *action* (its scripts); the `version` input
selects the *binary* it installs, `latest` by default. Pin both for a
reproducible job.

## What is not verified yet

Tested without GitHub (`tests/action.rs`: fixture events, a stub `gh` that
records request bodies, a stub `atelier`, real `git` on a throwaway
repository): the install and its checksum failures, the mapping of findings
to inline comments and body, replacement on re-run, the fallback when a
review is refused, the authorization gate, the fork and
`pull_request_target` refusals, hostile text passing through as data, and
which variables reach atelier.

Only a real runner can confirm:

- that GitHub's API accepts the request bodies as built (review with
  `line`/`side` comments, `PUT` on an earlier review, replies to review
  comments) and that `gh api --paginate` output is read correctly;
- that GitHub's diff and `git diff` agree on which lines are commentable
  often enough that the body-only fallback is rare;
- the shallow-checkout path (`git fetch --unshallow` on a default
  `actions/checkout`), and fetching `refs/pull/<n>/head` with the
  credentials `actions/checkout` leaves;
- `$GITHUB_ACTION_PATH`, the composite `env:` mapping, `::add-mask::` and
  `::stop-commands::` as the runner implements them;
- the release download itself (`releases/latest` redirect, asset names) and
  the arm64 runner;
- GitHub Enterprise Server, which is untried.
