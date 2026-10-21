#!/usr/bin/env bash
# Builds release <version> for <target> (x86_64-unknown-linux-musl or
# aarch64-unknown-linux-musl) into <outdir>: the static binary in a tarball, the .deb and
# .rpm packages, a CycloneDX SBOM of the binary and, for x86_64, the Arch Linux package.
# Prints the binary's path. Needs rustup with the target added, cargo-deb,
# cargo-generate-rpm, cargo-cyclonedx and, for x86_64, makepkg: the release workflow runs
# it in an Arch Linux container, whose repositories package every one of them.
set -euo pipefail

[ $# -eq 3 ] || { echo "usage: $0 <version> <target> <outdir>" >&2; exit 2; }
version=$1
target=$2
outdir=$3

cd "$(dirname "$(readlink -f "$0")")/.."
mkdir -p "$outdir"
outdir=$(readlink -f "$outdir")

# aarch64 is cross-compiled: Rust's bundled linker links a static musl binary without a
# cross toolchain, since no dependency compiles C.
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
cargo build --release --locked --target "$target"
binary=target/$target/release/portico

scripts/package-release.sh "$version" "$target" "$binary" "$outdir" >&2
# Cargo.toml's [package.metadata.deb] and [package.metadata.generate-rpm] list the files.
# The release profile already strips the binary, which a host strip could not do for aarch64.
cargo deb --no-build --no-strip --target "$target" --output "$outdir/" >&2
cargo generate-rpm --target "$target" --output "$outdir/" >&2
cargo cyclonedx --format json --spec-version 1.5 --no-build-deps --target "$target" \
    --override-filename "portico-v$version-$target.cdx"
mv "portico-v$version-$target.cdx.json" "$outdir/"
if [ "$target" = x86_64-unknown-linux-musl ]; then
    scripts/package-arch.sh "$version" "$binary" "$outdir" >&2
fi
readlink -f "$binary"
