#!/usr/bin/env bash
# Packages a release binary as portico-v<version>-x86_64-unknown-linux-gnu.tar.gz
# with the license files and README, plus a SHA-256 checksum file, in <outdir>.
# Prints the tarball path.
set -euo pipefail

[ $# -eq 3 ] || { echo "usage: $0 <version> <binary> <outdir>" >&2; exit 2; }
version=$1
binary=$2
outdir=$3

root=$(dirname "$(readlink -f "$0")")/..
name="portico-v${version}-x86_64-unknown-linux-gnu"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mkdir -p "$outdir" "$stage/$name"
install -m 0755 "$binary" "$stage/$name/portico"
install -m 0644 "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$root/README.md" "$stage/$name/"

outdir=$(readlink -f "$outdir")
tar -C "$stage" --owner=0 --group=0 -czf "$outdir/$name.tar.gz" "$name"
(cd "$outdir" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "$outdir/$name.tar.gz"
