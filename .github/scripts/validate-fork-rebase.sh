#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 OLD_UPSTREAM_SHA NEW_UPSTREAM_SHA PRE_REBASE_SHA" >&2
  exit 2
fi

old_upstream_sha="$1"
new_upstream_sha="$2"
pre_rebase_sha="$3"

pre_rebase_commits="${RUNNER_TEMP:-/tmp}/pre-rebase-fork-commits.txt"
post_rebase_commits="${RUNNER_TEMP:-/tmp}/post-rebase-fork-commits.txt"

git log --format='%s' --reverse \
  "${old_upstream_sha}..${pre_rebase_sha}" > "$pre_rebase_commits"
git log --format='%s' --reverse \
  "${new_upstream_sha}..HEAD" > "$post_rebase_commits"

if [[ ! -s "$pre_rebase_commits" ]]; then
  echo "No pre-rebase fork commits were captured; refusing to validate." >&2
  exit 1
fi

if ! cmp -s "$pre_rebase_commits" "$post_rebase_commits"; then
  echo "The curated fork commit sequence changed during rebase:" >&2
  diff -u "$pre_rebase_commits" "$post_rebase_commits" >&2 || true
  echo "Auto-rebase must preserve the fork commit subjects; stop for manual review." >&2
  exit 1
fi

required_files=(
  .github/workflows/fork-release.yml
  codex-rs/cli/src/update.rs
  codex-rs/cli/src/update/install.rs
  codex-rs/workflow/src/lib.rs
)
for path in "${required_files[@]}"; do
  if [[ ! -f "$path" ]]; then
    echo "Required fork file is missing: $path" >&2
    exit 1
  fi
done

required_text=(
  'codex-rs/cli/src/main.rs|mod update;'
  'codex-rs/cli/src/main.rs|update::run(action).await?;'
  'codex-rs/cli/src/update.rs|LATEST_RELEASE_URL'
  'codex-rs/cli/src/update/install.rs|replace_symlink'
)
for requirement in "${required_text[@]}"; do
  path="${requirement%%|*}"
  expected="${requirement#*|}"
  if ! grep -Fq -- "$expected" "$path"; then
    echo "Required fork invariant disappeared: $path must contain '$expected'" >&2
    exit 1
  fi
done

echo 'Fork rebase invariants passed.'
