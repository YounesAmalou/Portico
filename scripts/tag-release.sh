#!/usr/bin/env bash
# Tags the current commit as a release and pushes the tag, after checking that
# it is the tip of origin/main, that Cargo.toml carries the version and that
# CHANGELOG.md has its section. Tags are immutable, so every mistake caught
# here is one that cannot be made.
set -euo pipefail

[ $# -eq 1 ] || { echo "usage: $0 <X.Y.Z>" >&2; exit 2; }
version=$1
cd "$(dirname "$(readlink -f "$0")")/.."

git fetch --quiet origin main
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] \
    || { echo "HEAD is not the tip of origin/main; run: git checkout main && git pull" >&2; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "working tree is not clean" >&2; exit 1; }
cargo_version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
[ "$cargo_version" = "$version" ] \
    || { echo "Cargo.toml is at $cargo_version, not $version; merge the version bump first" >&2; exit 1; }
scripts/changelog-notes.sh "$version" >/dev/null
if git ls-remote --exit-code --tags origin "refs/tags/v$version" >/dev/null 2>&1; then
    echo "tag v$version already exists on origin" >&2
    exit 1
fi

git tag "v$version"
git push origin "v$version"
