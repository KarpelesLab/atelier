#!/usr/bin/env bash
# Shared helpers of the atelier GitHub Action (see docs/github-action.md).
# Sourced by the other scripts; it runs nothing by itself.
#
# One rule holds for every script here: what comes from the event (comment
# bodies, titles, branch names, file contents) and what the model writes is
# data. It travels in files and environment variables and reaches programs as
# quoted arguments or on stdin; it is never spliced into shell source.

# The hidden markers that let a later run recognize what an earlier one
# posted. A superseded review carries a different marker, so it is not picked
# up again.
# shellcheck disable=SC2034  # used by the scripts that source this file
MARKER_REVIEW='<!-- atelier-review -->'
# shellcheck disable=SC2034
MARKER_SUPERSEDED='<!-- atelier-review:superseded -->'
# shellcheck disable=SC2034
MARKER_REPLY='<!-- atelier-reply -->'

# The `result` output, and a comment's body, are cut to this many characters
# (GitHub refuses a comment body over 65536).
# shellcheck disable=SC2034
RESULT_LIMIT=60000

# One line, safe to follow `::warning::` or `::error::`: a line break in a
# workflow command's message would start a new command.
oneline() {
  printf '%s' "$*" | tr '\r\n' '  '
}

log() {
  printf 'atelier-action: %s\n' "$(oneline "$*")" >&2
}

warn() {
  printf '::warning title=atelier::%s\n' "$(oneline "$*")"
}

die() {
  printf '::error title=atelier::%s\n' "$(oneline "$*")"
  exit 1
}

# 32 hex digits nobody can guess: a heredoc delimiter, a stop-commands token.
random_token() {
  od -An -N16 -tx1 /dev/urandom | tr -d ' \n'
}

# set_output NAME FILE: the file's content becomes the step output NAME. The
# delimiter is random, so no content can end the value early and define
# outputs of its own.
set_output() {
  local name=$1 file=$2 delim
  [ -n "${GITHUB_OUTPUT:-}" ] || return 0
  delim="atelier_$(random_token)"
  {
    printf '%s<<%s\n' "$name" "$delim"
    cat -- "$file"
    printf '\n%s\n' "$delim"
  } >>"$GITHUB_OUTPUT"
}

# set_output_value NAME VALUE, for short values the scripts made themselves.
set_output_value() {
  local tmp
  tmp=$(mktemp)
  printf '%s' "$2" >"$tmp"
  set_output "$1" "$tmp"
  rm -f -- "$tmp"
}

# summary FILE: append the file to the job's step summary, when there is one.
summary() {
  [ -n "${GITHUB_STEP_SUMMARY:-}" ] || return 0
  {
    cat -- "$1"
    printf '\n'
  } >>"$GITHUB_STEP_SUMMARY"
}

# `gh` with the token the workflow gave the action. The token is put in the
# environment of this one process only: atelier, and whatever it starts,
# never has it (see run_atelier in run.sh).
gh_api() {
  GH_TOKEN="${INPUT_GITHUB_TOKEN:-}" GH_PROMPT_DISABLED=1 gh api "$@" </dev/null
}
