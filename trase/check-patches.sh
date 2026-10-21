#!/usr/bin/env bash
# Every commit above the upstream base must carry the ledger trailers.
# Merge commits (a PR's merge ref) are skipped; the stack itself is linear.
set -euo pipefail
if [ -z "$(git tag --list 'v*' | head -1)" ]; then
  git fetch --quiet --tags https://github.com/agentgateway/agentgateway.git
fi
base="${1:-$(git describe --tags --abbrev=0 --match 'v*' --exclude '*-trase*')}"
status=0
while read -r sha; do
  subject=$(git log -1 --format=%s "$sha")
  for key in Patch-Id Upstream-Status Owner Added Drop-When; do
    if ! git log -1 --format=%B "$sha" | git interpret-trailers --parse | grep -q "^${key}:"; then
      echo "::error::commit ${sha:0:10} (\"$subject\") lacks trailer ${key}"
      status=1
    fi
  done
done < <(git rev-list --reverse --no-merges "${base}..HEAD")
if [ "$status" -eq 0 ]; then
  echo "all $(git rev-list --count --no-merges "${base}..HEAD") patch commits above ${base} carry ledger trailers"
fi
exit "$status"
