#!/usr/bin/env bash
# Runs one CI check from a root container (as GitHub's container jobs and the local
# Docker rehearsal do) as an unprivileged user: several tests prove that Portico
# refuses unreadable or unwritable SSH files, which root can always access.
set -euo pipefail

[ $# -eq 1 ] || { echo "usage: $0 <check>" >&2; exit 2; }

if [ "$(id -u)" -eq 0 ]; then
    id ci >/dev/null 2>&1 || useradd --create-home ci
    chown -R ci: "$PWD"
    exec runuser -u ci -- "$(readlink -f "$0")" "$@"
fi

rustup default stable
exec scripts/ci-checks.sh "$1"
