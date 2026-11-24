# macOS Support Design

**Status:** Approved; being implemented

## Purpose

Portico builds only on Linux: `src/platform/mod.rs` stops every other target with a `compile_error!`. This slice is the macOS support slice of v5 in `ROADMAP.md`: a macOS implementation of the platform layer (`docs/architecture.md`, "Add a platform"), release builds for Apple silicon and Intel, a Homebrew formula, and the test suites running on macOS in CI. It is a second implementation of the layer, not a fork of the Linux code: what both systems share is written once.

Linux behaviour does not change. No CLI flag, human output or JSON document changes on Linux.

## Settled questions

These were settled with the maintainer in this slice's design review.

- **A recycled PID is never mistaken for a tunnel: kqueue plus start time.** macOS has no pidfd. Before Portico checks that a PID is still the tunnel it recorded, it registers a kqueue `EVFILT_PROC`/`NOTE_EXIT` watch on that PID. The watch is attached to the process, not to the number: if that process exits, the watch fires, even if the PID is reused afterwards. Portico checks the watch before each signal and never signals once it has fired. The record keeps the process's start time (microseconds since the epoch, from `sysctl`), its user and its `-E <log>` arguments, as on Linux. What is left is the instant between the last check of the watch and `kill(2)`, in which the process would have to exit and macOS would have to hand its PID to a new process of the same user; PIDs are allocated in increasing order up to 99,999 before wrapping.
- **The terminal launcher follows the terminal Portico runs in.** On macOS a new terminal window (connecting from the TUI, a container shell) opens in the terminal named by `PORTICO_TERMINAL` when it is set, otherwise in the terminal Portico itself runs in (`TERM_PROGRAM`), otherwise in Terminal. `PORTICO_TERMINAL` is read on macOS only; Linux keeps `xdg-terminal-exec` and ignores it. When the settings and preferences slice of `ROADMAP.md` ships, the choice moves into the preferences file, with the variable kept as an override.
- **macOS CI: one Apple silicon job per pull request.** A `macos` job on `macos-latest` (arm64) runs clippy, the unit and integration tests and the end-to-end suite on every pull request that changes code. Intel is built, by cross-compiling on the same runner, in release builds and rehearsals, and not tested per pull request.
- **Homebrew through Portico's own tap, bumped by the release.** The formula lives in a tap repository (`brew install younesamalou/tap/portico`) and installs the prebuilt release tarballs. A release job renders the formula for the released version and pushes it to the tap with a `HOMEBREW_TAP_TOKEN` secret; while the secret is missing the job is skipped, as the AUR job is without its key. The template lives in `packaging/homebrew/portico.rb`.

## Decisions

### Platform layout

`src/platform/` gains a `unix` module for what Linux and macOS share, and a `macos` module beside `linux`:

```
src/platform/
  mod.rs        cfg selection; compile_error! for any target other than Linux and macOS
  fs.rs, paths.rs, process.rs, launcher.rs   re-export the implementation for the target
  unix/         shared by Linux and macOS
    fs.rs         locking, atomic writes, directory sync, file identity (all of today's linux/fs.rs)
    paths.rs      home_dir, tilde, tilde_text, expand
    process.rs    process groups, reaping, the stop-then-kill tree walk, stop_process, current_uid
  linux/        pidfd and /proc: Pin, children, wait_until_stopped, Proc; xdg-terminal-exec; runtime_dir
  macos/        kqueue and sysctl: Pin, children, wait_until_stopped, Proc; open(1) launcher; runtime_dir
```

`unix/process.rs` holds the algorithms (the tree walk of `kill_process_tree`, `stop_process`) and calls three things each system provides: a `Pin` (a handle that pins a process's identity: a pidfd on Linux, a kqueue exit watch on macOS) with `signal` and `wait_exit`, `children(pid)`, and `wait_until_stopped(pid)`. Linux's code moves without changing what it does. The fault hook the connection tests use, `fail_pidfd_open_after`, becomes `fail_pin_after` (and `pidfd_open_failure_injected` becomes `pin_failure_injected`) and lives in the shared code around `Pin::open`, so the tests that use it run on both systems.

### Process control on macOS

- **Pin.** `kqueue()`, then `EV_ADD` an `EVFILT_PROC` filter with `NOTE_EXIT` on the PID. `ESRCH` means the process is gone. `signal` first polls the kqueue with a zero timeout and returns `ESRCH` if the exit was reported, then calls `kill(2)`. `wait_exit(ms)` waits on the kqueue.
- **Children.** `proc_listchildpids` (libproc).
- **Stopped.** `sysctl` `KERN_PROC_PID` reports `SSTOP` once every thread is stopped.
- **`Proc`** reads `sysctl`: `KERN_PROC_PID` for the state (`SZOMB` is shown as `Z`), the real UID, the session leader flag and the start time in microseconds since the epoch; `KERN_PROCARGS2` for the arguments; `proc_listallpids` for the process list; `kern.bootsessionuuid` for the boot ID. `started_at(start)` is `start / 1_000_000`. A record's `start` field means clock ticks after boot on Linux and microseconds since the epoch on macOS; records never move between systems, and the boot ID already makes a record from another boot stale.
- **`has_exited`** uses `waitid` with `WNOWAIT`, as on Linux.

`Proc::at(<fake /proc>)` stays a Linux-only test seam; the tests built on it run on Linux. A new test in `domain::tunnel::runtime` runs on both systems against a live process whose command line looks like a started tunnel (`sh -c 'sleep 30; :' ssh -E <log> -L … -- host`, in its own session), so recognising, rebuilding and identifying a tunnel is checked on macOS against the real process table.

### Terminal launcher on macOS

`platform::launcher::launch(launcher, command)` keeps its signature and its outcome (`Requested` or `Failed` within the grace period). On macOS, `DEFAULT_LAUNCHER` is `open`, and when `launch` is given it, the terminal is chosen as above and opened through `open(1)`:

| Terminal | `PORTICO_TERMINAL` / `TERM_PROGRAM` | Invocation |
|---|---|---|
| Terminal | `Terminal` / `Apple_Terminal` | `open -a Terminal <script>.command` |
| iTerm2 | `iTerm2` / `iTerm.app` | `open -a iTerm <script>.command` |
| Ghostty | `Ghostty` / `ghostty` | `open -na Ghostty --args -e <command…>` |
| WezTerm | `WezTerm` / `WezTerm` | `open -na WezTerm --args start -- <command…>` |
| kitty | `kitty` / `kitty` | `open -na kitty --args <command…>` |
| Alacritty | `Alacritty` / `alacritty` | `open -na Alacritty --args -e <command…>` |

Names match without regard to case. Terminal and iTerm2 take no command on their command line, so Portico writes a private (`0700`) `.command` script in the per-user temporary directory that deletes itself and `exec`s the quoted command; opening it needs no Automation permission. A `PORTICO_TERMINAL` that names none of these but is a path to an executable is run as a launcher with the command as its arguments, as `xdg-terminal-exec` is on Linux. An unknown name falls back to Terminal. Any `launcher` other than `DEFAULT_LAUNCHER` is run with the command as its arguments on both systems, which is how the tests use a fake launcher.

### macOS paths

- **Runtime directory:** `~/.ssh/portico/run/tunnels`, Linux's fallback. macOS has no `XDG_RUNTIME_DIR`, and the per-user `$TMPDIR` is cleaned of files untouched for three days, which would delete a long-running tunnel's record.
- **System `ssh_config`:** `/etc/ssh/ssh_config`, where macOS keeps it.
- **Home:** `HOME`, as on Linux.

### Files

Locking (`flock` through `fs2`), atomic writes (`rename(2)`), directory sync and file identity (device, inode, size, modification and status-change times) behave the same on macOS, so `fs` is shared whole.

### `libc` outside the platform layer

- `domain::tunnel::runtime` probes a port (`bind_probe`, formerly `bind_v6_only`) on a socket from the platform's `fs::cloexec_socket`, which sets `FD_CLOEXEC` after `socket(2)` on macOS (no `SOCK_CLOEXEC`) and keeps the atomic flag on Linux. macOS lets a second `SO_REUSEADDR` socket bind an address another socket listens on, so the macOS probe binds without `SO_REUSEADDR` (for IPv4 too); Linux keeps its probe.
- `tui::hangup` uses `pipe2`, which macOS lacks; the platform's `fs::nonblocking_pipe` replaces it (`pipe` plus `fcntl` on macOS).
- `localtime_r`, `access`, `geteuid`, `getuid`, `kill`, `isatty`, `poll` and the errno values used exist on both systems and stay.

### Desktop panel

The Omarchy panel, launcher entry and keybinding (`src/desktop/`) need Linux. On macOS `portico desktop` stays parseable but is hidden from the help, and running it reports that the desktop integration needs Linux (Omarchy) and exits with status 1 (`--json` reports it as an error document). `main` skips the panel refresh on macOS. The module itself still compiles, so its unit tests run on both systems.

### Build and CI

- `ci.yml` gains a `macos` job on `macos-latest`, run when the `rust` filter is set, as a regular user (the runner's user is not root): `scripts/ci-checks.sh clippy`, `test` and `e2e`. It installs nothing beyond the runner's own Rust, OpenSSH and Python.
- The test suites stop reading `/proc` directly. Helpers in `tests/support` ask `portico::platform::process::Proc` (or `ps(1)` where a test must not trust the code under test), and Linux-only tests (`F_SETLEASE`, the fake `/proc`) are marked `#[cfg(target_os = "linux")]`.
- `release.yml` gains a `build-macos` job on `macos-latest` for `aarch64-apple-darwin` and `x86_64-apple-darwin`: a tarball per target (`scripts/package-release.sh`) and a CycloneDX SBOM, attested like the Linux artifacts. The job runs the arm64 binary's `--version` before packaging it. The binaries are ad hoc signed by the linker and not notarized; Homebrew installs them without quarantine.
- A `homebrew` job, after `release`, renders `packaging/homebrew/portico.rb` with the version and the two tarballs' SHA-256 (`scripts/homebrew-formula.sh`) and pushes it to the tap. It runs only for tags and only when the `homebrew` environment's `HOMEBREW_TAP_TOKEN` is set.

### Maintainer steps (not done by this slice)

- Create the tap repository `YounesAmalou/homebrew-tap` (public, default branch `main`, empty `Formula/` directory).
- Create a fine-grained token with contents write access to that repository only, and store it as `HOMEBREW_TAP_TOKEN` in a new `homebrew` environment of this repository.
- Optional, later: an Apple Developer ID to sign and notarize the binaries for direct downloads outside Homebrew.

## Out of scope

- A macOS desktop interface (menu bar app): v5 slice 7.
- Windows: v5 slice 6, which reuses this structure (a `windows` module beside `unix`).
- Homebrew core: the tap ships first.
