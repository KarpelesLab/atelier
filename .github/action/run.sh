#!/usr/bin/env bash
# The atelier GitHub Action: run a review, a prompt, or what a comment asks.
# See docs/github-action.md for what each mode does and why it is built the
# way it is; action.yml maps the action's inputs to the INPUT_* variables
# read here.
#
# Untrusted text (the event, the model's output) is only ever read from files
# with jq and handed on as quoted arguments, on stdin or in files. Nothing
# here uses `eval`, and no variable is expanded unquoted except the two
# arrays built by this script.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=.github/action/lib.sh
. "$here/lib.sh"

mode=${INPUT_MODE:-review}
event=${GITHUB_EVENT_NAME:-}
repo=${GITHUB_REPOSITORY:-}
atelier=${ATELIER_BIN:-atelier}

# ---------------------------------------------------------------- refusals

# `pull_request_target` runs with the base repository's secrets and a
# read/write token, while the code a review or a prompt would look at is the
# pull request's — anyone's. That combination is how repositories get
# compromised, so the action does not take part in it, in any mode.
if [ "$event" = pull_request_target ]; then
  die "the pull_request_target event is not supported: it would run atelier with this repository's secrets on code from the pull request. Use the pull_request event (see docs/github-action.md)."
fi

case $mode in
  review | prompt | comment) ;;
  *) die "mode: expected review, prompt or comment" ;;
esac

permission_mode=${INPUT_PERMISSION_MODE:-}
if [ -z "$permission_mode" ]; then
  # A review only reads; a prompt is usually asked to change something.
  if [ "$mode" = review ]; then permission_mode=plan; else permission_mode=auto-edit; fi
fi
case $permission_mode in
  plan | ask-edits | auto-edit | yolo) ;;
  *) die "permission-mode: expected plan, ask-edits, auto-edit or yolo" ;;
esac

max_turns=${INPUT_MAX_TURNS:-30}
[[ $max_turns =~ ^[1-9][0-9]*$ ]] || die "max-turns: expected a positive number"

fail_on=${INPUT_FAIL_ON:-}
case $fail_on in
  '' | P0 | P1 | P2 | P3) ;;
  *) die "fail-on: expected P0, P1, P2, P3 or nothing" ;;
esac

post=${INPUT_POST:-}
case $post in
  '' | comments | summary | none) ;;
  *) die "post: expected comments, summary or none" ;;
esac

# extra-args is the workflow author's: split on whitespace into words, with
# no quoting, no globbing and no expansion of any kind.
extra=()
if [ -n "${INPUT_EXTRA_ARGS:-}" ]; then
  read -r -d '' -a extra <<<"$INPUT_EXTRA_ARGS" || true
fi

# ------------------------------------------------------------- environment

# The key reaches atelier in its environment and nowhere else. A value that
# came from `secrets.*` is masked by the runner already; this covers one that
# did not. (`::add-mask::` is read by the runner and not shown in the log.)
if [ -n "${INPUT_API_KEY:-}" ]; then
  while IFS= read -r key_line; do
    if [ -n "$key_line" ]; then printf '::add-mask::%s\n' "$key_line"; fi
  done <<<"$INPUT_API_KEY"
  export ATELIER_API_KEY=$INPUT_API_KEY
fi
if [ -n "${INPUT_BASE_URL:-}" ]; then export ATELIER_BASE_URL=$INPUT_BASE_URL; fi
if [ -n "${INPUT_MODEL:-}" ]; then export ATELIER_MODEL=$INPUT_MODEL; fi

# The mode is the input's and nothing else's: ATELIER_APPROVE=all in the
# job's environment would otherwise be `yolo`.
export ATELIER_MODE=$permission_mode
unset ATELIER_APPROVE
# ATELIER_DEBUG prints request bodies — the conversation, file contents — to
# the job log.
unset ATELIER_DEBUG
# The version installed is the version that runs; no peers on a runner.
export ATELIER_NO_UPDATE=1 ATELIER_NO_COORDINATOR=1
# Project trust is the `trust-project` input's to give, not the job
# environment's: see trust_project below.
unset ATELIER_TRUST_PROJECT ATELIER_TRUST_HOOKS

# trust_project FOREIGN: accept the project's atelier.toml when the workflow
# asked for it — unless the checkout is code from a fork, where that file is
# the fork author's and would start their MCP servers and hooks.
trust_project() {
  [ "${INPUT_TRUST_PROJECT:-false}" = true ] || return 0
  if [ "$1" = true ]; then
    warn "trust-project is ignored: the code comes from a fork, and its atelier.toml is not the repository's"
    return 0
  fi
  export ATELIER_TRUST_PROJECT=1
}

work=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/atelier-action.XXXXXX")
trap 'rm -rf -- "$work"' EXIT

if [ -n "${INPUT_WORKING_DIRECTORY:-}" ]; then
  cd -- "$INPUT_WORKING_DIRECTORY" || die "working-directory: cannot enter it"
fi

# run_atelier STDIN STDOUT ARGS…: run atelier, leaving its status in $code.
#
# - It does not get the GitHub token: not the action's input, not the
#   variables `gh` and `git` read.
# - Its stderr (progress: tool calls, the answer as written) goes to the job
#   log between `::stop-commands::` and its end token, so a line the model —
#   or a file it printed — starts with `::` is text, not a workflow command.
code=0
run_atelier() {
  local stdin=$1 stdout=$2 token
  shift 2
  token=$(random_token)
  printf '::stop-commands::%s\n' "$token"
  set +e
  env -u INPUT_GITHUB_TOKEN -u INPUT_API_KEY -u GH_TOKEN -u GITHUB_TOKEN \
    -u GH_ENTERPRISE_TOKEN -u GITHUB_ENTERPRISE_TOKEN \
    "$atelier" "$@" <"$stdin" >"$stdout"
  code=$?
  set -e
  printf '\n::%s::\n' "$token"
}

# finish: publish the exit status and end the step with it.
finish() {
  set_output_value exit-code "$code"
  if [ "$code" -ne 0 ]; then
    case $code in
      3) why="--max-turns ($max_turns) ran out before the model finished" ;;
      4) why="the answer did not have the shape asked for, even after the retry" ;;
      5) why="the review reported a finding at or above $fail_on" ;;
      *) why="the run failed" ;;
    esac
    printf '::error title=atelier::atelier exited with status %s: %s\n' "$code" "$why"
  fi
  exit "$code"
}

# publish_result FILE HEADING: the `result` output and the step summary from
# what `atelier --print --output-format json` wrote. Leaves the text, cut to
# the documented size, in $work/result.txt.
publish_result() {
  local json=$1 heading=$2
  # shellcheck disable=SC2016
  if ! jq --raw-output --argjson limit "$RESULT_LIMIT" '
        ((.result // "") | tostring) as $text
        | (if .is_error == true
           then "\n\n_atelier stopped (\(.subtype // "error" | tostring)): \(.error // "no detail" | tostring)_"
           else "" end) as $tail
        | (if ($text | length) > $limit then $text[0:$limit] + "\n\n[… cut]" else $text end) + $tail
      ' "$json" >"$work/result.txt" 2>/dev/null; then
    printf '_atelier produced no result (exit status %s); see the job log._\n' "$code" >"$work/result.txt"
  fi
  set_output result "$work/result.txt"
  {
    printf '### %s\n\n' "$heading"
    cat "$work/result.txt"
  } >"$work/summary.md"
  summary "$work/summary.md"
}

# ------------------------------------------------------------------ review

# Make `origin/<base>` usable as a merge base. actions/checkout fetches one
# commit by default, which has no history to find where the branch left.
ensure_base() {
  local base=$1
  if ! git rev-parse --verify --quiet "refs/remotes/origin/$base^{commit}" >/dev/null; then
    log "fetching the base branch"
    git fetch --no-tags --quiet origin "+refs/heads/$base:refs/remotes/origin/$base" ||
      die "could not fetch the pull request's base branch from origin"
  fi
  if ! git merge-base "refs/remotes/origin/$base" HEAD >/dev/null 2>&1; then
    if [ "$(git rev-parse --is-shallow-repository)" = true ]; then
      log "the checkout is shallow: fetching the history (use fetch-depth: 0 to avoid this)"
      git fetch --no-tags --quiet --unshallow origin ||
        die "could not fetch the history of a shallow checkout; set fetch-depth: 0 on actions/checkout"
    fi
    git merge-base "refs/remotes/origin/$base" HEAD >/dev/null 2>&1 ||
      die "the checkout shares no history with the base branch; set fetch-depth: 0 on actions/checkout"
  fi
}

do_review() {
  local args=(review --output-format json --max-turns "$max_turns")
  [ -z "$fail_on" ] || args+=(--fail-on "$fail_on")

  local is_pr=false number='' base='' head_sha='' foreign=false
  if [ "$event" = pull_request ]; then
    is_pr=true
    number=$(jq --raw-output '.pull_request.number // empty' "$GITHUB_EVENT_PATH")
    base=$(jq --raw-output '.pull_request.base.ref // empty' "$GITHUB_EVENT_PATH")
    head_sha=$(jq --raw-output '.pull_request.head.sha // empty' "$GITHUB_EVENT_PATH")
    # shellcheck disable=SC2016
    foreign=$(jq --raw-output --arg repo "$repo" \
      '((.pull_request.head.repo.full_name // "") | ascii_downcase) != ($repo | ascii_downcase)' \
      "$GITHUB_EVENT_PATH")
    [[ $number =~ ^[0-9]+$ ]] || die "the pull_request event names no pull request number"
    [[ $head_sha =~ ^[0-9a-f]{40,64}$ ]] || die "the pull_request event names no head commit"
    [ -n "$base" ] || die "the pull_request event names no base branch"
    # A branch name is anyone's to choose. It reaches git inside a refspec
    # and atelier behind `origin/`, so it can never be read as an option.
    ensure_base "$base"
    args+=(--base "origin/$base")
  fi
  args+=(${extra[@]+"${extra[@]}"})
  trust_project "$foreign"

  run_atelier /dev/null "$work/review.json" "${args[@]}"

  if jq --exit-status '.findings | type == "array"' "$work/review.json" >/dev/null 2>&1; then
    jq --compact-output '.findings' "$work/review.json" >"$work/findings.json"
  else
    # No report (a failed run): nothing to post; the status says why.
    printf '[]\n' >"$work/findings.json"
    set_output findings "$work/findings.json"
    printf '_atelier review produced no report (exit status %s); see the job log._\n' "$code" >"$work/result.txt"
    set_output result "$work/result.txt"
    [ "$post" = none ] || summary "$work/result.txt"
    [ "$code" -ne 0 ] || code=1
    finish
  fi
  set_output findings "$work/findings.json"

  # The pull request's diff, as GitHub shows it: from where the head left
  # the base, to the head. actions/checkout's default for a pull request is
  # a merge commit, whose files can differ from the head's; those that do
  # are listed as drifted and their findings go in the body.
  local commit=${head_sha:-$(git rev-parse HEAD)} from prefix
  prefix=$(git rev-parse --show-prefix)
  : >"$work/pr.diff"
  : >"$work/drift"
  if [ "$is_pr" = true ]; then
    local diff=(git -c core.quotePath=false diff --no-color --no-ext-diff
      --src-prefix=a/ --dst-prefix=b/)
    if git cat-file -e "$head_sha^{commit}" 2>/dev/null &&
      from=$(git merge-base "refs/remotes/origin/$base" "$head_sha" 2>/dev/null); then
      "${diff[@]}" "$from" "$head_sha" >"$work/pr.diff" || : >"$work/pr.diff"
      git -c core.quotePath=false diff --no-color --no-ext-diff --name-only "$head_sha" >"$work/drift" ||
        : >"$work/drift"
    elif from=$(jq --raw-output '.reviewed.merge_base // empty' "$work/review.json") &&
      [[ $from =~ ^[0-9a-f]{40,64}$ ]]; then
      "${diff[@]}" "$from" HEAD >"$work/pr.diff" || : >"$work/pr.diff"
    fi
  fi
  "$here/map-findings.sh" --review "$work/review.json" --diff "$work/pr.diff" \
    --commit "$commit" --prefix "$prefix" --drift "$work/drift" >"$work/mapped.json"
  jq --raw-output '.summary' "$work/mapped.json" >"$work/result.txt"
  set_output result "$work/result.txt"

  [ -n "$post" ] || { if [ "$is_pr" = true ]; then post=comments; else post=summary; fi; }
  if [ "$post" = comments ] && [ "$is_pr" != true ]; then
    log "post: comments needs a pull_request event; writing the step summary instead"
    post=summary
  fi
  if [ "$post" = comments ]; then
    if ! "$here/post-review.sh" --mapped "$work/mapped.json" --repo "$repo" --pr "$number"; then
      warn "could not post the review on pull request #$number (does the job have 'pull-requests: write'? a pull request from a fork gets a read-only token); the findings are in the step summary"
      post=summary
    fi
  fi
  [ "$post" != summary ] || summary "$work/result.txt"
  finish
}

# ------------------------------------------------------------------ prompt

do_prompt() {
  [ -n "${INPUT_PROMPT:-}" ] || die "prompt: mode 'prompt' needs a prompt"
  printf '%s\n' "$INPUT_PROMPT" >"$work/prompt.txt"
  trust_project false
  # The prompt is read from stdin: it is never an argument that could be
  # taken for an option.
  run_atelier "$work/prompt.txt" "$work/result.json" \
    --print --output-format json --max-turns "$max_turns" ${extra[@]+"${extra[@]}"}
  publish_result "$work/result.json" "atelier"
  finish
}

# ----------------------------------------------------------------- comment

# reply NUMBER COMMENT_ID INLINE FILE: answer the comment — in its
# thread when it is a review comment, else as a new comment. The body opens
# with a hidden marker, so a reply never starts with the trigger phrase.
reply() {
  local number=$1 comment_id=$2 inline=$3 file=$4
  # shellcheck disable=SC2016
  jq --null-input --rawfile text "$file" --arg marker "$MARKER_REPLY" \
    '{body: ($marker + "\n" + $text)}' >"$work/reply.json"
  if [ "$inline" = true ]; then
    gh_api --method POST "repos/$repo/pulls/$number/comments/$comment_id/replies" \
      --input "$work/reply.json" >/dev/null
  else
    gh_api --method POST "repos/$repo/issues/$number/comments" \
      --input "$work/reply.json" >/dev/null
  fi
}

do_comment() {
  local plan="$work/plan.json"
  "$here/event.sh" >"$plan"
  if [ "$(jq --raw-output '.needs_pr // false' "$plan")" = true ]; then
    local n
    n=$(jq --raw-output '.number' "$plan")
    [[ $n =~ ^[0-9]+$ ]] || die "the event names no pull request number"
    gh_api "repos/$repo/pulls/$n" >"$work/pr.json" ||
      die "could not read pull request #$n (does the job have 'pull-requests: read'?)"
    "$here/event.sh" --pr "$work/pr.json" >"$plan"
  fi

  local verdict reason
  verdict=$(jq --raw-output '.verdict' "$plan")
  reason=$(jq --raw-output '.reason' "$plan")
  if [ "$verdict" = skip ]; then
    log "$reason"
    set_output_value triggered false
    set_output_value exit-code 0
    exit 0
  fi

  local kind number comment_id inline
  number=$(jq --raw-output '.number // empty' "$plan")
  comment_id=$(jq --raw-output '.comment_id // empty' "$plan")
  inline=$(jq --raw-output '.review_comment // false' "$plan")
  if [ "$verdict" != run ]; then
    set_output_value triggered false
    if [[ $number =~ ^[0-9]+$ ]] && [[ $comment_id =~ ^[0-9]+$ ]]; then
      printf 'Not run: %s.\n' "$reason" >"$work/refusal.txt"
      reply "$number" "$comment_id" "$inline" "$work/refusal.txt" ||
        log "could not post the refusal as a reply"
    fi
    set_output_value exit-code 1
    die "refused: $reason"
  fi
  [[ $number =~ ^[0-9]+$ ]] && [[ $comment_id =~ ^[0-9]+$ ]] || die "the event is incomplete"
  kind=$(jq --raw-output '.kind' "$plan")
  log "$reason"
  set_output_value triggered true

  local foreign=false
  if [ "$kind" = pull_request ]; then
    local head_sha
    head_sha=$(jq --raw-output '.head_sha' "$plan")
    foreign=$(jq --raw-output '.fork' "$plan")
    [[ $head_sha =~ ^[0-9a-f]{40,64}$ ]] || die "the pull request has no usable head commit"
    jq --raw-output '.head_ref' "$plan" >"$work/head-ref.txt"
    set_output head-ref "$work/head-ref.txt"
    # The pull request's head, by number: `refs/pull/N/head` exists for
    # forks too, and no branch name is ever put on a command line. What is
    # checked out is the commit event.sh vetted, not whatever the ref points
    # to by now.
    git fetch --no-tags --quiet origin "+refs/pull/$number/head:refs/remotes/atelier-action/pr-$number" ||
      die "could not fetch pull request #$number"
    git checkout --quiet --detach "$head_sha" ||
      die "could not check out the head of pull request #$number (it may have been pushed to since the comment)"
  fi
  trust_project "$foreign"

  jq --raw-output '.prompt' "$plan" >"$work/prompt.txt"
  run_atelier "$work/prompt.txt" "$work/result.json" \
    --print --output-format json --max-turns "$max_turns" ${extra[@]+"${extra[@]}"}
  publish_result "$work/result.json" "atelier, asked on #$number"
  reply "$number" "$comment_id" "$inline" "$work/result.txt" ||
    warn "could not post the reply on #$number (does the job have 'issues: write' / 'pull-requests: write'?); the answer is in the step summary"
  finish
}

case $mode in
  review) do_review ;;
  prompt) do_prompt ;;
  comment) do_comment ;;
esac
