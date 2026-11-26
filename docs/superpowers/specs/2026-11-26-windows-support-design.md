# Windows Support Design

**Status:** Approved; being implemented

## Purpose

Portico builds on Linux and macOS only: `src/platform/mod.rs` stops every other target with a `compile_error!`. This slice is the Windows support slice of v5 in `ROADMAP.md`: a Windows implementation of the platform layer (`docs/architecture.md`, "Add a platform") on the OpenSSH client that ships with Windows, release builds for x64 and arm64, winget distribution, and every test suite running on Windows in CI. Like macOS, it is another implementation of the layer, not a fork: what Windows shares with the Unix systems stays written once.

Linux and macOS behaviour does not change. No CLI flag, human output or JSON document changes on them.

## Goals

- `portico`, its TUI and every CLI command work on Windows 10 (1809 or later, the first with ConPTY and the bundled OpenSSH client) and Windows 11, on x64 and arm64, with Windows' own `ssh.exe` (Win32-OpenSSH).
- Saved port forwards start, are found again after Portico exits, and stop, without Unix signals, with the same guarantee as on Linux: a recycled process ID is never mistaken for a tunnel.
- Opening a command in a new terminal window uses Windows Terminal.
- Local container listing works with Docker Desktop and Podman Desktop.
- CI runs clippy, the unit and integration tests and the end-to-end suite on Windows on every pull request that changes code.
- The release builds and attests Windows zips, and publishes to winget.

## Non-goals

- WSL distributions as targets. A user who runs Docker Engine inside a WSL distro without Docker Desktop runs Portico inside that distro, as a Linux program.
- Windows containers (Docker Desktop's "Windows containers" mode): their shells are not `sh`. Only Linux containers are supported.
- A Windows desktop interface (system tray app): v5 slice 3.
- Any change to the preferences file's contents or the container shell user picker, which the "Remembered container user" slice is changing at the same time. Only the file's location is decided here (see Paths).

## Anti-goals

- No Windows service or background helper: tunnels stay ordinary `ssh` processes (the "no daemon" rule).
- No second copy of the test suite: Windows runs the same tests, not Windows-specific twins.
- No PuTTY or other client: OpenSSH stays authoritative.

## Settled questions

These were settled with the maintainer in this slice's design review.

- **Docker Desktop is the only Windows-local container engine considered; WSL is not a target.** Docker Desktop runs one engine inside its own WSL distro: Windows' `docker.exe` reaches it over the named pipe `//./pipe/docker_engine`, and every WSL distro with WSL integration enabled reaches the same engine, so `docker ps` lists the same containers from Windows and from those distros. The documentation names the two cases this misses (Docker Engine installed inside a distro without Docker Desktop, and Windows containers mode). Podman Desktop is reached the same way, through `podman.exe`.
- **Every test suite runs on Windows, keeping the existing shell-script fakes.** The integration tests fake `ssh`, `docker` and other programs with `#!/bin/sh` scripts. On Windows, `write_executable(path, script)` writes the script to `<path>.sh` and puts a small test-only launcher at `<path>.exe` that runs `sh <path>.sh <args…>` (the `sh` of Git for Windows, present on GitHub's runners and with any Git install), passing standard input, output, error and the exit code through. A rewrite of every fake in Rust, and PowerShell twins of each script, were rejected: the first churns tests for days and reads worse, the second doubles every fake.
- **Windows' `ssh.exe` ignores `-f`, so a tunnel's readiness is decided per kind.** Win32-OpenSSH has no background mode (PowerShell/Win32-OpenSSH issues 999 and 2209, still open in 9.5): with `-f` it stays in the foreground. Linux and macOS keep `ssh -f`, which backgrounds only once the forward succeeded. On Windows Portico starts `ssh -N` (no `-f`) detached itself, and decides the tunnel is up per kind: a local (`-L`) or dynamic (`-D`) forward is up once its listen address is bound (polled with the same probe that checks a port is free) while `ssh` still runs; a remote (`-R`) forward is up once the `-E` log reports authentication and `ssh` is still running one second later (with `ExitOnForwardFailure=yes`, a refused remote forward ends `ssh` within that time). The log level is raised to `VERBOSE` on Windows only, which is the lowest level that logs the authentication. The timeout, the cancellation and the error text when `ssh` exits are unchanged.
- **The end-to-end server: an unprivileged `sshd.exe` first, the OpenSSH Server service as the fallback.** The first implementation task proves on the CI runner that `sshd.exe` serves the tests as the current user. If it cannot, CI installs and uses the runner's OpenSSH Server service (the runner's user is an administrator), with each test still getting its own port, keys and configuration; the suite on developer machines then documents the same requirement.
- **x64 and arm64, published to winget by the release.** The release builds `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc` zips. A `winget` job (tags only) submits the new version's manifests to `microsoft/winget-pkgs` with a `WINGET_TOKEN` stored in a `winget` environment; while the token is missing the job does nothing, as the Homebrew job does without its token.

## Approaches considered for the tests

| Approach | Trade-off | Outcome |
|---|---|---|
| Launcher shim beside each script, scripts kept | needs Git for Windows' `sh` when testing; every fake stays as it is | chosen |
| One Rust fake binary driven by a spec | no `sh` needed; rewrites ~55 fakes, harder to read, conflicts with parallel work | rejected |
| A PowerShell twin per fake | two copies drift apart | rejected |

## Decisions

### Platform layout

```
src/platform/
  mod.rs        cfg selection; compile_error! for any target other than Linux, macOS and Windows
  fs.rs, paths.rs, process.rs, launcher.rs   re-export `unix` + the system's module, or `windows`
  unix/         unchanged (compiled on Unix only)
  linux/, macos/  unchanged
  windows/      fs, paths, process, launcher
```

`unix` becomes `#[cfg(unix)]`. On Windows each facade re-exports `windows` alone, which provides every item the rest of the crate uses from `fs`, `paths`, `process` and `launcher`, with the same names and signatures where the concept exists. The shared algorithms in `unix/process.rs` (the stop-then-kill tree walk, `stop_process`) are not reused: Windows has no stop signal, so its versions are simpler (below). Items that exist only to serve Unix (`terminate_process_group`, process groups in `connect`) get a Windows counterpart with the same name and the Windows meaning, so callers outside the platform layer need no `cfg`. Where a caller needs Unix-only data (an `ExitStatus` signal), the platform layer gains a small function (`process::exit_code`) both systems implement.

System calls go through the `windows-sys` crate, a Windows-only dependency (`[target.'cfg(windows)'.dependencies]`), used only inside `src/platform/windows/`. `libc` moves to `[target.'cfg(unix)'.dependencies]`.

### Process control on Windows

- **Pin.** `OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE)`. An open handle keeps its process object alive and its ID unassigned, so it pins identity like a pidfd: after the process exits, the handle reports the exit and the ID cannot be reused while the handle is open. `signal` (terminate) checks `WaitForSingleObject(handle, 0)` first and returns "gone" if the process already exited; `wait_exit(ms)` waits on the handle. The fault hook `fail_pin_after` keeps working, so the cancellation tests run on Windows too.
- **Stopping a tunnel.** Windows' `ssh.exe` has no graceful-shutdown signal Portico can send to a detached process, and a tunnel holds no state that needs flushing, so stopping is `TerminateProcess` on the pinned handle, after the identity check. `stop_process` keeps its contract (the process is gone when it returns `Ok`).
- **Process trees** (`terminate_process_tree`, used when a connection test is cancelled): the children are found with `CreateToolhelp32Snapshot` (parent IDs), each is pinned and its creation time compared with the parent's before it is terminated, so a reused ID whose process started after the parent is never touched. Children are terminated before the parent.
- **Process groups** (`process_group(0)` and `terminate_process_group` in `domain::openssh::connect`): on Windows the child is created with `CREATE_NEW_PROCESS_GROUP` (out of reach of the console's Ctrl+C, as the Unix process group is) and `terminate_process_group` terminates its tree. The `CommandExt` calls move behind a platform function (`process::own_group(&mut Command)`).
- **`Proc`** (recognising a started tunnel): `pids()` from the Toolhelp snapshot; `stat` gives the state (running, or exited for a handle that is signalled) and the start time from `GetProcessTimes` (100 ns intervals since 1601, stored as is in the record's `start`; `started_at` converts it to Unix seconds); `uid` is replaced by the owner's SID string from `GetTokenInformation(TokenUser)`, compared with the current user's (the record's `uid` field holds that string on Windows; records never move between systems); `cmdline` reads the command line through `NtQueryInformationProcess(ProcessCommandLineInformation)` and splits it with Windows' rules (`CommandLineToArgvW`); `boot_id` is derived from the boot time (now minus `GetTickCount64`, rounded to the second, which is stable across one boot). `has_exited` checks the handle without reaping.
- **User identity.** `Proc::uid`, `current_uid` and the `uid` parameters of `is_tunnel`, `find_tunnel` and `identify` take a platform type `process::Uid`: `u32` on Unix, the SID string on Windows. Records do not store the user, and `start` stays a `u64` (a FILETIME fits), so the record file format does not change.
- **Orphans.** No Job Object: it would end the tunnel with Portico, which the no-daemon rule's "keeps running after Portico exits" forbids. A `ProxyCommand` child whose parent already exited is cleaned up on a best-effort basis by the tree walk (creation time later than the parent's start).
- **Starting a tunnel** (`domain::tunnel::runtime`, today `setsid` in `pre_exec`): on Windows `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW`, with standard handles not inherited, so the `ssh` outlives Portico and its console. This also goes through a platform function (`process::detach(&mut Command)`).
- **Exit codes** (`cli::exit_code`): on Windows the exit code is used as is (there is no signal); a code above 255 becomes failure.
- **Ctrl+C during `portico test`**: `signal-hook` handles SIGINT and SIGTERM on Windows through the C runtime's `signal`, which the console's Ctrl+C and Ctrl+Break reach; SIGHUP does not exist there and is registered on Unix only. Closing the console window ends Portico as Windows ends it.

### The TUI

- **Hang-up watcher** (`tui::hangup`): the crossterm loop it works around reads a Unix tty; on Windows crossterm reads console input events, and a closed console ends the process (`CTRL_CLOSE_EVENT`). The watcher is Unix-only; on Windows `next_event` reads crossterm directly and an error from the console is treated as `Closed`.
- **Waiting for Enter** (`wait_for_enter` in `tui::mod`, today `poll` on stdin): on Windows it reads the console input with crossterm's event poll (key events), which honours the same idle tick and termination flag.
- **Local time** (`localtime_r` in `tui::mod` and `domain::tunnel`): a platform function `platform::time::local_offset_and_fields` (or the `localtime_s` of the C runtime on Windows) replaces the two direct calls; the output format does not change.
- crossterm, ratatui and the raw-mode handling already support Windows consoles and Windows Terminal.

### Terminal launcher on Windows

`platform::launcher::launch(launcher, command)` keeps its signature and outcomes. On Windows, `DEFAULT_LAUNCHER` is `wt`:

| Choice | How it opens |
|---|---|
| `PORTICO_TERMINAL` set to a path to an executable | run with the command as its arguments, as on the other systems |
| `PORTICO_TERMINAL=cmd`, or `wt.exe` not found on `PATH` (it is an App Execution Alias under `%LOCALAPPDATA%\Microsoft\WindowsApps`) | a new console window: `CreateProcess` of the command with `CREATE_NEW_CONSOLE` |
| otherwise | `wt.exe -w new -- <command…>` (a new Windows Terminal window) |

The command line is built with Windows quoting rules (`std::process::Command` already applies them to arguments). Any `launcher` other than `DEFAULT_LAUNCHER` is run with the command as its arguments, which is how the tests use a fake launcher.

### Windows paths

- **Home:** `%USERPROFILE%`, which Win32-OpenSSH uses too. `HOME` is ignored on Windows, since Git Bash, MSYS and Cygwin set it to other places; the tests point `USERPROFILE` at their temporary home. `~` display and expansion accept both `\` and `/` after `~`.
- **SSH directory:** `~\.ssh`, which Windows' OpenSSH uses.
- **System `ssh_config`:** `%ProgramData%\ssh\ssh_config` (`C:\ProgramData\ssh\ssh_config` when the variable is missing). `SYSTEM_SSH_CONFIG` becomes a function on Windows (`system_ssh_config()`), with a constant kept on Unix through the same function.
- **Preferences file:** `%APPDATA%\portico\settings.toml`; `XDG_CONFIG_HOME` still wins when it is set (which the tests use). Only `config_dir` changes; the settings code itself is untouched.
- **Runtime directory:** `~\.ssh\portico\run\tunnels`, as on macOS.
- **Paths written into OpenSSH files** (the managed file's `Include`, `IdentityFile`): written with forward slashes and `~` where they are under the home, which Win32-OpenSSH reads, so a file stays readable by `ssh -G` on Windows. Paths Portico shows to people use `\`.

### Files

- **Locking:** `fs2` already locks on Windows (`LockFileEx`).
- **Atomic replace:** `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)` (what `std::fs::rename` does, plus write-through). A target held open without `FILE_SHARE_DELETE` makes the rename fail with "access denied"; it is retried for up to one second before the error is reported.
- **Directory sync:** a no-op on Windows (NTFS commits the rename's metadata with write-through).
- **File identity (`Stamp`):** volume serial number and file index from `GetFileInformationByHandle`, size, last-write time and change time (`FILE_BASIC_INFO.ChangeTime`).
- **Writing through symlinks** (settings and managed files): symlinks are followed the same way; creating them in the tests needs Developer Mode or the right privilege, which GitHub's runners have (the runner's user is an administrator).
- **Permissions:** OpenSSH on Windows checks ACLs, not mode bits. Files Portico creates inherit the ACL of `~\.ssh`, which Windows' OpenSSH already accepts. Mode bits Portico sets on Unix (`0600`, `0700`) are skipped on Windows.
- **`cloexec_socket` and `nonblocking_pipe`:** Windows handles are not inherited unless asked for; `cloexec_socket` is a plain socket there, and `nonblocking_pipe` is Unix-only along with the hang-up watcher.

### `libc` outside the platform layer

- `bind_probe` (`domain::tunnel::runtime`): on Windows the probe binds through `std::net::TcpListener` with `SO_EXCLUSIVEADDRUSE` (set through `windows-sys` in the platform layer), which refuses an address in use by anyone; the Unix probe does not change. The `setsockopt`/`bind` code moves into `platform::fs` (now `platform::net::bind_probe`), so `domain` no longer calls `libc` for it.
- `domain::adopt` (`access(W_OK)`, `geteuid`, errno values): moved behind `platform::fs::writable(path)` and `platform::process::is_superuser()` (on Windows, `writable` tries to open the file for writing without truncating it; `is_superuser` is false, since an elevated administrator is still subject to ACLs).
- `app::identities` (`getuid` for the agent socket's owner): on Windows the agent is the OpenSSH Authentication Agent service on the named pipe `\\.\pipe\openssh-ssh-agent`; the ownership check is skipped there, as Windows' named pipe ACL already limits it to the user.
- `docs/architecture.md`'s list of `libc` calls in `domain` shrinks accordingly.

### Containers on Windows

- A local target runs `docker.exe` or `podman.exe` found on `PATH`, as on Unix. Docker's access decision (`docker` group, `sg`, socket group) does not apply to a local Windows target: there the probe is skipped and the decision is "no offer". Remote targets are Linux hosts and behave as today.
- Opening a container shell from Windows uses `docker exec -it` in the console; ConPTY gives it a terminal.

### Desktop integration

`portico desktop` and the TUI settings overlay's desktop rows behave as on macOS: hidden, refused with "needs Linux (Omarchy)", and the panel refresh in `main` skipped.

### Tests

- **Fake executables:** `tests/support/executable.rs` gains the Windows branch described in Settled questions. The launcher is a tiny program (`sh_shim`) that is never part of the installed binary or the published crate: an example target excluded from the package, which `cargo test --all-targets` builds, or a file the tests compile with `rustc` on first use; the builder picks whichever is reliable. It is copied next to each script. Scripts that only need POSIX `sh`, `echo`, `cat`, `printf`, `sleep` and argument recording keep working unchanged under Git for Windows' `sh`. Tests that prove Unix-only behaviour (permission bits, signals, process groups, `/proc`, `F_SETLEASE`, Docker's group logic) are `#[cfg(unix)]`, each paired with a Windows test where Windows has the behaviour (for example ACL-denied writes are covered by a Windows test that removes write access with `icacls`).
- **End-to-end suite:** the same tests. The pseudo-terminal harness uses `portable-pty`, which uses ConPTY on Windows; the parts of `tests/e2e/terminal.rs` that call `python3`'s `pty` are replaced by `portable-pty` on every system (or kept Unix-only where the test is about Unix behaviour, such as the hang-up tests). The throwaway server is Windows' `sshd.exe` (`C:\Windows\System32\OpenSSH\sshd.exe` after enabling the OpenSSH Server capability, or the Win32-OpenSSH release zip), started as the current user on `127.0.0.1` with its own configuration, host key and `authorized_keys`, as on Unix. If `sshd.exe` turns out unable to serve the current user without running as a service, implementation stops and the maintainer chooses the fallback; this is the slice's main risk.
- **Line endings:** a `.gitattributes` with `* text=auto eol=lf` keeps scripts and fixtures LF on Windows checkouts. Recorded arguments are compared as Portico passed them; tests that compare paths build the expected path with `Path::join` rather than writing `/`.
- **Order of work:** the first task proves the end-to-end server on the Windows runner (see Settled questions). Locally, `cargo check --target x86_64-pc-windows-gnu` (std only, no linker) finds the remaining Unix-only code before CI does.
- **`ci-checks.sh`** runs under Git for Windows' `bash` unchanged where it can; Windows-only differences (the `sshd` path) are variables.

### Build, CI and release

- `ci.yml` gains a `windows` job on `windows-latest`, run when the `rust` filter is set: a matrix of `scripts/ci-checks.sh clippy`, `test` and `e2e` with `fail-fast: false`, each limited to 30 minutes and its check to 20. It runs as the runner's user. It installs only the OpenSSH server (for `e2e`). It is not added to branch protection by this slice; the maintainer adds it once it has proven stable (maintainer step).
- `release.yml` gains `build-windows` on `windows-latest`: `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc` (cross-compiled on the same runner), a zip per target with `portico.exe`, the licences and the README, and a CycloneDX SBOM, attested like the others; the x64 binary's `--version` is checked first. `checksums` waits for it. The binaries are not code-signed (Authenticode signing needs a certificate; a later item).
- A `winget` job after `release`, tags only, renders the three winget manifests (version, installer, default locale) from templates in `packaging/winget/` with the zips' URLs and SHA-256 (`scripts/winget-manifest.sh`), and opens a pull request to `microsoft/winget-pkgs` with the `winget` environment's `WINGET_TOKEN` (a classic token with `public_repo` on the maintainer's account, which forks `winget-pkgs`). It does nothing while the token is missing. A release rehearsal runs `winget validate` on the rendered manifests on the Windows runner. The package identifier is `YounesAmalou.Portico`, installer type `zip` with a nested portable `portico.exe` and the command alias `portico`.

### Maintainer steps (not done by this slice)

- Fork `microsoft/winget-pkgs` under the maintainer's account; create a classic token with `public_repo`; store it as `WINGET_TOKEN` in a new `winget` environment restricted to `v*` tags.
- The first submission to winget is reviewed by Microsoft's moderators; later versions are usually merged automatically.
- Add the three `windows` checks to branch protection once they have been stable.
- Optional, later: an Authenticode certificate to sign the binaries.

## Left to the builder

- The exact split of `windows/` into files, and helper names, as long as the facades export what callers use.
- Whether a Windows item that has no meaning returns a fixed value or is compiled out, case by case, preferring no `cfg` outside `platform`.
- Which individual tests need `cfg(unix)`, recorded in the plan as they are found.
