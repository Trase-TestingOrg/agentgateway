#!/usr/bin/env bash
# Release the Trase build: tag trase/<upstream base>-<N> on a clean, synced, green
# trase/main, push the tag, wait for the release workflow, print the image digest.
#
#   trase/release.sh            # next build number for the current upstream base
#   trase/release.sh 3          # explicit build number
#   trase/release.sh --no-wait  # tag and push, do not wait for the workflow
#
# Needs: git, gh (logged in to the fork), a push URL that can create tags.
set -euo pipefail

wait_for_run=1
build=""
for arg in "$@"; do
  case "$arg" in
    --no-wait) wait_for_run=0 ;;
    [0-9]*) build="$arg" ;;
    *) echo "usage: trase/release.sh [N] [--no-wait]" >&2; exit 2 ;;
  esac
done

remote="${TRASE_REMOTE:-$(git remote | grep -E '^(trasefork|origin)$' | head -1)}"
repo="$(gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null || echo Trase-TestingOrg/agentgateway)"

branch="$(git branch --show-current)"
if [ "$branch" != "trase/main" ]; then
  echo "release from trase/main (on $branch)" >&2; exit 1
fi
if [ -n "$(git status --porcelain)" ]; then
  echo "working tree is not clean" >&2; exit 1
fi
git fetch --quiet --tags "$remote"
if [ "$(git rev-parse HEAD)" != "$(git rev-parse "$remote/trase/main")" ]; then
  echo "trase/main is not in sync with $remote/trase/main; pull or push first" >&2; exit 1
fi

base="$(git describe --tags --abbrev=0 --match 'v*')"
trase/check-patches.sh "$base"

conclusion="$(gh run list --repo "$repo" --workflow trase-ci --branch trase/main --commit "$(git rev-parse HEAD)" --limit 1 --json conclusion --jq '.[0].conclusion // "none"')"
if [ "$conclusion" != "success" ]; then
  echo "trase-ci for $(git rev-parse --short HEAD) is '$conclusion', not success; wait for it or fix it" >&2; exit 1
fi

if [ -z "$build" ]; then
  last="$(git tag --list "trase/${base}-*" | sed "s#^trase/${base}-##" | sort -n | tail -1)"
  build=$(( ${last:-0} + 1 ))
fi
tag="trase/${base}-${build}"
image_tag="${base}-trase.${build}"
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  echo "tag $tag already exists" >&2; exit 1
fi

patches="$(git rev-list --count --no-merges "${base}..HEAD")"
git tag -a "$tag" -m "Trase build ${build} of agentgateway ${base}: ${patches} patches (see trase/PATCHES.md)"
git push "$remote" "$tag"
echo "tagged $tag -> image tag $image_tag ($patches patches on $base)"

if [ "$wait_for_run" -eq 0 ]; then
  echo "release workflow: https://github.com/$repo/actions/workflows/trase-release.yml"; exit 0
fi

echo "waiting for the release workflow..."
run_id=""
for _ in $(seq 1 30); do
  run_id="$(gh run list --repo "$repo" --workflow trase-release --event push --limit 10 --json databaseId,headBranch --jq ".[] | select(.headBranch==\"$tag\") | .databaseId" | head -1)"
  [ -n "$run_id" ] && break
  sleep 5
done
if [ -z "$run_id" ]; then
  echo "no release run found for $tag yet; check https://github.com/$repo/actions" >&2; exit 1
fi
gh run watch --repo "$repo" "$run_id" --exit-status
gh run view --repo "$repo" "$run_id" --log 2>/dev/null | grep -o 'image: `[^`]*`' | tail -1 | sed 's/`//g'
echo "pin that digest in the governance-proxy chart and Tilt"
