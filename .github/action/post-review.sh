#!/usr/bin/env bash
# Post what map-findings.sh produced as one pull request review, and retire
# the reviews earlier runs of this action left on the same pull request.
#
#   post-review.sh --mapped FILE --repo OWNER/NAME --pr NUMBER
#
# Everything this action posts carries a hidden marker (lib.sh). A run:
#   1. lists the pull request's reviews and keeps the ids of those with the
#      marker (the earlier runs');
#   2. posts the new review, when there are findings — with its inline
#      comments, or, if GitHub refuses them (a line it does not consider part
#      of the diff), again with every finding in the body;
#   3. for each earlier review, deletes its marked inline comments and
#      replaces its body with a one-line "superseded" note.
# So a pull request shows the findings of the latest run only, however many
# pushes it saw. (A `COMMENT` review cannot be dismissed or deleted through
# the API once submitted; emptying it is what the API allows.)
#
# Exit status: 0 when the pull request shows the new result, 1 when the new
# review could not be posted (the caller falls back to the step summary).
# Failing to retire an old review is a warning, not a failure.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=.github/action/lib.sh
. "$here/lib.sh"

mapped='' repo='' pr=''
while [ $# -gt 0 ]; do
  case $1 in
    --mapped) mapped=${2:?} ;;
    --repo) repo=${2:?} ;;
    --pr) pr=${2:?} ;;
    *) die "post-review.sh: unknown argument" ;;
  esac
  shift 2
done
[[ $repo =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || die "post-review.sh: --repo is not OWNER/NAME"
[[ $pr =~ ^[0-9]+$ ]] || die "post-review.sh: --pr is not a number"
[ -r "$mapped" ] || die "post-review.sh: --mapped is not readable"

tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT

# Ids, one per line, of the listed objects whose body carries the marker.
# `gh api --paginate` prints one JSON array per page; jq reads them all.
marked_ids() {
  jq --raw-output --arg marker "$MARKER_REVIEW" \
    '.[] | select(type == "object") | select((.body // "") | contains($marker))
     | .id | select(type == "number")'
}

# 1. The earlier runs' reviews, before ours is added to the list.
if ! gh_api --paginate "repos/$repo/pulls/$pr/reviews?per_page=100" >"$tmp/reviews.json"; then
  warn "could not list the reviews of pull request #$pr: an earlier atelier review, if any, stays as it is"
  : >"$tmp/reviews.json"
fi
marked_ids <"$tmp/reviews.json" >"$tmp/old" || : >"$tmp/old"

# 2. The new review.
count=$(jq --raw-output '.count' "$mapped")
sha=$(jq --raw-output '.fallback.commit_id[0:10]' "$mapped")
if [ "$count" -gt 0 ]; then
  jq '.review' "$mapped" >"$tmp/review.json"
  if ! gh_api --method POST "repos/$repo/pulls/$pr/reviews" --input "$tmp/review.json" >/dev/null; then
    log "GitHub refused the review with inline comments; posting the findings in the review body instead"
    jq '.fallback' "$mapped" >"$tmp/fallback.json"
    gh_api --method POST "repos/$repo/pulls/$pr/reviews" --input "$tmp/fallback.json" >/dev/null || exit 1
  fi
  log "posted a review with $count finding(s) on pull request #$pr"
  note="Superseded by a newer atelier review."
else
  note="Superseded: the latest atelier review, of \`$sha\`, reported no findings."
fi

# 3. Retire the earlier ones.
jq --null-input --arg body "$note

$MARKER_SUPERSEDED" '{body: $body}' >"$tmp/superseded.json"
while IFS= read -r review_id; do
  [[ $review_id =~ ^[0-9]+$ ]] || continue
  if gh_api --paginate "repos/$repo/pulls/$pr/reviews/$review_id/comments?per_page=100" >"$tmp/comments.json"; then
    marked_ids <"$tmp/comments.json" >"$tmp/old-comments" || : >"$tmp/old-comments"
    while IFS= read -r comment_id; do
      [[ $comment_id =~ ^[0-9]+$ ]] || continue
      gh_api --method DELETE "repos/$repo/pulls/comments/$comment_id" >/dev/null ||
        warn "could not delete comment $comment_id of an earlier atelier review"
    done <"$tmp/old-comments"
  else
    warn "could not list the comments of the earlier atelier review $review_id"
  fi
  gh_api --method PUT "repos/$repo/pulls/$pr/reviews/$review_id" --input "$tmp/superseded.json" >/dev/null ||
    warn "could not mark the earlier atelier review $review_id as superseded"
done <"$tmp/old"
