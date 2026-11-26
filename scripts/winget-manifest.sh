#!/usr/bin/env bash
# Writes the winget manifests of release <version> into <outdir>: the three templates in
# packaging/winget/ with the version and the SHA-256 of the release's two Windows zips
# filled in. The zips must carry the release's names, since the manifests point at the
# release's files of those names. The release workflow submits the result to
# microsoft/winget-pkgs, and a rehearsal validates it with `winget validate`.
set -euo pipefail

[ $# -eq 4 ] || { echo "usage: $0 <version> <x64 zip> <arm64 zip> <outdir>" >&2; exit 2; }
version=$1
x64_zip=$2
arm64_zip=$3
outdir=$4

root=$(dirname "$(readlink -f "$0")")/..

checksum() {
    local zip=$1 expected="portico-v$version-$2.zip"
    [ "$(basename -- "$zip")" = "$expected" ] || { echo "$zip is not $expected" >&2; exit 1; }
    [ -f "$zip" ] || { echo "missing $zip" >&2; exit 1; }
    if command -v sha256sum >/dev/null; then
        sha256sum -- "$zip" | cut -d ' ' -f 1
    else
        shasum -a 256 -- "$zip" | cut -d ' ' -f 1
    fi | tr '[:lower:]' '[:upper:]'
}

x64=$(checksum "$x64_zip" x86_64-pc-windows-msvc)
arm64=$(checksum "$arm64_zip" aarch64-pc-windows-msvc)
mkdir -p "$outdir"
for template in "$root"/packaging/winget/*.yaml; do
    sed -e "s/@VERSION@/$version/g" \
        -e "s/@X64_SHA256@/$x64/" \
        -e "s/@ARM64_SHA256@/$arm64/" \
        "$template" |
        # The templates' opening comment is about the template, not the submitted manifest.
        awk 'started || /^# yaml-language-server:/ { started = 1; print }' \
            > "$outdir/$(basename -- "$template")"
done
