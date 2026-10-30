#!/usr/bin/env bash
# Release-only early green for CI's heavy jobs (#880).
#
# `/merge` pushes one commit that only bumps the version and adds a
# CHANGELOG section. That changes the head SHA, so every check runs again
# for a commit that cannot change any result. A job that runs this script
# first can skip its remaining steps (and still report green, which the
# merge queue needs) when the push since the previous head is such a
# commit AND the same job passed on that previous head.
#
# Two modes:
#
#   tools/ci_release_only.sh diff <before> <head>
#       exit 0 iff before..head only changes the release version lines
#       (Cargo.toml's [package] version, db-core's Cargo.lock entry, the
#       VERSION const in src/lib.rs) and CHANGELOG.md; exit 1 otherwise.
#       Needs both commits in the local object database.
#
#   tools/ci_release_only.sh ci <job-name>
#       the CI step: reads EVENT, BEFORE, HEAD, REPO and GH_TOKEN from the
#       environment, fetches the two commits, and writes `skip=true` or
#       `skip=false` to $GITHUB_OUTPUT. It fails open: anything unexpected
#       (not a pull_request push, no BEFORE, a fetch or API error, the job
#       not green on BEFORE) is `skip=false`, a full run.
#
# merge_group and push events never skip, so the merge queue still tests
# every merge commit in full.
set -euo pipefail

# The files a release commit may touch. Anything else is a full run.
CODE_FILES="Cargo.toml Cargo.lock src/lib.rs"
ALLOWED='^(Cargo\.toml|Cargo\.lock|src/lib\.rs|CHANGELOG\.md)$'

# `file` at `rev` with the release version replaced by a placeholder, so two
# revisions of a file compare equal exactly when only the version differs.
normalized() {
  local rev=$1 file=$2
  case "$file" in
    Cargo.toml)
      # The first `version =` line is the [package] one.
      git show "$rev:$file" | perl -pe 'BEGIN{$d=0} if(!$d && s/^version = "[^"]*"/version = "X"/){$d=1}'
      ;;
    Cargo.lock)
      git show "$rev:$file" | perl -0pe 's/(name = "db-core"\nversion = )"[^"]*"/$1"X"/'
      ;;
    src/lib.rs)
      git show "$rev:$file" | perl -pe 's/^(pub const VERSION: &str = )"[^"]*";/$1"X";/'
      ;;
  esac
}

release_only() {
  local before=$1 head=$2 files bad f
  git cat-file -e "$before^{commit}" 2>/dev/null || return 1
  git cat-file -e "$head^{commit}" 2>/dev/null || return 1
  files=$(git diff --name-only "$before" "$head")
  # An empty diff is not a release: run in full rather than reason about it.
  [ -n "$files" ] || return 1
  bad=$(printf '%s\n' "$files" | grep -vE "$ALLOWED" || true)
  [ -z "$bad" ] || return 1
  for f in $CODE_FILES; do
    printf '%s\n' "$files" | grep -qx "$f" || continue
    cmp -s <(normalized "$before" "$f") <(normalized "$head" "$f") || return 1
  done
  return 0
}

# Whether `job` has a successful check run on `sha`.
job_passed() {
  local sha=$1 job=$2 conclusions
  conclusions=$(gh api "repos/$REPO/commits/$sha/check-runs" --paginate \
    --jq ".check_runs[] | select(.name == \"$job\") | .conclusion") || return 1
  # Any non-success run of that name (cancelled, failed, a rerun) is a full run.
  [ -n "$conclusions" ] && ! printf '%s\n' "$conclusions" | grep -qvx success
}

emit() {
  echo "ci_release_only: skip=$1 ($2)"
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    echo "skip=$1" >> "$GITHUB_OUTPUT"
  fi
}

case "${1:-}" in
  diff)
    release_only "${2:?before}" "${3:?head}"
    ;;
  ci)
    job=${2:?job name}
    if [ "${EVENT:-}" != "pull_request" ]; then
      emit false "event ${EVENT:-unset} always runs in full"; exit 0
    fi
    if [ -z "${BEFORE:-}" ] || [ -z "${HEAD:-}" ]; then
      emit false "no previous head (opened or reopened)"; exit 0
    fi
    if ! git fetch --no-tags --depth=1 origin "$BEFORE" "$HEAD" 2>/dev/null; then
      emit false "could not fetch the previous head (force push?)"; exit 0
    fi
    if ! release_only "$BEFORE" "$HEAD"; then
      emit false "the push changes more than the release version and CHANGELOG"; exit 0
    fi
    if ! job_passed "$BEFORE" "$job"; then
      emit false "$job was not green on the previous head"; exit 0
    fi
    emit true "release-only push, $job green on ${BEFORE:0:8}"
    ;;
  *)
    echo "usage: $0 diff <before> <head> | ci <job-name>" >&2
    exit 2
    ;;
esac
