#!/usr/bin/env bash
# Runs one continuous-integration check. CI jobs and local rehearsals call the
# same subcommands so a green local run means a green job.
set -euo pipefail

usage() {
    echo "usage: $0 <fmt|clippy|test|e2e|docs|msrv|audit|deny|package|desktop>" >&2
    exit 2
}

msrv() {
    local version
    version=$(sed -n 's/^rust-version = "\(.*\)"/\1/p' Cargo.toml)
    [ -n "$version" ] || { echo "rust-version is not set in Cargo.toml" >&2; exit 1; }
    rustup toolchain install "$version" --profile minimal
    cargo "+$version" check --all-targets --locked
}

# CI installs cargo-audit and cargo-deny prebuilt from the Arch repositories; a local
# rehearsal without them builds the missing one on first use.
ensure_tool() {
    command -v "cargo-$1" >/dev/null || cargo install --locked "cargo-$1"
}

# The desktop panel (desktop/quickshell): Qt 6's qmllint on every QML file, and
# portico.js against the golden files with Qt Quick Test, in Qt's own
# JavaScript engine. Needs qt6-declarative and quickshell.
desktop() {
    local qt=/usr/lib/qt6/bin dir=desktop/quickshell omarchy=/usr/share/omarchy/shell
    local tool
    for tool in qmllint qmltestrunner; do
        [ -x "$qt/$tool" ] || { echo "$qt/$tool is missing: install qt6-declarative" >&2; exit 1; }
    done
    [ -d /usr/lib/qt6/qml/Quickshell ] || { echo "Quickshell's QML modules are missing: install quickshell" >&2; exit 1; }
    # Quickshell's PanelWindow is creatable although its type information says otherwise.
    "$qt/qmllint" --max-warnings 0 --uncreatable-type disable -I "$dir" \
        "$dir"/shell.qml "$dir"/Portico*.qml "$dir"/tests/tst_portico.qml
    # The Omarchy entry points import qs.Commons and qs.Ui from Omarchy's shell:
    # resolved when Omarchy is installed, otherwise left unresolved. Its theme
    # objects are plain QtObjects whose members qmllint cannot see.
    local imports
    imports=$(mktemp -d)
    trap 'rm -rf -- "$imports"' RETURN
    local lenient=(--max-warnings 0 --uncreatable-type disable --missing-property disable)
    if [ -d "$omarchy/Commons" ]; then
        ln -s "$omarchy" "$imports/qs"
    else
        lenient+=(--import disable --unresolved-type disable --unqualified disable)
    fi
    "$qt/qmllint" "${lenient[@]}" -I "$imports" -I "$dir" "$dir"/Omarchy*.qml
    # Omarchy's own manifest checks, when Omarchy is installed.
    if command -v omarchy-plugin-validate >/dev/null; then
        omarchy-plugin-validate "$dir"
    fi
    QT_QPA_PLATFORM=offscreen QML_XHR_ALLOW_FILE_READ=1 "$qt/qmltestrunner" -input "$dir/tests"
}

[ $# -eq 1 ] || usage
case "$1" in
    fmt) cargo fmt --check ;;
    clippy) cargo clippy --all-targets --all-features -- -D warnings ;;
    test) cargo test --all-targets ;;
    e2e) cargo test --test e2e -- --ignored ;;
    docs) cargo test --test docs ;;
    msrv) msrv ;;
    audit) ensure_tool audit && cargo audit ;;
    deny) ensure_tool deny && cargo deny check ;;
    package) cargo package --locked ;;
    desktop) desktop ;;
    *) usage ;;
esac
