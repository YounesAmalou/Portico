#!/usr/bin/env bash
# Builds the Arch Linux package of release <version> from a prebuilt x86_64 binary with
# makepkg and packaging/arch/PKGBUILD, into <outdir>. Prints the package path. Run as
# root (as in the release workflow's container), it hands makepkg to an unprivileged
# user, as makepkg requires.
set -euo pipefail

[ $# -eq 3 ] || { echo "usage: $0 <version> <binary> <outdir>" >&2; exit 2; }
version=$1
binary=$2
outdir=$3

root=$(dirname "$(readlink -f "$0")")/..
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

install -m 0644 "$root/packaging/arch/PKGBUILD" "$root/LICENSE-MIT" "$root/LICENSE-APACHE" \
    "$root/README.md" "$root/CHANGELOG.md" "$root/packaging/desktop/portico.desktop" \
    "$root/packaging/desktop/portico.svg" "$stage/"
install -m 0755 "$binary" "$stage/portico"
sed -i "s/^pkgver=.*/pkgver=$version/" "$stage/PKGBUILD"

# The package only copies files: no build dependencies to install, no checksums to
# compare for files staged just above.
build=(env PKGDEST="$stage/out" makepkg --nodeps --skipinteg)
if [ "$(id -u)" -eq 0 ]; then
    id pkg >/dev/null 2>&1 || useradd --create-home pkg
    chown -R pkg: "$stage"
    build=(runuser -u pkg -- "${build[@]}")
fi
(cd "$stage" && "${build[@]}" >&2)

mkdir -p "$outdir"
outdir=$(readlink -f "$outdir")
package=$(find "$stage/out" -name "portico-$version-*.pkg.tar.zst")
install -m 0644 "$package" "$outdir/"
echo "$outdir/$(basename "$package")"
