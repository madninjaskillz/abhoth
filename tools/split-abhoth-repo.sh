#!/usr/bin/env bash
# Splits Abhoth out of the openorca repo into a fresh repo with its history (CUTOVER.md, D131).
#
#   rust/tools/split-abhoth-repo.sh <openorca-repo> <out-dir> [branch] [--build]
#
# <openorca-repo>  a local clone of openorca (path or URL); it is cloned, never changed.
# <out-dir>        must not exist; the split repo is created there.
# [branch]         the branch to split (default feature/rust-port; at cutover: main).
# --build          also run fmt, clippy and the workspace tests in the split repo.
#
# Layout of the result: rust/* at the root (Cargo workspace, crates/, crosscheck/, assets/,
# docs), the React client at client/ (src/OpenOrca.WebUi/client with its history). Nothing is
# pushed: add the remote and push by hand at cutover.
#
# Needs git-filter-repo (`python -m pip install --user git-filter-repo`).
set -euo pipefail

src=${1:?usage: split-abhoth-repo.sh <openorca-repo> <out-dir> [branch] [--build]}
out=${2:?usage: split-abhoth-repo.sh <openorca-repo> <out-dir> [branch] [--build]}
branch=feature/rust-port
build=0
for a in "${@:3}"; do
  case $a in
    --build) build=1 ;;
    *) branch=$a ;;
  esac
done

if git filter-repo --version >/dev/null 2>&1; then
  filter_repo=(git filter-repo)
elif python -m git_filter_repo --version >/dev/null 2>&1; then
  filter_repo=(python -m git_filter_repo)
else
  echo "git-filter-repo is missing: python -m pip install --user git-filter-repo" >&2
  exit 1
fi
[ -e "$out" ] && { echo "$out already exists" >&2; exit 1; }

echo "== cloning $src ($branch)"
git clone --quiet --no-local --no-tags --single-branch --branch "$branch" "$src" "$out"
cd "$out"

echo "== filtering history (rust/ -> /, src/OpenOrca.WebUi/client/ -> client/)"
"${filter_repo[@]}" --force --quiet \
  --path rust/ --path src/OpenOrca.WebUi/client/ \
  --path-rename rust/: \
  --path-rename src/OpenOrca.WebUi/client/:client/
git branch -m "$branch" main
git gc --quiet --prune=now

echo "== checks"
fail=0
[ -f Cargo.toml ] && [ -d crates ] && [ -f client/dist/index.html ] || { echo "layout: unexpected tree"; fail=1; }
commits=$(git rev-list --count HEAD)
echo "commits: $commits"
# No Anthropic key anywhere in the split history (every commit's diff, all refs).
key_prefix="sk-""ant"   # split so this file never matches itself
hits=$(git log -p --all | grep -c "$key_prefix" || true)
echo "Anthropic key prefix in history: $hits"
[ "$hits" = 0 ] || fail=1
# Nothing may point back into the openorca tree any more.
outside=$(git grep -nE '\.\./\.\./\.\./(src|tests)/OpenOrca' -- '*.rs' '*.toml' | grep -v 'build.rs' || true)
[ -z "$outside" ] || { echo "paths into the openorca tree:"; echo "$outside"; fail=1; }

if [ "$build" = 1 ]; then
  echo "== build and tests"
  cargo fmt --all --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo test --workspace --all-features --no-fail-fast
fi

[ "$fail" = 0 ] && echo "SPLIT OK: $out (branch main, $commits commits)" || { echo "SPLIT CHECKS FAILED"; exit 1; }
