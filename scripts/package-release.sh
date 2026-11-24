#!/usr/bin/env bash
# Packages a release binary built for <target> as portico-v<version>-<target>.tar.gz with
# the license files and README, in <outdir>. Prints the tarball path. The release
# workflow checksums every artifact together in SHA256SUMS.
set -euo pipefail

[ $# -eq 4 ] || { echo "usage: $0 <version> <target> <binary> <outdir>" >&2; exit 2; }
version=$1
target=$2
binary=$3
outdir=$4

root=$(dirname "$(readlink -f "$0")")/..
name="portico-v${version}-${target}"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mkdir -p "$outdir" "$stage/$name"
install -m 0755 "$binary" "$stage/$name/portico"
install -m 0644 "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$root/README.md" "$stage/$name/"

outdir=$(readlink -f "$outdir")
# GNU tar (Linux) and bsdtar (macOS) spell "owned by root" differently.
if tar --version 2>/dev/null | grep -q GNU; then
    owner=(--owner=0 --group=0)
else
    owner=(--uid 0 --gid 0)
fi
tar -C "$stage" "${owner[@]}" -czf "$outdir/$name.tar.gz" "$name"
echo "$outdir/$name.tar.gz"
