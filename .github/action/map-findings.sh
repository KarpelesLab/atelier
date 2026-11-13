#!/usr/bin/env bash
# Turn the findings of `atelier review --output-format json` and the pull
# request's diff into the body of ONE pull request review. Pure: it reads
# files and prints one JSON object.
#
#   map-findings.sh --review FILE --diff FILE --commit SHA
#                   [--prefix DIR/] [--drift FILE]
#
#   --review  what `atelier review --output-format json` printed
#   --diff    a unified diff of the pull request (`git diff`, a/ b/ prefixes)
#   --commit  the commit the comments are attached to (the pull request's head)
#   --prefix  where atelier ran, relative to the repository root, with its
#             trailing slash: findings name files relative to that directory,
#             GitHub from the root
#   --drift   a list of files, one per line, whose content where atelier read
#             it is not what --commit has: their line numbers cannot be
#             trusted, so their findings go in the body
#
# GitHub accepts a comment only on a line the diff shows. A finding on such a
# line (added, or context of a hunk) becomes an inline comment; every other
# finding is listed in the review's body, so none is lost.
#
# Output:
#   count     number of findings
#   inline    how many became inline comments
#   review    the request body for POST /repos/{repo}/pulls/{n}/reviews, or
#             null when there are no findings
#   fallback  the same review with every finding in the body and no inline
#             comment, for when GitHub refuses the first
#   summary   the findings as Markdown, for the step summary
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=.github/action/lib.sh
. "$here/lib.sh"

review='' diff='' commit='' prefix='' drift=''
while [ $# -gt 0 ]; do
  case $1 in
    --review) review=${2:?} ;;
    --diff) diff=${2:?} ;;
    --commit) commit=${2:?} ;;
    --prefix) prefix=${2-} ;;
    --drift) drift=${2:?} ;;
    *) die "map-findings.sh: unknown argument" ;;
  esac
  shift 2
done
[ -n "$review" ] && [ -n "$diff" ] && [ -n "$commit" ] ||
  die "map-findings.sh: --review, --diff and --commit are required"

tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT
if [ -z "$drift" ]; then
  drift="$tmp/drift"
  : >"$drift"
fi

# Every line of the new side that the diff shows, as `<line>\t<path>`.
# Inside a hunk only the first character of a line is looked at, so content
# that resembles a header (`+++ x`, an added line reading `++ x`) is content.
awk '
  /^diff --git / { path = ""; inhunk = 0; next }
  !inhunk && /^\+\+\+ / {
    p = substr($0, 5)
    sub(/\t$/, "", p)
    path = (substr(p, 1, 2) == "b/") ? substr(p, 3) : ""
    next
  }
  /^@@ / {
    start = $3
    sub(/^\+/, "", start)
    split(start, parts, ",")
    line = parts[1] + 0
    inhunk = 1
    next
  }
  inhunk && path != "" {
    c = substr($0, 1, 1)
    if (c == "+" || c == " " || $0 == "") { print line "\t" path; line++ }
    else if (c != "-" && c != "\\") inhunk = 0
  }
' "$diff" >"$tmp/lines"

# shellcheck disable=SC2016  # $review and friends are jq variables
jq --null-input --compact-output \
  --slurpfile review "$review" \
  --rawfile lines "$tmp/lines" \
  --rawfile drift "$drift" \
  --arg commit "$commit" \
  --arg prefix "$prefix" \
  --arg marker "$MARKER_REVIEW" \
  --argjson limit "$RESULT_LIMIT" '
  def rank: {"P0": 0, "P1": 1, "P2": 2, "P3": 3}[.] // 3;
  def clip: if length > $limit then .[0:$limit] + "\n\n[… cut]" else . end;
  def plural(n; word): "\(n) \(word)\(if n == 1 then "" else "s" end)";

  # {path: {"<line>": true}} for every commentable line.
  (reduce ($lines | split("\n")[] | select(length > 0)
           | capture("^(?<line>[0-9]+)\t(?<path>.*)$")) as $l
     ({}; .[$l.path][$l.line] = true)) as $shown
  | ($drift | split("\n") | map(select(length > 0))) as $drifted
  | [ ($review[0].findings // [])[]
      | select(type == "object")
      | {
          path: ($prefix + (.file // "" | tostring)),
          line: (if (.line | type) == "number" then (.line | floor) else 0 end),
          severity: (.severity // "P3" | tostring | if rank == 3 then "P3" else . end),
          summary: (.summary // "" | tostring),
          scenario: (.scenario // "" | tostring)
        }
    ]
  | sort_by(.severity | rank) as $all
  | ($all | map(select(
        .line > 0
        and ($shown[.path][.line | tostring] == true)
        and (. as $f | $drifted | index($f.path) == null)))) as $inline
  | ($all - $inline) as $rest

  | def where: if .line > 0 then "`\(.path):\(.line)`" else "`\(.path)`" end;
    def item:
      "- **\(.severity)** \(where) — \(.summary)"
      + (if .scenario != "" then "\n  Fails when: \(.scenario)" else "" end);
    def counts:
      [ ("P0", "P1", "P2", "P3") as $s
        | ($all | map(select(.severity == $s)) | length) as $n
        | select($n > 0) | "\($n) \($s)" ] | join(", ");
    def head:
      "### atelier review\n\n"
      + (if ($all | length) == 0 then "No findings"
         else "\(plural($all | length; "finding")) (\(counts))" end)
      + " in the changes up to `\($commit[0:10])`.";
    def listed(items; title):
      if (items | length) == 0 then ""
      else "\n\n\(title)\n\n" + (items | map(item) | join("\n")) end;

    {
      count: ($all | length),
      inline: ($inline | length),
      review: (
        if ($all | length) == 0 then null
        else {
          commit_id: $commit,
          event: "COMMENT",
          body: ((head + listed($rest; "On lines the diff does not show:") | clip)
                 + "\n\n" + $marker),
          comments: ($inline | map({
            path: .path,
            line: .line,
            side: "RIGHT",
            body: (("**\(.severity)** \(.summary)"
                    + (if .scenario != "" then "\n\n**Fails when:** \(.scenario)" else "" end)
                    | clip) + "\n\n" + $marker)
          }))
        } end),
      fallback: {
        commit_id: $commit,
        event: "COMMENT",
        body: ((head + listed($all; "Findings:") | clip) + "\n\n" + $marker)
      },
      summary: (head + listed($all; "Findings:") | clip)
    }
  '
