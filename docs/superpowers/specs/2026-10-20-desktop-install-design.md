# Desktop Install Design

**Status:** Approved; implemented

## Purpose

The desktop panel (`2026-10-19-desktop-panel-design.md`) installs today only through symlinks from a checkout of this repository, and nothing gives Portico a launcher entry or a keybinding. This slice adds `portico desktop install | uninstall | status`: a CLI-only command (no TUI) that integrates Portico with the desktop in three optional, separately confirmed components:

1. **Panel:** the Quickshell panel, embedded in the binary, written to a Portico-owned directory and linked into Omarchy's plugins or Quickshell's configurations.
2. **Launcher entry:** a freedesktop `.desktop` file and an SVG icon, so application launchers list Portico (it opens the TUI in a terminal).
3. **Keybinding:** one line, between markers, in the compositor's configuration, to toggle the panel.

It also ships the launcher entry in the AUR package, and covers the "Ship the desktop panel in packages" row of `ROADMAP.md`'s "Production readiness" table: the panel ships inside the binary itself, so every channel that installs `portico` (crates.io, the AUR, the release tarball) carries it.

The rules of `ROADMAP.md` hold: no daemon (updates happen when `portico` runs), no database (no record of what was installed: everything is recognised by its location, link target or markers), no plugin system inside Portico (the panel stays a separate consumer of the `--json` contract), and every change to the user's files is previewed and confirmed, as `init`, `adopt` and `export` do.

## Settled questions

- **The panel ships inside the binary.** `src/desktop/panel.rs` embeds every file of `desktop/quickshell/` except `tests/` and `preview.png` with `include_bytes!`, as a fixed list of `(name, bytes)`. A plain `cargo install portico-ssh` with no checkout can therefore install the panel, and the installed panel always matches the binary's JSON schema. A test compares the list with the directory, so a file added to the panel without being embedded (or the reverse) fails CI. `Cargo.toml`'s `exclude` stops excluding `desktop/` and `packaging/` wholesale: it excludes `desktop/quickshell/tests/`, `desktop/quickshell/preview.png` and `packaging/aur/`, so the crate carries the panel files and `packaging/desktop/`; `tests/package.rs` pins exactly that.

- **Portico owns one directory for the panel:** `$XDG_DATA_HOME/portico/panel`, or `~/.local/share/portico/panel` when `XDG_DATA_HOME` is unset, empty or not absolute (as the XDG specification says). It holds the embedded files and `VERSION`, a one-line stamp `<crate version>+<hash>`, where the hash is a 64-bit FNV-1a of the embedded names and bytes, printed as 16 hex digits. The hash makes two builds of the same version with different panel files (a checkout between releases) refresh as well. Only links go into the target's configuration; the directory is the single copy, written only by Portico. The README's statement of which files Portico owns gains this directory, the launcher files and the marked keybinding lines.

- **Target detection: Omarchy first, then plain Quickshell.** Omarchy runs on Quickshell, so checking `qs` first would give an Omarchy user a second, separate shell.
  - **Omarchy** when `omarchy-shell` is on `PATH` and `~/.config/omarchy/plugins` is a directory. Omarchy always uses `~/.config`, whatever `XDG_CONFIG_HOME` says (its shell's `PluginRegistry.qml`, its `omarchy plugin` commands and its Hyprland bootstrap, which loads `hypr.bindings`, all hardcode `$HOME/.config`), so the Omarchy target's link and keybinding file are under `~/.config`. Plain Quickshell, Hyprland and Sway honour `XDG_CONFIG_HOME`.
  - **Quickshell** when `qs` is on `PATH`.
  - `--omarchy` or `--quickshell` forces a target (they conflict with each other). A forced target still needs its program: `--omarchy` without `omarchy-shell` is an error naming it.
  - Neither found: `install` stops before printing a plan, says what is missing and prints the exact commands to use once it is there:

    ```
    Portico found no desktop shell to install the panel into:
      Omarchy needs omarchy-shell on PATH and ~/.config/omarchy/plugins (neither found).
      Quickshell needs qs on PATH (not found).
    Install Quickshell, or Omarchy, then run:
      portico desktop install --quickshell
      portico desktop install --omarchy
    To install only the launcher entry: portico desktop install --no-panel
    ```

    and exits with status 1. `--no-panel` installs the launcher entry alone (the keybinding needs the panel, see below).

- **What each target does with the panel.**
  - **Omarchy:** link `~/.config/omarchy/plugins/portico` to the panel directory, run `omarchy-shell shell rescanPlugins`, then `omarchy plugin enable portico`. Omarchy's scanner follows the link, and it reloads a plugin whose files change, so a refresh of the panel directory reaches the running shell.
  - **Quickshell:** link `~/.config/quickshell/portico` to the panel directory, then run `qs -c portico -n -d` (start it detached now, unless one is already running) so the keybinding works at once. Portico does not edit the autostart: `install` prints the one line that starts the panel with the session, for the compositor found (Hyprland Lua `hl.on("hyprland.start", function() hl.exec_cmd("qs -c portico") end)`, Hyprland `exec-once = qs -c portico`, Sway `exec qs -c portico`, otherwise "start `qs -c portico` with your session").
  - **An existing path at the link location.** A link to the panel directory is already installed, and nothing is done. A link elsewhere (the checkout link of the manual steps, or a dangling link) is replaced, and the plan says so, naming its old target. A real file or directory is never touched: the panel component is unavailable, and the plan says why and what to move away.

- **Updates without a daemon or a post-install hook.** cargo has no post-install hook, and an AUR `.install` script runs as root and cannot reach user homes, so the panel is kept current from the user's own runs:
  1. **Every `portico` run** (any command except `desktop`, and except `--help` and `--version`, which clap answers before) reads `<panel dir>/VERSION`. When the file does not exist nothing else happens: no panel is installed, and the cost is one failed `open`. When the stamp is from an older Portico (its version sorts before this binary's as a semantic version), or from another build of this same version (same version, different hash), Portico rewrites its own copy: every embedded file is written to `<name>.tmp-<pid>` and renamed over its name, files of the directory that are not embedded (a file an older panel had) are removed, and `VERSION` is written last. It never touches anything outside the panel directory: the links point at the directory, so they keep working. Two `portico` runs at once (the panel starts `hosts` and `tunnels` together) write the same bytes, and each rename is atomic, so the result is the same.
  2. **A refresh that fails never fails the command and prints nothing:** stdout and stderr belong to the command (a `--json` document must be the only output, and the TUI owns the terminal). The stamp stays old, which `portico desktop status` reports ("written by 0.5.1; this portico is 0.6.0") with the fix (`portico desktop install`), and which the panel shows as a banner.
  3. **Re-running `portico desktop install`** repairs or updates everything: it rewrites panel files whose content differs from the embedded ones, restores a missing or wrong link, and offers what is missing.
  4. **The panel's banner.** When it opens, the panel reads `VERSION` next to its own files (Quickshell `FileView`) and runs `portico --version`. When `VERSION` exists and its version (the part before `+`) differs from Portico's, it shows a one-line banner above the lists: "This panel is from Portico 0.5.1 and portico is 0.6.0. Run `portico desktop install` to update it." The lists still load. A panel run from a checkout has no `VERSION`, so it shows no banner; the `schema` check still covers it. The comparison is a pure function of `portico.js`, tested with the others.
  - **Only forward, and never from a build directory.** A panel stamped by a newer Portico is left alone, and a binary under a cargo build directory (an ancestor holding cargo's `CACHEDIR.TAG`, or a `target` directory next to a `Cargo.toml`) never refreshes. So two Portico binaries on one machine (say `cargo install` and the AUR package, or a `cargo run` during development) never take turns rewriting the panel, which Omarchy would hot-reload each time. The banner and `status` cover a panel that is newer than the `portico` it runs, and `portico desktop install` from the older binary rewrites it on request. The development check runs only once a refresh is due, so a run with no panel still costs one failed `open`.
  - **A link or a file at the panel directory** (a developer linking it to a checkout) is never followed: the refresh does nothing there, `install` reports the panel as unavailable with the reason, `status` reports it as blocked, and `uninstall` leaves it alone. Following it would write into, and remove files from, a directory that is not Portico's.
  - A refresh while the panel is open makes Quickshell reload it (Omarchy and `qs` both reload on change), which closes the popup once after an upgrade. That is accepted.

- **The launcher entry is two committed files,** `packaging/desktop/portico.desktop` and `packaging/desktop/portico.svg`:

  ```ini
  [Desktop Entry]
  Type=Application
  Name=Portico
  GenericName=SSH Host Manager
  Comment=Browse, connect to and manage your OpenSSH hosts
  Exec=portico
  Icon=portico
  Terminal=true
  Categories=Network;System;
  Keywords=ssh;openssh;hosts;tunnels;containers;
  ```

  The icon is a simple monochrome-friendly SVG of a portico (pediment over columns), drawn for this slice.
  - **The AUR package** installs them to `/usr/share/applications/portico.desktop` and `/usr/share/icons/hicolor/scalable/apps/portico.svg`.
  - **The user install** writes them to `$XDG_DATA_HOME/applications/portico.desktop` and `$XDG_DATA_HOME/icons/hicolor/scalable/apps/portico.svg` (default `~/.local/share`), from copies embedded in the binary, with two changes: a first comment line `# Installed by portico desktop install; portico desktop uninstall removes it.` (an XML comment in the SVG, after its declaration), which is how `uninstall` and `status` recognise them, and `Exec=` set to the absolute path of the running binary (`std::env::current_exe`). The session that starts a launcher often lacks `~/.cargo/bin` on `PATH`, the same problem the panel solves with its lookup; `status` reports an `Exec=` path that no longer exists, and when the running binary lies under a cargo `target/` directory (a development run), the plan warns that the entry would run a build directory's binary.
  - **When a system entry exists** (`applications/portico.desktop` in any directory of `XDG_DATA_DIRS`, default `/usr/local/share:/usr/share`), the launcher component is shown as "provided by /usr/share/applications/portico.desktop" and not offered: a user copy would only shadow it.
  - A file at the user location without the marker is the user's own: the component is unavailable, and the plan says so. So is any link at either location, dangling or not: writing through it would create a file outside `~/.local/share`, which `uninstall` would never remove.
  - No cache is updated: `update-desktop-database` only matters for MIME associations, and launchers scan `hicolor` without `gtk-update-icon-cache`.

- **The keybinding is interactive and confirmed, never silent.**
  - **Proposal:** `SUPER + ALT + P`. Combinations are written and parsed as modifiers and one key joined by `+` (spaces optional, any case): modifiers `SUPER` (also `WIN`, `LOGO`, `MOD4`), `CTRL` (`CONTROL`), `ALT` (`MOD1`), `SHIFT`; at least one modifier; the key is a letter, a digit, `F1` to `F24`, or another key name passed through as written. Portico prints them canonically, in the order SUPER, CTRL, ALT, SHIFT, upper case, as Omarchy does (`SUPER + ALT + P`).
  - **Which compositor:** Hyprland when `HYPRLAND_INSTANCE_SIGNATURE` is set (Omarchy always is), Sway when `SWAYSOCK` is set, otherwise another compositor.
  - **Is it taken?**
    - On Hyprland, including Omarchy: `hyprctl binds -j`, the live and standard listing, which includes every source file and Omarchy's defaults. A bind is a clash when its `submap` is empty, it is not a `mouse` bind, its `modmask` equals the combination's (SHIFT 1, CTRL 4, ALT 8, SUPER 64), and its `key` equals the key ignoring case. The clash is named by its `description`, or by `dispatcher arg` when it has none. If `hyprctl` fails, the keybinding is unavailable ("could not list Hyprland's bindings: …"); Portico never writes a binding it could not check.
    - On Sway: the configuration (`$XDG_CONFIG_HOME/sway/config`, else `~/.sway/config`) and the files it `include`s (globs expanded, one level), reading `set $mod <value>` and every `bindsym` line (flags such as `--release` skipped), with `Mod4`/`Logo` as SUPER, `Mod1`/`Alt` as ALT, `Control`/`Ctrl` as CTRL. The block form `bindsym { <keys> <command> … }` is read line by line. Bindings inside a `mode` block are ignored.
    - Portico's own marked line counts as Portico's, not as a clash: re-running `install` with the same combination finds it already in place.
  - **Where the line goes, exactly:**

    | Setup | File | Line |
    |---|---|---|
    | Omarchy | `~/.config/hypr/bindings.lua` (Omarchy's file for personal bindings) | `o.bind("SUPER + ALT + P", "Portico", "omarchy-shell shell toggle portico")` |
    | Hyprland with a Lua configuration (`~/.config/hypr/hyprland.lua` exists) | `hyprland.lua` | `hl.bind("SUPER + ALT + P", hl.dsp.exec_cmd("qs -c portico ipc call portico toggle"), { description = "Portico" })` |
    | Hyprland with `hyprland.conf` | the file `hyprland.conf` `source`s whose name contains `bind` (as in `source = ~/.config/hypr/bindings.conf`), else `hyprland.conf` | `bind = SUPER ALT, P, exec, qs -c portico ipc call portico toggle` |
    | Sway | the main configuration | `bindsym $mod+Alt+p exec qs -c portico ipc call portico toggle` when `$mod` is `Mod4` or `Logo`, else `bindsym Mod4+Alt+p …` |
    | Another compositor | nothing | `install` prints the command to bind (`qs -c portico ipc call portico toggle`) and changes nothing |

    `~/.config/hypr` honours `XDG_CONFIG_HOME`, except for Omarchy, whose `bindings.lua` is always `~/.config/hypr/bindings.lua`. The Omarchy target uses Omarchy's command; a plain Quickshell target on Hyprland or Sway uses the `qs` IPC command. A plain Quickshell setup's binding only works while `qs -c portico` runs, which is why `install` starts it and prints the autostart line.
  - **The written block** is appended at the end of the file, after a newline if the file lacks a final one:

    ```lua
    -- portico desktop install: begin
    o.bind("SUPER + ALT + P", "Portico", "omarchy-shell shell toggle portico")
    -- portico desktop install: end
    ```

    (`#` instead of `--` in Hyprland `.conf` and Sway files). `uninstall` removes the lines from `begin` to `end`, inclusive, and nothing else; a second block, or a `begin` without an `end`, makes it stop and say so rather than guess. Changing the combination replaces the block in place.
  - **Backup:** before each edit, the file is copied to `<file>.portico-bak.<unix seconds>` (Omarchy's own backups use the same timestamped pattern), created exclusively: when that name is taken (two edits in one second), `-1`, `-2` and so on are appended, so a backup is never overwritten. The write goes to a temporary file renamed over the original, keeping its permissions. Backups are never removed by Portico; the plan names the one it will create.
  - **A configuration file that is a link** (dotfiles managed by stow or chezmoi) keeps its link: Portico resolves it and edits the file it points to, with the temporary file and the backup next to that file, and the plan names both (`~/dotfiles/hypr/bindings.lua (linked from ~/.config/hypr/bindings.lua)`).
  - **Reload:** Hyprland reloads its configuration when a file changes; Portico then runs `hyprctl binds -j` again and, if the combination is not bound yet, says so and suggests `hyprctl reload`. Sway does not, so Portico runs `swaymsg reload`, named in the plan.
  - **The keybinding needs the panel:** it is offered only when the panel is installed or being installed in the same run. Declining the panel at its question skips the keybinding with "Skipping the keybinding: it toggles the panel, which is not installed."
  - **Flags:** `--bind "<combo>"` proposes that combination instead of the default; `--no-bind` skips the component; `--yes` accepts every proposal non-interactively but still refuses a taken combination (below).

- **The `install` flow: the whole plan, then one question per component, then the changes.** `install` first works out every component (nothing written), prints the whole plan (components, every file and link it will create, edit or replace, every backup and every command it will run), then asks for each component that has something to do, `[y/N]` as Portico's other confirmations. The keybinding question also accepts a combination: typing one checks it, prints the new line and asks again; typing nothing or `n` skips it. When the default is taken, the question becomes "Type another combination, or press Enter to skip". All questions come before any change, so stopping at a prompt (Ctrl+C, end of input) changes nothing. Then it applies the accepted components in order (panel, launcher entry, keybinding), printing one line per step. A failed step (a command that exits non-zero, a write that fails) is reported with its error and its fix, the components after it still run, and `install` exits with status 1; a component that failed half-way is left as far as it got, and `install` again (or `uninstall`) handles it, since every step is idempotent.
  - Components with nothing to do are listed as "already installed" and not asked about. A component that is unavailable is listed with its reason and not asked about.
  - **`--yes`** answers yes to every question. With `--yes`, a taken combination (the default or `--bind`'s) stops `install` before any change, exit status 1: "SUPER + P is taken (Pseudo window). Choose another with --bind, or skip the keybinding with --no-bind." Without a terminal and without `--yes`, `install` prints the plan and fails with "desktop install requires --yes in non-interactive mode", as `adopt` does.
  - **Other flags:** `--no-panel` and `--no-launcher` leave those components out (and `--no-panel` leaves out the keybinding).

- **`status` reports each component and the panel version,** reads only (it runs `omarchy plugin list --json` for the enabled state, and nothing else), and exits 0:

  ```
  $ portico desktop status
  Panel        installed in ~/.local/share/portico/panel, 0.6.0 (matches this portico)
    Omarchy    ~/.config/omarchy/plugins/portico -> ~/.local/share/portico/panel, enabled
    Quickshell not linked
  Launcher     ~/.local/share/applications/portico.desktop (runs /home/you/.cargo/bin/portico)
               ~/.local/share/icons/hicolor/scalable/apps/portico.svg
  Keybinding   SUPER + ALT + P in ~/.config/hypr/bindings.lua, line 62
  ```

  Other states: "not installed"; "written by 0.5.1; this portico is 0.6.0, run `portico desktop install`"; "files differ from this portico's, run `portico desktop install`"; a link to elsewhere ("links to ~/Dev/Portico/desktop/quickshell, not Portico's copy"); "provided by /usr/share/applications/portico.desktop"; "runs /old/path/portico, which no longer exists"; "unmarked file, not Portico's"; "Omarchy plugin disabled" or "enabled state unknown (omarchy-shell is not running)".

- **`uninstall` removes exactly what `install` created, after a preview.** It finds, without any record: the panel directory; links at the two link locations whose target is the panel directory (a link elsewhere is the user's and stays); the two launcher files carrying the marker; and the marked block in each candidate keybinding file (Omarchy's `bindings.lua`, the Hyprland and Sway files above). It prints the plan and asks once, `Remove the desktop integration? [y/N]` (`--yes` skips the question, and a non-interactive run needs it). Then, in order:
  1. Omarchy: `omarchy plugin disable portico` (when the plugin is enabled), remove the link, `omarchy-shell shell rescanPlugins`. Quickshell: `qs -c portico kill` (when one is running, per `qs list`), then remove the link.
  2. Remove the marked block from each file, with a backup first, and `swaymsg reload` on Sway.
  3. Remove the launcher files.
  4. Remove the panel directory, and `$XDG_DATA_HOME/portico` when it is then empty.

  Backups are kept. A failed step is reported, the others still run, and the exit status is 1. Nothing to remove prints "Portico's desktop integration is not installed." and exits 0.

- **Command shape.** A `desktop` subcommand group in `src/cli.rs`, its printing and prompting in `src/cli/desktop.rs`, and the logic in `src/desktop.rs` with `src/desktop/{panel,launcher,keybinding,combo}.rs`. The environment it needs (`HOME`, `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_DIRS`, `PATH`, `HYPRLAND_INSTANCE_SIGNATURE`, `SWAYSOCK`) is read in `main.rs` into a `DesktopEnv` value, so `main` stays the only reader of `HOME` and tests pass their own. External programs (`omarchy-shell`, `omarchy`, `qs`, `hyprctl`, `swaymsg`) are found on that `PATH` and run with argument lists, never through a shell, with their output captured and shown on failure.

  ```
  portico desktop install [--omarchy | --quickshell] [--bind <COMBO> | --no-bind] [--no-panel] [--no-launcher] [-y | --yes]
  portico desktop uninstall [-y | --yes]
  portico desktop status
  ```

## Transcripts

### Omarchy (the maintainer's machine: the panel was linked from a checkout)

```
$ portico desktop install
Portico 0.6.0: desktop integration for Omarchy (omarchy-shell found, ~/.config/omarchy/plugins exists).

Panel
  write    ~/.local/share/portico/panel/ (10 files and VERSION, 0.6.0)
  replace  ~/.config/omarchy/plugins/portico, a link to ~/Dev/Portico/desktop/quickshell,
           with a link to ~/.local/share/portico/panel
  run      omarchy-shell shell rescanPlugins
  run      omarchy plugin enable portico

Launcher entry
  write    ~/.local/share/applications/portico.desktop (runs /home/yousen/.cargo/bin/portico)
  write    ~/.local/share/icons/hicolor/scalable/apps/portico.svg

Keybinding: SUPER + ALT + P, free (checked with hyprctl binds)
  backup   ~/.config/hypr/bindings.lua to ~/.config/hypr/bindings.lua.portico-bak.1792432000
  append   to ~/.config/hypr/bindings.lua, after line 61:
             -- portico desktop install: begin
             o.bind("SUPER + ALT + P", "Portico", "omarchy-shell shell toggle portico")
             -- portico desktop install: end

Install the panel? [y/N] y
Install the launcher entry? [y/N] y
Add the keybinding SUPER + ALT + P? Type y, n, or another combination [y/N] SUPER + P
SUPER + P is taken (Pseudo window). Type another combination, or press Enter to skip: SUPER + ALT + P
  append   to ~/.config/hypr/bindings.lua, after line 61:
             -- portico desktop install: begin
             o.bind("SUPER + ALT + P", "Portico", "omarchy-shell shell toggle portico")
             -- portico desktop install: end
Add the keybinding SUPER + ALT + P? Type y, n, or another combination [y/N] y

Wrote ~/.local/share/portico/panel (0.6.0).
Linked ~/.config/omarchy/plugins/portico -> ~/.local/share/portico/panel.
Ran omarchy-shell shell rescanPlugins.
Ran omarchy plugin enable portico: Enabled portico
Wrote ~/.local/share/applications/portico.desktop.
Wrote ~/.local/share/icons/hicolor/scalable/apps/portico.svg.
Added the keybinding to ~/.config/hypr/bindings.lua (backup ~/.config/hypr/bindings.lua.portico-bak.1792432000).

Done.
Open the panel from the Portico icon in the bar or with SUPER + ALT + P.
Run portico desktop status to check it, and portico desktop uninstall to remove it.
```

### Plain Quickshell on Hyprland (`hyprland.conf` sources `bindings.conf`)

```
$ portico desktop install
Portico 0.6.0: desktop integration for Quickshell (qs found; no Omarchy).

Panel
  write    ~/.local/share/portico/panel/ (10 files and VERSION, 0.6.0)
  link     ~/.config/quickshell/portico -> ~/.local/share/portico/panel
  run      qs -c portico -n -d   (starts the panel now, hidden until toggled)

Launcher entry
  write    ~/.local/share/applications/portico.desktop (runs /home/you/.cargo/bin/portico)
  write    ~/.local/share/icons/hicolor/scalable/apps/portico.svg

Keybinding: SUPER + ALT + P, free (checked with hyprctl binds)
  backup   ~/.config/hypr/bindings.conf to ~/.config/hypr/bindings.conf.portico-bak.1792432000
  append   to ~/.config/hypr/bindings.conf (sourced by hyprland.conf), after line 40:
             # portico desktop install: begin
             bind = SUPER ALT, P, exec, qs -c portico ipc call portico toggle
             # portico desktop install: end

Install the panel? [y/N] y
Install the launcher entry? [y/N] n
Add the keybinding SUPER + ALT + P? Type y, n, or another combination [y/N] y

Wrote ~/.local/share/portico/panel (0.6.0).
Linked ~/.config/quickshell/portico -> ~/.local/share/portico/panel.
Ran qs -c portico -n -d.
Skipped the launcher entry.
Added the keybinding to ~/.config/hypr/bindings.conf (backup ~/.config/hypr/bindings.conf.portico-bak.1792432000).

Done.
Toggle the panel with SUPER + ALT + P.
Portico does not start the panel with your session. Add this line to ~/.config/hypr/hyprland.conf:
  exec-once = qs -c portico
Run portico desktop status to check it, and portico desktop uninstall to remove it.
```

### Uninstall

```
$ portico desktop uninstall
Portico's desktop integration:
  run      omarchy plugin disable portico
  remove   ~/.config/omarchy/plugins/portico (a link to ~/.local/share/portico/panel)
  run      omarchy-shell shell rescanPlugins
  backup   ~/.config/hypr/bindings.lua to ~/.config/hypr/bindings.lua.portico-bak.1792518400
  remove   lines 62-64 of ~/.config/hypr/bindings.lua:
             -- portico desktop install: begin
             o.bind("SUPER + ALT + P", "Portico", "omarchy-shell shell toggle portico")
             -- portico desktop install: end
  remove   ~/.local/share/applications/portico.desktop
  remove   ~/.local/share/icons/hicolor/scalable/apps/portico.svg
  remove   ~/.local/share/portico/panel/
Remove the desktop integration? [y/N] y
Ran omarchy plugin disable portico: Disabled portico
Removed ~/.config/omarchy/plugins/portico.
Ran omarchy-shell shell rescanPlugins.
Removed Portico's keybinding from ~/.config/hypr/bindings.lua (backup ~/.config/hypr/bindings.lua.portico-bak.1792518400).
Removed ~/.local/share/applications/portico.desktop.
Removed ~/.local/share/icons/hicolor/scalable/apps/portico.svg.
Removed ~/.local/share/portico/panel.
```

## Out of scope

- A TUI for any of this: a "TUI settings panel" (desktop integration and other preferences from the TUI) is added to `ROADMAP.md` as an unscheduled item.
- `--json` for `desktop status`.
- Writing the session autostart for plain Quickshell (printed, not written; see "Decided in review").
- Compositors other than Hyprland and Sway for the keybinding, and Omarchy versions before its Quickshell shell (without `omarchy-shell`, which are treated as plain Hyprland when `qs` is present).
- System-wide installs: `install` writes only under the user's home; packages ship the launcher entry, and the panel needs nothing outside the binary.
- Changing the panel's behaviour beyond the version banner.

## Testing

- **Unit tests** (`src/desktop/*.rs`): combination parsing and printing (aliases, order, case, a missing modifier, two keys); the `hyprctl binds -j` clash check against a captured listing (the three `P` bindings of the maintainer's machine, a submap bind, a mouse bind); the Sway parser (`$mod`, flags, includes, mode blocks); the line for each setup; appending and removing the marked block (missing final newline, a second block, an unterminated block, CRLF kept as found); the `hyprland.conf` `source` choice; `XDG_DATA_HOME` handling; the stamp and its hash; the refresh (missing `VERSION` does nothing, a different stamp rewrites and removes stray files, an unwritable directory fails silently and leaves the old stamp).
- **The embedded list** matches `desktop/quickshell/` minus `tests/` and `preview.png`, and the embedded launcher files equal `packaging/desktop/`.
- **Integration tests** (`tests/desktop.rs`) run the real binary in a temporary `HOME` with `XDG_*` pointing into it and `PATH` set to a directory of fake `omarchy-shell`, `omarchy`, `qs`, `hyprctl` and `swaymsg` scripts that log their arguments and answer from fixtures (a `hyprctl binds -j` listing, `omarchy plugin list --json`). They cover the Omarchy and plain-Quickshell transcripts above (the prompts answered through stdin), `--yes`, `--yes` with a taken combination (nothing changed, exit 1), `--bind`, `--no-bind`, the non-interactive refusal, no target found, a forced target, a link to elsewhere being replaced and a real directory being refused, a failing `omarchy plugin enable` (exit 1, later components still installed), `status` in each state, `uninstall` removing exactly what was created (byte-for-byte equality of the keybinding file with its pre-install content), the auto-refresh on an ordinary command (`portico hosts --json` with an old `VERSION` rewrites the panel and its stdout is still exactly the JSON document), and a refresh failure that leaves the command's output and exit status unchanged. No test touches the real `~/.config`.
- **Panel:** `tst_portico.qml` covers the banner function (equal versions, different versions, a missing or malformed stamp, `portico --version` output with a trailing newline); `qmllint` covers the `FileView` addition.
- **CI:** the binary now embeds `desktop/`, so a change there runs the Rust checks as well as the doc guards (the `changes` job of `ci.yml`).
- **Docs guard** (`tests/docs.rs`): the README's desktop section names `portico desktop install`; `packaging/aur/PKGBUILD` installs both launcher files.
- **Manual, on the maintainer's machine, read-only:** Omarchy's launcher lists entries from Quickshell's `DesktopEntries` (`/usr/share/omarchy/shell/services/AppLibrary.qml`), which scans `$XDG_DATA_HOME/applications`, and indexes icons under `~/.local/share/icons/**/apps/`. That `portico.desktop` parses and is listed is checked by running a throwaway Quickshell configuration that prints `DesktopEntries.applications` with `XDG_DATA_HOME` pointing at a temporary directory holding the generated files, without writing to the maintainer's `~/.local/share`. That Omarchy's own launcher then shows it needs the files in the real location, which this slice must not write; it goes in the pull request's manual check list, with the full `portico desktop install` on Omarchy.

## Decided in review

1. **The plain-Quickshell autostart line stays printed, not written,** in this slice.
2. **Omarchy's `o` helper is in scope in `bindings.lua`:** `o` is a global that `default.hypr.helpers` defines, and Omarchy's configuration requires it before `hypr.bindings`, so the written block needs no `require` or `local`.
3. **The Hyprland Lua names are the installed API:** `hl.bind(keys, dispatcher, { description = … })`, `hl.dsp.exec_cmd(cmd)` and `hl.on("hyprland.start", fn)` are declared in Hyprland 0.56's stubs (`/usr/share/hypr/stubs/hl.meta.lua`) and used by its default `hyprland.lua` and by Omarchy, so a Lua-configured plain Hyprland gets the `hl.bind` line.
4. **The auto-refresh never runs for `desktop` commands,** runs before the command writes anything, and costs one failed `open` when no panel is installed; a test proves `portico hosts --json` with no panel creates nothing under `XDG_DATA_HOME`.
5. **A development binary is flagged:** when the running binary lies under a `target/` directory of a cargo build, the plan warns that the launcher entry would run a build directory's binary.
6. **The manual Omarchy check list** (full install, the launcher shows Portico, SUPER + ALT + P toggles the panel, `uninstall` restores `bindings.lua` byte for byte) is in the pull request description, for the maintainer.
7. **Second review.** Edits of a linked configuration file go to the link's target; a link or file at the panel directory or at a launcher path is never followed; the refresh only moves a panel forward and never runs from a cargo build; Omarchy's paths are always under `~/.config`; backups are never overwritten; Sway's block-form `bindsym` is read; and declining the panel says why the keybinding is skipped.
