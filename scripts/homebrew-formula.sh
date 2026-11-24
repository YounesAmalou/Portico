#!/usr/bin/env bash
# Prints the Homebrew formula of release <version>: packaging/homebrew/portico.rb with the
# version and the SHA-256 of the two macOS tarballs in <dist> filled in. The release
# workflow pushes the result to the tap.
set -euo pipefail

[ $# -eq 2 ] || { echo "usage: $0 <version> <dist>" >&2; exit 2; }
version=$1
dist=$2

root=$(dirname "$(readlink -f "$0")")/..

checksum() {
    local tarball="$dist/portico-v$version-$1.tar.gz"
    [ -f "$tarball" ] || { echo "missing $tarball" >&2; exit 1; }
    if command -v sha256sum >/dev/null; then
        sha256sum -- "$tarball" | cut -d ' ' -f 1
    else
        shasum -a 256 -- "$tarball" | cut -d ' ' -f 1
    fi
}

aarch64=$(checksum aarch64-apple-darwin)
x86_64=$(checksum x86_64-apple-darwin)
sed -e "s/@VERSION@/$version/g" \
    -e "s/@AARCH64_SHA256@/$aarch64/" \
    -e "s/@X86_64_SHA256@/$x86_64/" \
    "$root/packaging/homebrew/portico.rb" |
    # The template's opening comment is about the template, not the tap's formula.
    awk 'started || !/^#/ { started = 1; print }'
