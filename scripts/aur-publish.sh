#!/usr/bin/env bash
# Publishes release <version> to the AUR: points packaging/aur at the tag, proves the
# package builds with makepkg, then pushes PKGBUILD and .SRCINFO to the AUR repository.
# Meant for an Arch container (root is fine: it drops to an unprivileged user, as
# makepkg requires). Pushing again for a version already on the AUR changes nothing.
#
# Needs AUR_SSH_PRIVATE_KEY (a key registered on the maintainer's AUR account). The
# AUR host key is pinned in packaging/aur/known_hosts. AUR_REMOTE overrides the
# repository URL, which is how the script is rehearsed against a local repository.
set -euo pipefail

[ $# -eq 1 ] || { echo "usage: $0 <version>" >&2; exit 2; }
version=$1

if [ "$(id -u)" -eq 0 ]; then
    pacman -Syu --noconfirm --needed rust git openssh
    id aur >/dev/null 2>&1 || useradd --create-home aur
    chown -R aur: "$PWD"
    exec runuser -u aur -- "$(readlink -f "$0")" "$@"
fi

[ -n "${AUR_SSH_PRIVATE_KEY:-}" ] || {
    echo "AUR_SSH_PRIVATE_KEY is not set: add it as a secret of the 'aur' environment" >&2
    exit 1
}
remote=${AUR_REMOTE:-ssh://aur@aur.archlinux.org/portico.git}
root=$PWD
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

install -m 0600 /dev/null "$work/key"
printf '%s\n' "$AUR_SSH_PRIVATE_KEY" > "$work/key"
export GIT_SSH_COMMAND="ssh -i $work/key -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=$root/packaging/aur/known_hosts"

scripts/aur-update.sh "$version" >/dev/null

(
    cd packaging/aur
    mkdir "$work/build" "$work/pkg" "$work/src"
    RUSTUP_TOOLCHAIN=stable BUILDDIR=$work/build PKGDEST=$work/pkg SRCDEST=$work/src \
        makepkg --nodeps --force
)

git clone --quiet "$remote" "$work/aur"
cp packaging/aur/PKGBUILD packaging/aur/.SRCINFO "$work/aur/"
cd "$work/aur"
git add PKGBUILD .SRCINFO
if git diff --cached --quiet; then
    echo "the AUR already has portico $version"
    exit 0
fi
git -c user.name="Younes A. Amalou" -c user.email="YounesAmalou@users.noreply.github.com" \
    commit --quiet -m "Update to $version"
git push --quiet origin HEAD:master
echo "published portico $version to the AUR"
