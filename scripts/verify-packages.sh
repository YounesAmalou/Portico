#!/usr/bin/env bash
# Installs each release artifact of one architecture in a clean container of its own
# distribution and checks that `portico --version` prints <version>: the tarball in
# Alpine (which has no glibc), the .deb in Debian, the .rpm in Fedora and, for x86_64,
# the Arch Linux package in Arch Linux. <dir> holds the artifacts. The architecture is
# this machine's unless given; another one runs under emulation (binfmt and QEMU), which
# is how an aarch64 rehearsal runs on an x86_64 workstation. Needs Docker.
set -euo pipefail

[ $# -eq 2 ] || [ $# -eq 3 ] || { echo "usage: $0 <version> <dir> [x86_64|aarch64]" >&2; exit 2; }
version=$1
dir=$(readlink -f "$2")
case "${3:-$(uname -m)}" in
    x86_64) target=x86_64-unknown-linux-musl platform=linux/amd64 deb=amd64 rpm=x86_64 ;;
    aarch64) target=aarch64-unknown-linux-musl platform=linux/arm64 deb=arm64 rpm=aarch64 ;;
    *) echo "unsupported architecture ${3:-$(uname -m)}" >&2; exit 2 ;;
esac

# check <image> <artifact> <install command>
check() {
    local image=$1 artifact=$2 install=$3 printed
    [ -f "$dir/$artifact" ] || { echo "$artifact is missing from $dir" >&2; exit 1; }
    echo "Installing $artifact in $image" >&2
    printed=$(docker run --rm --quiet --pull always --platform "$platform" -v "$dir:/dist:ro" "$image" \
        sh -ec "{ $install; } >&2 && portico --version")
    [ "$printed" = "portico $version" ] || {
        echo "$artifact in $image printed '$printed', not 'portico $version'" >&2
        exit 1
    }
}

tarball=portico-v$version-$target
check alpine "$tarball.tar.gz" \
    "tar -xzf /dist/$tarball.tar.gz -C /opt && ln -s /opt/$tarball/portico /usr/local/bin/portico"
check debian:stable-slim "portico_${version}-1_$deb.deb" \
    "apt-get update -q && apt-get install -qy /dist/portico_${version}-1_$deb.deb"
check fedora "portico-$version-1.$rpm.rpm" "dnf install -qy /dist/portico-$version-1.$rpm.rpm"
if [ "$rpm" = x86_64 ]; then
    check archlinux "portico-$version-1-x86_64.pkg.tar.zst" \
        "pacman -Sy --noconfirm && pacman -U --noconfirm /dist/portico-$version-1-x86_64.pkg.tar.zst"
fi
echo "Every $target artifact installs and runs" >&2
