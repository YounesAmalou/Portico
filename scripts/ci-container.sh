#!/usr/bin/env bash
# Runs one CI check from a root container (as GitHub's container jobs and the local
# Docker rehearsal do) as an unprivileged user: several tests prove that Portico
# refuses unreadable or unwritable SSH files, which root can always access.
set -euo pipefail

[ $# -eq 1 ] || { echo "usage: $0 <check>" >&2; exit 2; }

if [ "$(id -u)" -eq 0 ]; then
    id ci >/dev/null 2>&1 || useradd --create-home ci
    # The checkout and any shared cargo and rustup homes (CI restores its build cache
    # into them as root) must belong to the user who builds.
    for dir in "$PWD" ${CARGO_HOME:+"$CARGO_HOME"} ${RUSTUP_HOME:+"$RUSTUP_HOME"}; do
        mkdir -p "$dir"
        chown -R ci: "$dir"
    done
    exec runuser -u ci -- "$(readlink -f "$0")" "$@"
fi

rustup default stable
exec scripts/ci-checks.sh "$1"
