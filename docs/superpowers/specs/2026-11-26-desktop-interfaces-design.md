# macOS and Windows Desktop Interfaces Design

**Status:** Approved; implemented

## Purpose

On Linux, the Quickshell panel (`desktop/quickshell/`) lists the hosts, opens shells in hosts and their containers in a new terminal, and starts and stops saved tunnels, without anyone opening a terminal first. macOS and Windows have nothing like it: Portico runs there (#82, #86), but only from a terminal, and `portico desktop` is refused on both.

This slice adds the counterpart on each system: a **menu bar app on macOS** and a **system tray app on Windows**, one Rust program (`portico-tray`) built for both. Like the Quickshell panel, it is a separate consumer of Portico's CLI and JSON output (`docs/json-output.md`), not code inside Portico. On each system it is installed, updated and removed by `portico desktop install | uninstall`, and so from the desktop integration rows of the TUI's settings overlay.

It implements what `ROADMAP.md` listed as "macOS and Windows desktop interfaces"; that item left the roadmap when the slice shipped.

## Goals

- From the menu bar (macOS) or the notification area (Windows), with no terminal open:
  - list the managed hosts and open a shell in one in a new terminal;
  - list a host's containers (and this machine's) for every container tab that is turned on, and open a shell in one;
  - list the saved tunnels with their state, and start or stop one.
- `portico desktop install`, `uninstall` and `status` manage the app on macOS and Windows, with the same preview-and-confirm style as on Linux, and the TUI's settings overlay offers them there.
- The app and the `portico` that installed it are always the same version, or the difference is shown with a one-click fix.
- **Footprint is a requirement:** the app stays as light as the TUI in memory, CPU when idle and download size (see "Footprint").

## Non-goals

- Adding, editing, removing or adopting hosts or tunnels from the app: those stay in the TUI and CLI, as for the Quickshell panel.
- A search field, a popover window or any custom-drawn UI: the app is native menus only.
- Linux: the Quickshell panel stays the Linux desktop surface. `portico-tray` does not build a GUI on Linux.
- Notarization (macOS) and Authenticode signing (Windows) of the app: as for `portico` itself, recorded under "Production readiness" in `ROADMAP.md`.
- An app store, a Homebrew cask or a winget package for the app: `portico desktop install` is the one way in.

## Anti-goals

- A Portico daemon. The app is the user's own background program, like the Quickshell process on Linux; Portico still runs only when the app runs it.
- Polling while idle. A tray app that wakes up every few seconds fails the footprint requirement even if each wake-up is cheap.
- A second implementation of Portico's logic in the app: it never reads OpenSSH files, `settings.toml` or tunnel records itself, and never runs `ssh`, `docker`, `podman` or `pct`.
- Portico downloading and running something it cannot prove is the build it was released with.

## Constraints

- The roadmap's rules: no daemon, no database, no plugin system inside Portico; desktop surfaces are separate consumers of the CLI.
- No CLI or JSON change in this slice. A parallel slice (v6 "Quick port forwards") reworks the tunnels tab, the host and container rows and the tunnel CLI; the app reads only documented fields and must keep working on whatever that slice adds under schema 1.
- The maintainer's machine is Linux: macOS and Windows code is built and run only on the CI runners (Apple silicon and Windows x64). Whatever can be tested on Linux must be.
- Conventional Commits, branch `feat/desktop-interfaces`, one pull request for both systems.

## Approaches

### Toolkit

| Approach | Trade-off |
|---|---|
| **Rust, one crate, `tray-icon` + `muda`** (chosen) | A real `NSStatusItem`/`NSMenu` on macOS and a Win32 notification icon and popup menu on Windows. About 1-2 MB and ~10 MB of memory, nothing running when idle. Same language and tooling as Portico, one codebase, lint-checkable from Linux. The UI is limited to native menus (submenus, checkmarks, disabled items). |
| SwiftUI `MenuBarExtra` on macOS, Win32/C++ or C# on Windows | The most native look, and a popover is possible. Two extra languages and toolchains that cannot be built on the maintainer's machine; WinUI 3 brings the Windows App SDK runtime, far heavier than the TUI. |
| Tauri (a web UI in a system WebView) | One rich UI for both, but a WebView costs 50-150 MB of memory, which fails the footprint requirement. |

The maintainer chose the Rust crate.

### Shipping and keeping in step

| Approach | Trade-off |
|---|---|
| Ship the app beside `portico` in the release archive, Homebrew and winget | Always the same version, no network at install time; a `cargo install` user gets no app. |
| Embed the app in `portico` (as the QML panel is) | Bigger `portico` downloads for everyone; crates.io builds lack the app. |
| **Download on install** (chosen) | `portico desktop install` fetches the app of its own version from the GitHub release. Only users who want the app download it; needs network at install time. |

The maintainer chose download on install, with `curl` (shipped by macOS, and as `curl.exe` by Windows 10 and later) rather than a built-in HTTP client, so `portico` gains no TLS dependency; and with **SHA-256 digests baked into release builds** of `portico`, so the download must match the release `portico` came from. A `portico` built from source (`cargo install`) has no baked digests and falls back to the release's `SHA256SUMS`.

### Pull requests and location

One pull request for both systems. The app lives in `desktop/tray/`, beside `desktop/quickshell/`: its own Cargo project (with an empty `[workspace]` table so it is not a member of Portico's package), never published to crates.io, and excluded from Portico's crate.

## Design

### The app: `desktop/tray/`

```
desktop/tray/
  Cargo.toml, Cargo.lock   package `portico-tray`, publish = false, its own lock file
  build.rs                 Windows: embeds the icon and a manifest (`winres` or equivalent)
  assets/                  the menu bar template icon (macOS) and the .ico (Windows), from packaging/desktop/portico.svg
  macos/Info.plist         the bundle's plist: LSUIElement = true, bundle id, version
  src/main.rs              arguments, the event loop, the tray icon; GUI parts behind cfg(any(target_os = "macos", windows))
  src/model.rs             pure: JSON documents -> menu model (no GUI types), unit-tested on Linux
  src/portico.rs           running portico (argument lists, never a shell), off the UI thread
  src/menu.rs              menu model -> muda menu, and menu ids -> actions
  tests/                   model tests against tests/fixtures/json/ (the golden files)
  README.md                a short pointer, as desktop/quickshell/README.md
```

On Linux the binary builds but its `main` only prints that the tray app is for macOS and Windows and exits with status 1, so `cargo test` and `cargo clippy` run on Linux and cover `model` and `portico`.

**Arguments.** `portico-tray --portico <path>`: the absolute path of the `portico` to run, because a program started at login sees neither Homebrew's nor Cargo's `PATH`. Without `--portico`, or when that path no longer exists, it looks for `portico` on `PATH`, then in `/opt/homebrew/bin`, `/usr/local/bin` and `~/.cargo/bin` (macOS) or `%LOCALAPPDATA%\Microsoft\WinGet\Links` and `%USERPROFILE%\.cargo\bin` (Windows), and uses the first that runs. `portico-tray --version` prints `portico-tray X.Y.Z`.

**Which `portico` path install records.** Package managers keep a version-independent link (Homebrew's `/opt/homebrew/bin/portico` into a versioned `Cellar` directory, winget's `WinGet\Links\portico.exe`), and the versioned path behind it disappears on the next upgrade, which is exactly when the app needs `portico` to update itself. So install records the first `portico` on `PATH` whose canonical path is the same file as the canonical `current_exe`, and `current_exe` itself only when no `PATH` entry matches. (Builder's note from the design review; the app's fallback lookup above covers the rest.)

**The app's version.** `desktop/tray/Cargo.toml` stays at `0.0.0` and is never bumped: release-plz bumps only `portico-ssh`. `scripts/build-tray.sh` sets `PORTICO_TRAY_VERSION` to the release version at build time, read with `option_env!`; a build without it (CI, a checkout, `--from` a local build) reports `dev`. A `dev` app shows no version banner, only a disabled "Development build" line, since any `portico` may drive it.

**What the menu shows**, mirroring the Quickshell panel's sections:

```
Portico
├─ Tunnels                       (only when the tunnels tab is on)
│   ├─ ✓ db-prod   localhost:5432 → db:5432   Running      click: stop
│   ├─   web-dev   localhost:8080 → web:80    Stopped      click: start
│   └─   cache     Failed: <reason>                         click: start
├─ ─────────
├─ Hosts
│   ├─ api-1  ▸  Open shell
│   │            ───────
│   │            Show containers   (then, after it loads:)
│   │            Docker: shop/api · nginx:1.27 · Up 3 hours
│   │            Docker: shop/db · postgres:17 · Up 3 hours
│   └─ …
├─ This machine ▸ Show containers / containers by backend
├─ ─────────
├─ <banner when versions differ: "Update the tray app to Portico X.Y.Z">
├─ Refresh
└─ Quit
```

- **Tunnels** come from `portico tunnels --json`, every entry it lists, by name. The parallel v6 slice "Quick port forwards" adds temporary forwards to that list and may change the tunnel CLI; it is not on `main` yet. Whichever of the two slices merges second adapts the app to the other's contract (and its golden files), so the app's tunnels section follows the documented JSON on `main` at merge time: name, `forward.description`, and the state (Running, Stopped, Failed with `reason`, Port in use with `address`). A running tunnel has a checkmark and clicking it runs `portico tunnel stop --json -- <name>`; any other state runs `portico tunnel start --json -- <name>`. While one runs, its item is disabled and labelled "Starting…" or "Stopping…"; the result document's `tunnel` report replaces it, and a `failed` outcome shows `error.message` as a disabled line under it. An unknown `status.state` shows the raw state and no action.
- **Hosts** come from `portico hosts --json`: one submenu per alias (the first of a duplicate alias, as `ssh` uses), `external` shown in the label. "Open shell" runs `portico connect --new-terminal -- <alias>`.
- **Containers** load only on request, as in the Quickshell panel: a menu cannot tell when a submenu opens on both systems, so each host (and "This machine") has a "Show containers" item. Clicking it runs the same single command the panel runs (`containersCommand` in `portico.js`): `portico containers --json --on=<alias> --backend=<b>...` with the container tabs that are on (without `--on` for this machine). Portico answers for the first of those backends installed on the target, so the listing is one backend's, named in each item ("Docker: …"); this is one `ssh` round trip per click, and showing every backend at once would need a CLI change this slice does not make. The items are added below "Show containers", which becomes "Refresh containers". A container item shows `display_name`, `image` and `status`, and runs `portico shell --new-terminal --on=<alias> --backend=<b> -- <id>` (Portico applies the remembered user itself). A failed listing shows `message` and the first `next` step as disabled lines, and is listed again on the next click. Listings are kept until the next Refresh.
- **Settings:** `portico settings --json` decides which sections exist: Tunnels only when `tabs.tunnels` is on, and only the backends whose tab is on, as on Linux. With no container tab on, "Show containers" is left out.
- **Errors and contract checks** follow the Quickshell panel's rules (`portico.js` `parse`): not JSON or no `schema` ("needs a Portico release with `--json`"), a `schema` other than 1 ("Update the tray app" or "Update Portico"), an `error` document scoped to its section. A launch (`connect`, `shell`) that exits non-zero shows the last non-empty stderr line, or "Portico could not open a terminal (exit status N)", as a disabled line under the item, until the next refresh.
- **Not found:** when the `portico` path does not run, the menu shows only "Portico was not found at <path>", a hint to run `portico desktop install` again, Refresh and Quit.
- All text is set as plain menu titles; free text in the documents is already one line with no control characters (`docs/json-output.md`, "Free text is one line"), and the app truncates titles to a readable length (builder's call, about 60 characters).

**When it runs portico.** At start, and on Refresh: `portico --version`, `settings --json`, `hosts --json` and `tunnels --json`, in that order, on a worker thread. Also when the menu is opened: on macOS and Windows `tray-icon` reports the click that opens the menu, and the app starts a refresh of `tunnels --json` then (hosts and settings only on Refresh), so a tunnel started from a terminal shows up the next time the menu is looked at. Nothing runs on a timer. Every run is an argument list with no shell; on Windows every child gets `CREATE_NO_WINDOW` so no console flashes. Results are sent to the event loop, which rebuilds the affected part of the menu. A menu is never replaced while it is open: new data shows the next time the menu opens.

**Version check and self-update.** The app compares its own version with `portico --version`. When they differ, a banner item "Portico is X.Y.Z, this app is A.B.C: Update" hands the update off and quits:

1. The app starts `portico desktop install --yes --tray-only` **detached** from itself: in a new session (`setsid`) on macOS, with `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` on Windows, with its output appended to `tray-update.log` in the app's support directory (macOS `~/Library/Application Support/Portico/`, Windows `%LOCALAPPDATA%\Portico\`), beside the app's single-instance lock. The LaunchAgent sets `AbandonProcessGroup` so launchd does not kill the installer when the app exits.
2. The app quits at once.
3. The installer waits up to 5 seconds for the app to exit, stops it through `platform::process` if it has not (on Windows only the app's own process, never its tree, because the installer is its child), replaces it, and starts the new copy (`launchctl kickstart gui/<uid>/<label>` when the login entry exists, otherwise directly, detached). It never runs `launchctl bootout` during an update.
4. If any step fails, the installer starts whichever app is in place (the old one when the replacement never happened), and the last line of `tray-update.log` says why. At start, an app finds a log whose last line is a failure newer than itself and shows it as a disabled "Update failed: <message>" item until the next successful update, which removes the log.

`--tray-only` updates only the app; it neither creates nor removes the login entry.

**Single instance.** A second copy started while one runs exits at once (a named mutex on Windows; a lock file under the app's support directory on macOS, `platform`-style `flock`).

### Installing it: `portico desktop` on macOS and Windows

`portico desktop` stops being Linux-only. Its subcommands keep their names; what each installs depends on the system:

| System | Pieces |
|---|---|
| Linux | The Quickshell panel, the launcher entry, the keybinding (unchanged) |
| macOS | The menu bar app, and starting it at login |
| Windows | The tray app, and starting it at login |

On macOS and Windows the Linux-only flags (`--omarchy`, `--quickshell`, `--bind`, `--no-bind`, `--no-panel`, `--no-launcher`) are hidden and refused with "only on Linux", and two flags appear: `--no-login` (leave out starting at login) and `--from <path>` (install from a local archive in the release layout instead of downloading: for a `portico` with no release, such as a checkout, and for testing; `scripts/build-tray.sh` makes one). `--from` takes an archive only, never a bare binary, so `portico` never has to assemble the macOS bundle itself; its digest is not checked, and the preview says so. `--yes` and the per-piece questions behave as on Linux. `--tray-only` (hidden) updates the app and leaves the login entry as it is: it is what the app's banner runs.

**Where things go:**

| | macOS | Windows |
|---|---|---|
| App | `~/Applications/Portico Tray.app` (bundle: `Contents/MacOS/portico-tray`, `Contents/Info.plist`, `Contents/Resources/portico.icns`) | `%LOCALAPPDATA%\Programs\Portico\portico-tray.exe` |
| Version stamp | `CFBundleShortVersionString` in the bundle's `Info.plist` | `portico-tray.exe --version` |
| At login | LaunchAgent `~/Library/LaunchAgents/io.github.younesamalou.portico.tray.plist` (`ProgramArguments`: the app's binary, `--portico`, `<path>`; `RunAtLoad`; `LimitLoadToSessionType` Aqua) | Value `Portico Tray` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`: `"<app>" --portico "<path>"` |
| Started now | `launchctl bootstrap gui/<uid> <plist>` (or `open -a` when `--no-login`) | spawned detached (`DETACHED_PROCESS`), as tunnels are |
| Stopped (install over a running copy, and uninstall) | `launchctl bootout gui/<uid>/<label>` when the user runs install or uninstall from a terminal (never during a self-update, see above), then any remaining running app is stopped through `platform::process` | the running `portico-tray.exe` of this user is stopped through `platform::process` |

The LaunchAgent label and the registry value name are Portico's own markers: uninstall removes only those, as on Linux it removes only lines between its markers.

**Download.** The release asset of `portico`'s own version and system:

```
https://github.com/YounesAmalou/Portico/releases/download/v<version>/portico-tray-v<version>-<target>.<tar.gz|zip>
```

where `<target>` is the target `portico` was built for (`aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`). Install runs `curl --fail --location --proto =https --tlsv1.2 --silent --show-error -o <tmp> <url>` into a private scratch directory under the app's support directory, announced with "Downloading <asset>…", before any question is asked, so the preview can show the download's size and digest check; nothing outside the scratch directory changes until the questions are answered. It then checks the file's SHA-256 (computed in Portico, a small dependency-free implementation or the `sha2` crate, builder's call against `cargo deny`):

- **Release builds** of `portico` carry the expected digest for their own target, set at build time (`PORTICO_TRAY_SHA256`, read with `option_env!`). A mismatch stops the install and deletes the download.
- **Builds without it** (`cargo install`, a checkout) download `SHA256SUMS` of the same release the same way and use its line for the archive; the preview says "checked against the release's SHA256SUMS". A version with no release (a 404) stops with a message naming `--from`.

It then unpacks with `tar` (`/usr/bin/tar -xzf` on macOS; on Windows `%SystemRoot%\System32\tar.exe`, bsdtar, which reads the zip; never a `tar` found on `PATH`), into the scratch directory, checks the unpacked layout is the expected one (a single `Portico Tray.app` or `portico-tray.exe`, no path leaving the directory), stops a running copy, and moves the result into place (replacing the previous version). On macOS, a downloaded file carries no quarantine attribute when `curl` writes it, so Gatekeeper does not block the ad-hoc-signed app; install clears `com.apple.quarantine` anyway when `--from` points at a file that has one.

`curl` or `tar` missing, a network error and a non-2xx status are each reported with the command that failed and its last stderr line.

**Preview and questions**, in the style of the Linux flow (every question is asked `[y/N]`, like every other Portico confirmation; the block below shows the default answer as `[Y/n]` only as a sketch of the layout):

```
Menu bar app       not installed → Portico Tray 0.8.0
                   download portico-tray-v0.8.0-aarch64-apple-darwin.tar.gz (1.4 MB),
                   checked against the digest built into this portico
                   install to ~/Applications/Portico Tray.app
Install it? [Y/n]
Start at login     not set up → LaunchAgent io.github.younesamalou.portico.tray
Set it up? [Y/n]
```

A piece that is already in place reports "installed, 0.8.0" and is not asked about; an app of another version is offered as an update.

**`status`** prints, per piece: the app (not installed / installed, version, and "out of date" when it differs from `portico`), the login entry (set up, pointing at which `portico`; or "points at <path>, which no longer exists"), and whether it is running. `desktop::Status` gains these lines, so the TUI overlay shows them as it shows the Linux ones.

**`uninstall`** previews and removes the login entry, stops the app and removes it.

**`main`** keeps skipping the automatic panel refresh outside Linux: updating the tray app needs the network, so it happens only from `desktop install`, the overlay, or the app's banner.

### The TUI settings overlay

On macOS and Windows the overlay's "Desktop integration" section appears, with the same two actions (Install or update, Uninstall) as on Linux; both already run `portico desktop install | uninstall` as a child in the same terminal. Only the status lines differ, from `desktop::Status`.

### Code layout in Portico

- `src/desktop/` gains a `tray` module: the plan (paths, login entry, the asset URL and expected digest), and the per-system pieces behind `cfg(target_os = "macos")` and `cfg(windows)`; the Linux pieces stay as they are. Pure planning (paths, URL, digest choice, the plist and the registry command line) compiles on every system so its tests run on Linux.
- `src/cli/desktop.rs` dispatches `install | uninstall | status` to the Linux or the tray flow; the questions and the preview format are shared.
- Running `curl`, `tar`, `launchctl` and starting the app go through `platform::process` and `platform::launcher` helpers; the registry value through `windows-sys` (`Win32_System_Registry` is already a feature).
- `src/main.rs` no longer refuses `portico desktop` outside Linux; `src/cli/mod.rs` no longer hides it.
- `docs/architecture.md`: the `desktop` row no longer says "Linux only".

### Build, CI and release

- **CI (`ci.yml`):** the `changes` job gains `desktop/tray/`. When it changes:
  - `tray (linux)`: `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test` in `desktop/tray/` (the model tests against the golden files), in the Arch container;
  - `macos (tray)` and `windows (tray)`: `cargo clippy -D warnings` and `cargo build --release` on the existing runners, then the footprint check (below);
  - `audit` and `deny` also check `desktop/tray/Cargo.lock`.
  These are required checks only through the existing aggregate rules; whether they become required branch-protection checks is the maintainer's call outside the repository.
- **`scripts/ci-checks.sh tray`** runs the Linux part locally.
- **Release (`release.yml`):** `build-macos` and `build-windows` build `portico-tray` first for each target (`scripts/build-tray.sh <version> <target> <outdir>`), package it (`portico-tray-v<version>-<target>.tar.gz` holding `Portico Tray.app` on macOS, built from `macos/Info.plist` with the version filled in and ad-hoc signed with `codesign -s -`; `.zip` holding `portico-tray.exe` on Windows), compute its SHA-256 and export it as `PORTICO_TRAY_SHA256` to the `portico` build of the same target. The tray archives are attested and listed in `SHA256SUMS` like every other artifact. A rehearsal builds them too.
- **Dependabot** gains a Cargo entry for `/desktop/tray`.
- **Portico's crate** excludes `desktop/tray/`.

### Footprint

The requirement is "as light as the TUI". Measured and enforced as follows:

- **Download size:** CI fails `macos (tray)` and `windows (tray)` when the packaged app is larger than a fixed 4 MiB cap (about the size of the `portico` archive today). Expected: 1-2 MB.
- **Idle CPU:** by construction, nothing runs on a timer; the event loop blocks in the system's run loop. The design forbids timers, so a review catches one.
- **Memory:** measured by hand on each system when the slice is verified (Activity Monitor's "Memory", Task Manager's "Memory (private working set)") and recorded in this design's "Verification" section; the target is at most the TUI's resident memory with an open host list on the same machine. CI cannot measure a GUI app's memory meaningfully on a headless runner.

### Testing

- `desktop/tray` model tests on Linux, against the golden files in `tests/fixtures/json/`: every documented state of a tunnel, host warnings and duplicates, container listings per backend and their errors, the settings tabs, schema mismatches, a non-JSON answer.
- `portico` unit tests (run on Linux) for the tray plan: the asset URL per target, digest selection (baked versus `SHA256SUMS`, a missing line, a mismatch), the `SHA256SUMS` parser, the plist and the registry command line (quoting of paths with spaces), the unpacked-layout check (rejecting extra files and paths that leave the directory), and the preview text.
- CLI tests with a fake `curl` and `tar` on `PATH` (as the tests already fake `ssh` and `docker`), on macOS and Windows CI: install from a fixture archive, the digest mismatch, a 404, `--from`, uninstall, status. Starting at login is tested by its files and registry value, never by logging in; `launchctl bootstrap` and starting the app are behind a seam the tests replace.
- The e2e suite is not extended: the app is a GUI.
- Manual verification on a Mac and a Windows machine before merge (see "Verification").

## Documentation

- `README.md`: the "Desktop panel" section becomes "Desktop integration", with a subsection per system; macOS and Windows: `portico desktop install`, what it downloads and where it goes, how to remove it, the Gatekeeper and SmartScreen notes.
- `docs/json-output.md`: the tray app named beside the panel as a consumer; no contract change.
- `docs/architecture.md`: the `desktop` module row.
- `CONTRIBUTING.md`: the tray CI jobs, `scripts/ci-checks.sh tray`, the release's tray artifacts.
- `ROADMAP.md`: remove v5 item 1 (and the v5 heading with it).
- `desktop/tray/README.md`.

## Left to the builder

- The `tray-icon`, `muda` and Windows resource crate versions, checked against `cargo deny`'s licence and advisory policy.
- How the event loop is driven on each system (`tao` or `winit` versus the bare system loop that `tray-icon`'s examples use); the lighter one that works wins.
- - The SHA-256 implementation in `portico`.
- Title truncation length.

## Verification

Done on Linux and in CI: the pure parts of the app and of the install flow are unit-tested, the app's Windows menu and event loop are linted with `cargo clippy --target x86_64-pc-windows-gnu`, and the macOS and Windows jobs build, package and size-check the archives.

Still to be done, on a Mac (Apple silicon) and a Windows machine (x64), because the maintainer's machine is Linux and CI cannot do them:

- The memory measurement: the app idle and with a menu open, and the TUI's for comparison, with the app's download size, recorded here.
- A real run of the app on each system, covering what only a real desktop shows: on macOS, that the run loop's mode during menu tracking delivers the refresh started when the menu opens, and `launchctl bootstrap` and `kickstart` of the LaunchAgent; on Windows, that the popup menu's mode is detected so no menu is replaced while open, and the `Portico Tray` value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
- The self-update hand-off on both systems: the banner starts the detached installer, the app quits, the installer replaces it and starts the new copy, and a failure leaves the old app running with the reason in `tray-update.log`.
