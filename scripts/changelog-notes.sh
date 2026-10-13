#!/usr/bin/env bash
# Prints the changelog notes of one released version: the body of its
# "## [<version>]" section, without the heading or surrounding blank lines.
set -euo pipefail

[ $# -ge 1 ] && [ $# -le 2 ] || { echo "usage: $0 <version> [changelog]" >&2; exit 2; }
version=$1
changelog=${2:-$(dirname "$(readlink -f "$0")")/../CHANGELOG.md}

notes=$(awk -v heading="## [$version]" '
    index($0, heading) == 1 { found = 1; next }
    found && index($0, "## [") == 1 { exit }
    found { print }
    END { if (!found) exit 3 }
' "$changelog") || {
    echo "missing changelog section for $version" >&2
    exit 1
}

# Command substitution already dropped trailing newlines; drop leading blank lines.
printf '%s\n' "$notes" | sed '/./,$!d'
