#!/usr/bin/env bash
# Packages a release binary built for <target> with the license files and README, in
# <outdir>, and prints the archive's path: portico-v<version>-<target>.tar.gz with the files
# in a directory of that name, or for Windows portico-v<version>-<target>.zip with portico.exe
# and the other files at its root, where winget's manifest finds portico.exe. The release
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

mkdir -p "$outdir"
outdir=$(readlink -f "$outdir")

case "$target" in
    *-windows-*)
        install -m 0755 "$binary" "$stage/portico.exe"
        install -m 0644 "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$root/README.md" "$stage/"
        # libarchive's bsdtar writes zips: Windows ships it as tar.exe, which Git for
        # Windows' GNU tar shadows in bash.
        if command -v bsdtar >/dev/null; then
            zipper=bsdtar
        else
            zipper="${SYSTEMROOT:-C:/Windows}/System32/tar.exe"
        fi
        (cd "$stage" && "$zipper" --format zip -cf "$outdir/$name.zip" \
            portico.exe LICENSE-MIT LICENSE-APACHE README.md)
        echo "$outdir/$name.zip"
        exit 0
        ;;
esac

mkdir -p "$stage/$name"
install -m 0755 "$binary" "$stage/$name/portico"
install -m 0644 "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$root/README.md" "$stage/$name/"

# GNU tar (Linux) and bsdtar (macOS) spell "owned by root" differently.
if tar --version 2>/dev/null | grep -q GNU; then
    owner=(--owner=0 --group=0)
else
    owner=(--uid 0 --gid 0)
fi
tar -C "$stage" "${owner[@]}" -czf "$outdir/$name.tar.gz" "$name"
echo "$outdir/$name.tar.gz"
