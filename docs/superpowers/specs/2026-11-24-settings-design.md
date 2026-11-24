# Settings Design

**Status:** Approved; implemented

## Purpose

Portico now has a TUI tab for the hosts, the tunnels and each of three container backends (Docker, Podman, Proxmox). Not every user wants every tab: someone with only Docker sees two tabs that always report "not installed", and the desktop panel shows a Tunnels section to someone who has none. The desktop integration (the Quickshell panel, its launcher entry and keybinding) can only be managed from `portico desktop` today.

This slice adds a settings overlay to the TUI and `portico settings` for scripts, backed by one small Portico-owned preferences file. The overlay turns tabs on and off and holds the desktop integration's status and actions. It implements v4 slice 1 of `ROADMAP.md`, "Settings and preferences". The next slice, "Remembered container user", will store its data in the same file.

## Goals

- Turn the tunnels tab and each container backend's tab on or off, from the TUI and from the CLI. The hosts tab is always on.
- A turned-off tab is hidden from the TUI and from the desktop panel. Neither the TUI nor the panel ever runs a turned-off backend's tool (`docker`, `podman`, `pct`); the CLI, run by hand, does what it is told.
- The desktop integration's status (installed, out of date, missing pieces) and its install, update and uninstall actions are reachable from the TUI, with exactly the previews and confirmations of `portico desktop install | uninstall`.
- Portico works without the file, and its defaults match today's behaviour: everything on.

## Non-goals

- **The CLI is not affected by tab settings.** `portico containers`, `portico shell`, `portico tunnels` and `portico tunnel …` behave exactly as today whatever the file says. Settings shape the TUI and the panel only. (Decided by the maintainer.)
- No live reload of hand edits while the TUI runs: the TUI reads the file when it starts.
- No settings beyond tabs in this slice. The remembered container user is slice 2.
- No desktop integration status in the JSON output: the panel does not need it.

## Anti-goals

- A settings file that Portico rewrites wholesale, losing the user's comments or layout.
- A crash, or a silently ignored file, when the file is invalid.
- A second implementation of the desktop install flow that can drift from the CLI's.

## Rules this design keeps

No daemon, no database: the file is a small hand-editable TOML file Portico owns, written atomically. OpenSSH stays authoritative, and credentials are untouched.

## Settled questions

Decided by the maintainer before the spec was written.

- **File:** `$XDG_CONFIG_HOME/portico/settings.toml`, which defaults to `~/.config/portico/settings.toml`. The location comes from the platform layer, so macOS and Windows can choose their own. Format: TOML, edited with `toml_edit`, so a write changes only the value it sets and keeps comments, order and unknown keys.
- **Scope of a turned-off tab:** the TUI tab and the panel section only. The CLI keeps working as intended (see Non-goals).
- **Desktop actions in the TUI:** Install, Update and Uninstall suspend the TUI, as connecting to a host does, and run `portico desktop install` or `portico desktop uninstall` in the same terminal, with its own previews and prompts, including typing another key combination. The TUI comes back afterwards with the status refreshed.
- **Where the view lives:** an overlay opened with `,` from any browsing tab and closed with Esc, like the migrate screen. It is not a tab.

## Approaches considered

For the desktop actions:

1. **Run the CLI flow in place (chosen).** It reuses the exact previews, prompts and the keybinding's free-text answer. Nothing is duplicated, so the TUI and the CLI can't disagree.
2. **A native TUI screen**, like migrate: more integrated, but the prompt-driven install flow would have to be rebuilt as a plan the TUI drives, and both kept in step. Roughly twice the work.
3. **Status only**, naming the command to run: the smallest option, but it doesn't meet the roadmap's "actions tied to it".

For the view: a Settings tab, always last, was the alternative to the overlay. It is easier to find, but it changes the tab bar and its numbering. The maintainer chose the overlay.

For the file: `~/.ssh/portico/settings.toml` would keep everything Portico owns in one place, but preferences aren't SSH configuration. JSON would avoid a dependency, but it is harder to edit by hand and can't hold comments.

## Decisions

### The file

```toml
# ~/.config/portico/settings.toml
[tabs]
tunnels = true
docker = true
podman = false
proxmox = false
```

- Every key is optional, and a missing key takes its default, `true`. A missing file, or a missing `[tabs]` table, means every tab is on.
- **Invalid file:** TOML that does not parse, or a known key with the wrong type (`podman = "no"`), is invalid. Portico then uses the defaults and reports the problem: its path, the line and TOML's message, as one line. Nothing crashes.
- **Unknown keys and tables** are ignored when reading and kept when writing, so a newer Portico's file still works with an older one. They produce no warning.
- **Writing:** read-modify-write under the file lock of `platform::fs`. Re-read the file, set the one key with `toml_edit`, and write it atomically (temporary file, then rename, then a directory sync). Create `portico/` under the config directory if it is missing. A write over an invalid file is refused, with the parse error, so a bad hand edit is never overwritten. A new file starts with a one-line comment saying Portico owns it and it may be edited by hand.
- **Permissions:** the file is created `0644`, as other XDG configuration is. It holds no secret.

### Layers

Following `docs/architecture.md`:

- **`platform::paths`:** `config_dir(home, xdg_config_home: Option<&OsStr>) -> PathBuf` returns `$XDG_CONFIG_HOME/portico` when it is an absolute path, otherwise `~/.config/portico`. Like `runtime_dir`, the caller passes the environment value. Only `main` reads the environment.
- **`domain::settings`:** the `Settings` type (for now `tabs: Tabs { tunnels, docker, podman, proxmox }`), its defaults, `load(path) -> Result<Settings, SettingsError>` (where a missing file is `Ok(default)`), and `set(path, key, value)`. Keys are dotted names (`tabs.podman`) with an enum behind them, so the CLI, the TUI and the JSON agree on spelling. `Tabs::shows(BackendId)` answers for a backend.
- **`app`:** `App` gains the settings path (set by `main`, as with the runtime directory; tests point it at a temporary directory) and `settings()` / `set_setting(key, value)`, which both interfaces call.
- **`desktop`:** the desktop integration status that `portico desktop status` prints becomes a data structure (`desktop::Status`: the panel state, each target's link state, the launcher and the bindings), built by one function in `src/desktop`. The CLI's `status` prints it, and the TUI's overlay summarises it. `docs/architecture.md` then says `src/desktop/` is used by the CLI and the TUI.

### CLI

```
portico settings                       # every setting, its value, and the file's path
portico settings set tabs.podman off   # change one; on/off, true/false, yes/no
portico settings --json                # the document below
```

- `portico settings` prints one line per key: `tabs.podman  off  (default on)`. It ends with the path and says when the file does not exist yet. With an invalid file, it prints the problem and exits with status 1.
- `set` with an unknown key or value is an argument error that lists the valid ones. Setting a value it already has still writes nothing and exits 0.
- Only `portico settings` reads the file. Every other CLI command ignores it (see Non-goals).

### JSON

`portico settings --json`:

```json
{
  "path": "/home/me/.config/portico/settings.toml",
  "exists": true,
  "tabs": { "tunnels": true, "docker": true, "podman": false, "proxmox": false },
  "schema": 1
}
```

- With an invalid file, it is an error document with the new `error.kind` `settings`, whose message gives the path, the line and the reason. Adding an `error.kind` value is allowed within schema 1.
- `docs/json-output.md` gains a "Settings" section pinned by a golden file, as the other documents are.

### Containers for the panel

The panel lists a host's containers with `portico containers --json`, which picks the first installed backend. To respect the settings, `--backend` may now be given more than once, and in JSON mode it means "the first installed of these, in the order given". One `--backend` behaves as today. Without `--backend`, nothing changes. In human mode, several `--backend` values list each of them, the same as the no-flag listing restricted to them. This is a new option, not a change to existing behaviour.

### TUI

**Startup.** `tui::run` loads the settings through `app`. An invalid file shows its one-line problem in the status line, and the TUI runs with every tab on.

**Hidden tabs.**
- The tab bar shows only the tabs that are on.
- `1`–`n` number the visible tabs in order, and Tab and Shift+Tab cycle through them.
- `T` does nothing when the tunnels tab is off.
- `c` goes to the last container tab shown if it is still on, otherwise the first one that is on. It does nothing when every backend is off.
- No listing is sent to a backend that is off (its idle worker stays, so turning the tab back on lists it).
- If the tab being shown is turned off, the TUI switches to Hosts.
- The test that keeps `Tab::ALL` in step with `backend::AUTOMATIC` stays. Visibility is a filter over `Tab::ALL`, not a different list.

**The overlay (`,`).**

```
┌ Settings ─────────────────────────────────────────────┐
│ Tabs                                                  │
│ > [x] Tunnels                                         │
│   [x] Docker                                          │
│   [ ] Podman                                          │
│   [ ] Proxmox                                         │
│                                                       │
│ Desktop integration                                   │
│   Panel     installed, written by 0.9.0 (out of date) │
│   Launcher  installed                                 │
│   Keybind   SUPER + ALT + P                           │
│   [Update]  [Uninstall]                               │
│                                                       │
│ ~/.config/portico/settings.toml                       │
│ ↑↓ move · Space/Enter toggle or run · Esc close       │
└───────────────────────────────────────────────────────┘
```

- `,` opens it from any tab that is browsing (as `1`–`5` work today). It is a new `Mode`.
- Up and Down move over the tab toggles and the desktop actions. Space or Enter toggles a tab or runs an action.
- A toggle is written at once through `app` and applied when the overlay closes. If the write fails (for example, the file became invalid), the overlay shows the error and the setting stays as it was.
- Desktop actions:
  - The overlay offers two actions. **Install or update** is always offered and runs `portico desktop install`. That flow already reports pieces that are done, asks only about the rest, and explains why it refuses anything blocked. So the overlay does not try to sort pieces into missing, declined or blocked; it never would have the record to tell "declined" from "never installed". **Uninstall** is offered when anything Portico installed is present (the panel directory, a link of ours, a marked launcher entry or icon, or a binding between our markers). (Builder's call: `main` refreshes an out-of-date panel before the TUI starts, so "out of date" rarely shows, and a single action covers every case.)
  - The status lines are the same facts `portico desktop status` prints, shortened to one line per piece. Blocked states, a package-provided launcher and a disabled Omarchy plugin appear there as they do in the CLI.
  - An action suspends the TUI with `terminal::suspended`. It runs the current executable (`std::env::current_exe`) with `desktop install` or `desktop uninstall` as a child process in the same terminal, waits, prints "Press Enter to return to Portico", and resumes. Running a child keeps the TUI from calling the CLI module, so the interfaces stay independent. A child that cannot start, or that exits with an error, is reported in the overlay.
  - The status is rebuilt when the overlay opens and after each action.

### Desktop panel

- When the panel opens, and on F5, it runs `portico settings --json`. A failure or an error document means every tab is on, so the panel never loses features because of a settings problem.
- With tunnels off, the Tunnels section and the footer's tunnel hints are hidden.
- Containers:
  - When every backend is on, the panel lists a host's containers with `containers --json`, exactly as today.
  - Otherwise it passes one `--backend` per backend that is on, in the order Docker, Podman, Proxmox.
  - When no backend is on, hosts do not expand.
- Opening a container's shell from the panel passes the backend the row was listed with (`--backend <backend>`; a document without a `backend` key is Docker's). Otherwise `portico shell` without `--backend` would search every backend and run a turned-off one's tool. (Builder's call: this also makes the panel's shell exact when two backends list the same ID.)
- The panel's JS helpers (`containersCommand`, `shellCommand`) take the backends and are covered in `tst_portico.qml`. `fake-portico` answers `settings --json`.

## Testing

- `domain::settings`: defaults, a missing file, a partial file, unknown keys kept, a wrong type, a parse error with its line, a write that keeps comments and order, and a write refused over an invalid file.
- `platform::paths::config_dir`: XDG set, unset, relative.
- CLI: `settings`, `settings set`, `settings --json` golden files (valid, missing file, invalid file), a bad key or value, and the repeated `--backend` in JSON mode.
- TUI state tests: hidden tabs in the tab bar and the digit mapping, `T` and `c` with tabs off, switching to Hosts when the current tab is turned off, the overlay's toggles writing the file, and whether Uninstall is offered for each desktop state.
- An end-to-end test through the PTY: open `,`, turn Podman off, close, and check that the Podman tab is gone and the file changed. A second test runs the Install action against a fake Quickshell home and returns to the TUI.
- Desktop panel: `qmltestrunner` cases for the hidden Tunnels section and the backend arguments.

## Documentation

- README: the settings overlay, `portico settings`, and the file.
- `docs/json-output.md`: the Settings document, the `settings` error kind, and the repeated `--backend`.
- `docs/architecture.md`: `domain::settings`, `platform::paths::config_dir`, and `desktop` used by the CLI and the TUI.
- `ROADMAP.md`: remove slice 1 and keep slice 2, renumbering the rest. The v1 spec's "Delivered follow-up slices" gains a "Settings overlay" entry.

## Left to the builder

- The exact wording of the status-line and overlay messages.
- How the overlay draws on narrow terminals (it scrolls like the migrate checklist).
- Whether `desktop::Status` lives in `src/desktop.rs` or a new `src/desktop/status.rs`.
