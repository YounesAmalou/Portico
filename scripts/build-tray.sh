#!/usr/bin/env bash
# Builds the tray app (desktop/tray) of release <version> for <target> into <outdir> and prints
# the archive's path; its build log goes to stderr. `portico desktop install` downloads it from
# the release of the same version and checks the layout below.
#
# macOS (aarch64-apple-darwin or x86_64-apple-darwin): portico-tray-v<version>-<target>.tar.gz
# with `Portico Tray.app` at its root, built from macos/Info.plist with the version filled in
# and signed ad hoc (codesign -s -). Needs rustup with the target added and Xcode's tools.
#
# Windows (x86_64-pc-windows-msvc or aarch64-pc-windows-msvc):
# portico-tray-v<version>-<target>.zip with portico-tray.exe at its root. Needs rustup with
# the target added, Visual Studio (its rc.exe embeds the icon) and Git for Windows' bash.
#
# The release workflow builds it before `portico` of the same target, whose build bakes the
# archive's SHA-256 in through PORTICO_TRAY_SHA256.
set -euo pipefail

[ $# -eq 3 ] || { echo "usage: $0 <version> <target> <outdir>" >&2; exit 2; }
version=$1
target=$2
outdir=$3

case "$target" in
    *-apple-darwin | *-windows-*) ;;
    *) echo "macOS or Windows target required, not $target" >&2; exit 2 ;;
esac
# The app compares its version with `portico --version`'s exactly, and it fills the plist.
[[ "$version" =~ ^[0-9][0-9A-Za-z.+-]*$ ]] ||
    { echo "version must be like 1.2.3 (no v prefix), not $version" >&2; exit 2; }

mkdir -p "$outdir"
outdir=$(readlink -f "$outdir")
root=$(dirname "$(readlink -f "$0")")/..
tray=$root/desktop/tray
name="portico-tray-v${version}-${target}"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

# Its own Cargo project, with its own lock file and target directory.
target_dir=${CARGO_TARGET_DIR:-$tray/target}
PORTICO_TRAY_VERSION=$version cargo build --locked --release --target "$target" \
    --manifest-path "$tray/Cargo.toml" --target-dir "$target_dir" >&2
binary=$target_dir/$target/release/portico-tray

case "$target" in
    *-windows-*)
        install -m 0755 "$binary.exe" "$stage/portico-tray.exe"
        # libarchive's bsdtar writes zips: Windows ships it as tar.exe, which Git for
        # Windows' GNU tar shadows in bash.
        if command -v bsdtar >/dev/null; then
            zipper=bsdtar
        else
            zipper="${SYSTEMROOT:-C:/Windows}/System32/tar.exe"
        fi
        rm -f "$outdir/$name.zip"
        (cd "$stage" && "$zipper" --format zip -cf "$outdir/$name.zip" portico-tray.exe)
        echo "$outdir/$name.zip"
        exit 0
        ;;
esac

app="$stage/Portico Tray.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m 0755 "$binary" "$app/Contents/MacOS/portico-tray"
sed "s/@VERSION@/$version/g" "$tray/macos/Info.plist" > "$app/Contents/Info.plist"
install -m 0644 "$tray/assets/portico.icns" "$app/Contents/Resources/portico.icns"
codesign -s - --force --deep "$app" >&2

# GNU tar (Linux) and bsdtar (macOS) spell "owned by root" differently. COPYFILE_DISABLE keeps
# macOS's tar from adding `._` entries for extended attributes, which would sit beside the
# bundle at the archive's root: `portico desktop install` refuses that.
if tar --version 2>/dev/null | grep -q GNU; then
    owner=(--owner=0 --group=0)
else
    owner=(--uid 0 --gid 0)
fi
COPYFILE_DISABLE=1 tar -C "$stage" "${owner[@]}" -czf "$outdir/$name.tar.gz" "Portico Tray.app"
listing=$(tar -tzf "$outdir/$name.tar.gz")
if grep -v '^Portico Tray\.app\(/\|$\)' <<< "$listing" >&2; then
    echo "$outdir/$name.tar.gz holds more than Portico Tray.app" >&2
    exit 1
fi
echo "$outdir/$name.tar.gz"
