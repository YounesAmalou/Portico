#!/usr/bin/env bash
# Points packaging/aur/PKGBUILD at release v<version>: downloads the tagged
# source archive, pins its SHA-256 and regenerates .SRCINFO. Run after the tag
# exists (the archive does not exist before). Prints the files to publish.
set -euo pipefail

[ $# -eq 1 ] || { echo "usage: $0 <version>" >&2; exit 2; }
version=$1
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "invalid version: $version" >&2; exit 2; }

dir=$(dirname "$(readlink -f "$0")")/../packaging/aur
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

url=$(sed -n "s/^url='\(.*\)'/\1/p" "$dir/PKGBUILD")
curl --fail --silent --show-error --location \
  --output "$work/source.tar.gz" "$url/archive/refs/tags/v$version.tar.gz"
sum=$(sha256sum "$work/source.tar.gz" | cut -d' ' -f1)

sed -i \
  -e "s/^pkgver=.*/pkgver=$version/" \
  -e "s/^pkgrel=.*/pkgrel=1/" \
  -e "s/^sha256sums=.*/sha256sums=('$sum')/" \
  "$dir/PKGBUILD"
(cd "$dir" && makepkg --printsrcinfo > .SRCINFO)
echo "$dir/PKGBUILD"
echo "$dir/.SRCINFO"
