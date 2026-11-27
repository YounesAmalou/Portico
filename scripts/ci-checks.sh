#!/usr/bin/env bash
# Runs one continuous-integration check. CI jobs and local rehearsals call the
# same subcommands so a green local run means a green job.
set -euo pipefail

usage() {
    echo "usage: $0 <fmt|clippy|test|e2e|docs|msrv|audit|deny|package|desktop|tray>" >&2
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

# The end-to-end suite against OpenSSH's server at $SSHD, which the tests read too. Git for
# Windows' bash runs this script on Windows, where the server is Win32-OpenSSH's sshd.exe,
# named the way the tests (native Windows programs) read a path.
e2e() {
    if [ -z "${SSHD:-}" ]; then
        case "$(uname -s)" in
            Linux) SSHD=/usr/bin/sshd ;;
            MINGW* | MSYS* | CYGWIN*) SSHD=C:/Windows/System32/OpenSSH/sshd.exe ;;
            *) SSHD=/usr/sbin/sshd ;;
        esac
    fi
    [ -f "$SSHD" ] || { echo "$SSHD is missing: install OpenSSH's server" >&2; exit 1; }
    export SSHD
    cargo test --test e2e -- --ignored
}

# Runs a command as the leader of a new session (macOS has no setsid(1)). The function's shell
# becomes the command, so a background `in_own_session` leaves the session's ID in `$!`.
in_own_session() {
    exec perl -MPOSIX -e 'POSIX::setsid() != -1 or die "setsid: $!\n"; exec @ARGV or die "exec: $!\n"' "$@"
}

# The test suite. On macOS it runs in a session of its own, and so in a process group of its own.
# The runner's processes share one process group, which the tests otherwise joined: macOS sent
# that group SIGHUP, as it does an orphaned process group, while a test ending a process tree
# held some of its processes stopped, and the runner ended with the tests (the job then timed
# out without a log). The processes launchd adopts from the tests are in another session than
# theirs, so their exits no longer count against the tests' group. The tests write to a file the
# step prints as it grows, and get TEST_LIMIT seconds (300 by default) once built: a hang ends
# the step with the tests' output and a process listing.
tests() {
    if [ "$(uname -s)" != Darwin ]; then
        cargo test --all-targets --no-fail-fast
        return
    fi
    cargo test --all-targets --no-run
    local log status=0 waited=0 limit=${TEST_LIMIT:-300}
    log=$(mktemp)
    in_own_session cargo test --all-targets --no-fail-fast >"$log" 2>&1 </dev/null &
    local tests=$!
    tail -n +1 -f "$log" &
    local tail=$!
    while kill -0 "$tests" 2>/dev/null && [ "$waited" -lt "$limit" ]; do
        sleep 1
        waited=$((waited + 1))
    done
    if kill -0 "$tests" 2>/dev/null; then
        sleep 1
        echo "::error::the tests were still running after $limit seconds; the processes left:"
        ps -U "$(id -u)" -o pid,ppid,pgid,stat,etime,command
        kill -KILL -- "-$tests" 2>/dev/null || true
        status=1
    fi
    wait "$tests" || status=$?
    sleep 1
    kill "$tail" 2>/dev/null || true
    rm -f -- "$log"
    return "$status"
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

# The tray app (desktop/tray, macOS and Windows): its own Cargo project. On Linux it builds
# its pure menu model and the stub main, which the tests and clippy cover.
tray() {
    cd desktop/tray
    cargo fmt --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --locked
}

# macOS's temporary directory is reached through a symbolic link (/var -> /private/var).
# Portico resolves links in the paths it writes, so tests that compare those paths with
# their temporary home need the resolved spelling from the start.
if [ "$(uname -s)" = Darwin ]; then
    TMPDIR=$(cd "${TMPDIR:-/tmp}" && pwd -P)/
    export TMPDIR
fi
# Windows often names the temporary directory by its short 8.3 name (C:\Users\RUNNER~1\...),
# and resolving a path gives the long one, so the tests get the long spelling from the start.
case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*)
        TMP=$(cygpath --windows --long-name "${TMP:-/tmp}")
        TEMP=$TMP
        export TMP TEMP
        ;;
esac

[ $# -eq 1 ] || usage
case "$1" in
    fmt) cargo fmt --check ;;
    clippy) cargo clippy --all-targets --all-features -- -D warnings ;;
    test) tests ;;
    e2e) e2e ;;
    docs) cargo test --test docs ;;
    msrv) msrv ;;
    # Both cover the tray app's own lock file too (deny.toml, read from the
    # repository root, names its macOS and Windows targets).
    audit) ensure_tool audit && cargo audit && cargo audit --file desktop/tray/Cargo.lock ;;
    deny) ensure_tool deny && cargo deny check && cargo deny --manifest-path desktop/tray/Cargo.toml check ;;
    package) cargo package --locked ;;
    desktop) desktop ;;
    tray) tray ;;
    *) usage ;;
esac
