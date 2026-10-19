# Desktop Panel Design

**Status:** Approved; implemented

## Purpose

Connecting to a host or starting a saved tunnel today means opening a terminal and typing a `portico` command. This slice adds a desktop panel: a small Quickshell component that lists the saved tunnels with their status and a start or stop action, and the hosts (and, on request, their containers) to open in a new terminal. It is a separate consumer of the CLI: it runs `portico` with `--json` when it opens, parses the documents described in `docs/json-output.md`, and runs `portico connect`, `portico shell` and `portico tunnel start|stop` for its actions. It writes nothing, stores nothing and duplicates no management: adding, editing, removing and adopting hosts and tunnels stay in the TUI and CLI.

It implements v3 item 1 of `ROADMAP.md`, "Desktop surface (Quickshell)". It is the last v3 slice and builds on "Tunnels" and "Machine-readable output".

## Settled questions

- **One panel, three thin hosts.** The shared part is a view (`PorticoView.qml`) and a model (`PorticoModel.qml`) over a pure parsing library (`portico.js`). Three entry points host them, each only a window or a dropdown around the view:

  | Entry point | Setup | Surface |
  |---|---|---|
  | `shell.qml` | Plain Quickshell (`qs -p desktop/quickshell`) | A launcher-style popup |
  | `OmarchyBarWidget.qml` | Omarchy plugin, `barWidget` | A bar icon whose click opens the view as a dropdown under it |
  | `OmarchyPanel.qml` | Omarchy plugin, `panel` | The same launcher-style popup, summoned by a keybinding |

- **Bar widget versus launcher popup: both, where each fits.** A bar widget needs a bar to live in, and only Omarchy's bar has a widget API a third party can target (`BarWidget`, `BarIconButton`, `KeyboardPanel` from `qs.Ui`, as the first-party Tailscale panel and the Motion Wallpaper plugin use). A plain Quickshell setup has its own bar, written by its user, so there the panel is a launcher-style popup: a centred layer-shell window with exclusive keyboard focus, toggled over Quickshell IPC (`qs -p <dir> ipc call portico toggle`), which the user binds to a key. On Omarchy the bar icon is the primary entry, and the `panel` entry point gives the same popup for a keybinding (`omarchy-shell shell toggle portico '{}'`), which is also how the panel is opened without the mouse. The popup is keyboard-first: typing filters, the arrow keys move, Enter acts, Right or Space expands a host, Escape closes.

- **Connecting opens a terminal through Portico, not through the panel.** The panel runs `portico connect --new-terminal -- <alias>` for a host, and `portico shell --new-terminal --on <alias> -- <id>` (without `--on` for this machine) for a container. Portico already asks `xdg-terminal-exec` for the terminal, runs itself there with `--hold` so a failure is explained on screen, and returns within half a second (`LAUNCH_GRACE`) with exit status 0 once the launch was requested. The panel therefore knows nothing about terminals, and the user's `xdg-terminal-exec` configuration (on Omarchy, the default terminal) is honoured. `--` keeps an alias or ID that starts with `-` from being read as a flag. On exit status 0 the popup closes; on any other status it stays open and shows, under the row, the last non-empty line Portico printed on stderr (human text, shown as is and never parsed), or "Portico could not open a terminal (exit status N)". The container shell opens as the image's default user: choosing another user stays in the CLI and TUI (`-u`), and remembering one is the roadmap's "Remembered container user".

- **Status refreshes without a daemon.** Nothing runs while the panel is closed. Opening it runs `portico hosts --json` and `portico tunnels --json`, which read files only (no `ssh`, no agent, no docker). While it stays open, `portico tunnels --json` runs again every 5 seconds, so a tunnel that fails or is started from a terminal shows up; a refresh never overlaps the previous one (a tick is skipped while one runs). The host list is read once per opening, plus on the refresh key (`F5` or Ctrl+R). The Quickshell process that hosts the panel is the user's shell, not a Portico service: Portico still runs only when the panel runs it, and every started tunnel is the ordinary `ssh` process Portico starts from the CLI.

- **Start and stop run asynchronously, one process per action.** Clicking Start runs `portico tunnel start --json -- <name>` (Stop: `tunnel stop`) in its own `Process`; the row shows a busy state and its action is disabled until the document arrives, which can take up to the host's `ConnectTimeout` for a start and about 3 seconds for a stop (`docs/json-output.md`, "Start and stop"). Other rows stay usable, and several tunnels can be starting at once. The result's `tunnel` report replaces the row, so no extra listing is needed; a `failed` outcome shows `error.message` and the first `error.next` under the row. A periodic refresh that began before an action finished could carry the old status, so the model counts finished actions and drops a refresh result whose run started before the latest one. The processes belong to the model, which outlives the popup (the shell root in plain Quickshell, the bar widget on Omarchy, and a `keepLoaded` panel), so closing the popup never kills a start half-way. A reload is different: Quickshell kills the processes of a component it destroys (Omarchy reloads a plugin when one of its files is saved, and `qs` reloads its configuration the same way), so every action runs as `setsid --fork --wait <portico> …`. Killing `setsid` then leaves Portico running in its own session to finish the start or stop and write its record; only the document is lost, and the next opening (or the next refresh of a reloaded panel that is open) reads the real status from `portico tunnels --json`. `setsid` (util-linux) is looked up with `portico`; without it, actions run directly.

- **Containers load only when a host is expanded.** The panel never runs `portico show` (it may wake a locked agent) and never lists containers up front. Expanding a host runs `portico containers --json --on <alias>` once; the result (or its error) is kept while the panel stays open, and is dropped on the next opening, so reopening shows a fresh listing. An expanded host has its own refresh action. While the listing runs (`BatchMode=yes`, so it cannot prompt, but it can take the host's `ConnectTimeout`), the host shows a loading line and can be collapsed; the result is still kept. Errors are shown under the host: `message`, then each `next` step (docker's own offers name the command to run in a terminal). A "This machine" row above the hosts expands to `portico containers --json` for local containers; it has no connect action.

- **What the view shows.** Two sections under one filter field: **Tunnels** first (usually few, and the ones with live state), then **Hosts**. The filter matches tunnel names and hosts and host aliases, case-insensitively by substring; a fuzzy matcher is not worth duplicating in JavaScript for a short list.
  - A tunnel row shows the name, `forward.description`, the host, and a state: Running, Stopped, Failed (with `reason`), or Port in use (with `address`, and "held by <by>" when another saved tunnel holds it). Its action is Stop when running, Start otherwise. Warnings (`unknown-host`, `collides-with`, and any kind added later) show as one dimmed line with their `message`.
  - A host row shows the alias and, dimmed, `external` for an external host. A duplicate alias is shown once, the first one, as `ssh` uses the first match; the `warnings` of `hosts --json` are shown at the foot of the list.
  - A container row shows `display_name`, then `image` and `status` dimmed; activating it opens the shell.
  - All text from documents is rendered with `Text.PlainText`: aliases, container names and statuses come from files and docker, and Qt's default `AutoText` would interpret markup in them.

- **Files live in `desktop/quickshell/`, one self-contained directory** that is both a Quickshell configuration and an Omarchy plugin:

  | File | Role |
  |---|---|
  | `shell.qml` | Plain Quickshell entry: the popup window and its `IpcHandler` (`toggle`, `open`, `close`) |
  | `manifest.json` | Omarchy manifest: `kinds` `bar-widget` and `panel`, id `portico` |
  | `OmarchyBarWidget.qml`, `OmarchyPanel.qml` | The Omarchy entry points, the only files importing `qs.Commons` and `qs.Ui` |
  | `PorticoView.qml` | The shared view; colours and font come from a `theme` property each host sets (Omarchy's `Color` and the bar's font, or built-in defaults) |
  | `PorticoModel.qml` | Runs the CLI, owns state, timers and actions |
| `PorticoRun.qml` | One `Quickshell.Io.Process` run: reports stdout, stderr and the exit status once the process and both streams have ended, or that it never started |
| `PorticoPopup.qml` | The launcher-style popup window, shared by `shell.qml` and `OmarchyPanel.qml` |
  | `portico.js` | Pure functions (`.pragma library`, no Qt types): check `schema`, turn each document into rows, describe failures |
  | `tests/` | The automated tests and the fake `portico` described under "Testing" |
  | `README.md` | A short pointer to the README section, for someone browsing the directory |

  One directory, rather than a shared folder plus an Omarchy folder, because Omarchy loads a third-party plugin from a single directory and a plugin that reached outside it (`../`) would break as soon as it is copied. Plain Quickshell only instantiates what `shell.qml` uses, so the Omarchy files there are inert.

- **Finding `portico`.** A session started by the compositor often lacks `~/.cargo/bin` on `PATH`. Each opening resolves the program once, in this order: `$PORTICO_BIN` when set, `portico` on `PATH`, then `~/.cargo/bin/portico` (where `cargo install` puts it). When `$PORTICO_BIN` is set it is the only candidate, so a wrong value is reported rather than silently bypassed. The lookup is one fixed `sh -c` script that runs `command -v` on its positional arguments; the candidates are passed as those arguments and never interpolated into the script, and `~/.cargo/bin` is built from the `HOME` the panel sees, not a literal tilde. Every Portico command itself is run as an argument list, never through a shell. A program that vanishes between the lookup and a run is reported the same way: Quickshell signals it by `running` turning false without `started`, and no `exited`. When none is found, the view shows only "Portico was not found", the three places it looked, how to set `PORTICO_BIN` (in Hyprland, `env = PORTICO_BIN,/path/to/portico`), and the view's Refresh action looks again.

- **Checking the contract.** Every document goes through one function, `portico.parse(stdout, exitStatus)`, before anything reads it:
  - **Not JSON, or no `schema`:** "This Portico does not print the JSON this panel reads; it needs a Portico release with `--json`." Exit status 2 with stderr text is what an older Portico prints for an unknown `--json` flag, and gets the same message.
  - **`schema` other than 1:** "This Portico prints schema N; this panel reads schema 1. Update the panel" (N > 1) or "Update Portico" (N < 1). Nothing else from the document is read, as `docs/json-output.md` asks.
  - **Schema 1 with `error`:** the error's `message` and `next`, scoped to where it happened (the Hosts or Tunnels section for `hosts` and `tunnels`, so an uninitialised setup still shows the tunnels; the host for `containers`; the row for `start` and `stop`).
  - Unknown keys are ignored; an unknown `error.kind`, warning `kind` or docker `problem` is treated as `other`. Values the contract only adds under a new schema (an unknown `status.state` or `outcome`) are still handled defensively: the row shows the raw state and offers no action.

  A schema problem in one document is a view-wide banner, since every other command runs the same Portico.

- **Installing is a documented copy or symlink; nothing installs itself.** The README gains a "Desktop panel" section:
  - **Plain Quickshell:** try it with `qs -p <checkout>/desktop/quickshell` and `qs -p <checkout>/desktop/quickshell ipc call portico toggle`. To keep it, link it as a named configuration (`ln -s <checkout>/desktop/quickshell ~/.config/quickshell/portico`), start `qs -c portico` with the session, and bind `qs -c portico ipc call portico toggle` to a key.
  - **Omarchy:** `ln -s <checkout>/desktop/quickshell ~/.config/omarchy/plugins/portico` (Omarchy's scanner follows the link and reloads on save) or copy the directory there, then `omarchy-shell shell rescanPlugins` and `omarchy plugin enable portico`; bind `omarchy-shell shell toggle portico '{}'` for the popup. `omarchy plugin add <url>` does not apply, because it expects a repository whose root is the plugin.
  - A link follows the checkout, so a `git pull` updates the panel; the `schema` check covers a panel and a Portico from different releases.

  The repository ships no installer: the steps are short, and they write into the user's configuration, which the user should do knowingly.

- **The roadmap's `plugin.json` is `manifest.json`.** Omarchy's plugin contract (`/usr/share/omarchy/shell/README.md`, "Plugin manifest", and the plugins in `~/.config/omarchy/plugins`) names the file `manifest.json` with `kinds` and `entryPoints`; the plugin follows the real format.

## Decisions

### Model

`PorticoModel.qml` is a `QtObject`-style component with:

- `program` (the resolved path, or empty) and `problem` (a view-wide problem: not found, schema, or a `hosts`/`tunnels` error document), so the view shows either the problem or the lists;
- `tunnels` and `hosts` (arrays of row objects from `portico.js`), `hostWarnings`, and `containers`, a map from target (`""` for this machine, otherwise the alias) to `{state: "loading" | "loaded" | "failed", rows, error}`;
- `busy`, a map from tunnel name to `"starting"` or `"stopping"`, and `rowErrors`, a map from a row key (`tunnel:<name>`, `host:<alias>`, `container:<target>/<id>`) to the error shown under it;
- `open()` (resolve the program, list hosts and tunnels, start the 5-second timer, drop cached containers and old row errors), `close()` (stop the timer; running actions continue), `refresh()`, `expand(target)`, `reloadContainers(target)`, `connect(alias)`, `openShell(target, id)`, `start(name)`, `stop(name)`, and a `requestClose` signal the hosts connect to their window.

Each command is one `Process` created for the run and destroyed after its `exited` signal, with stdout collected to its end (`StdioCollector`) and parsed once, as the contract asks. A `Process` that fails to start (the program vanished) is reported like a missing program.

### Library (`portico.js`)

Pure functions, no Qt and no I/O, so they can be tested against the golden files: `parse(stdout, exitStatus)` → `{ok, doc}` or `{problem}`; `hostRows(doc)` (dedupe, ownership), `tunnelRows(doc)` and `tunnelRow(report)` (label, description, state text, detail, action, warnings), `containerRows(doc)`, `actionResult(doc, name)` (the replacement row and an optional error), `errorLines(error)` (`message` plus `next`), `filter(rows, text)`, and `SCHEMA = 1`. Every reader tolerates missing optional fields by showing nothing for them rather than throwing, so a malformed document becomes a problem message, never a broken view.

### Theme

`PorticoView` takes a `theme` object (`foreground`, `dim`, `accent`, `urgent`, `background`, `fontFamily`, `fontSize`). The Omarchy entries fill it from `qs.Commons` (`Color`, `Style`) and the bar, so the panel follows the Omarchy theme; `shell.qml` uses neutral dark defaults. The view's markers are plain Unicode (`●`, `▸`, `▾`), so it needs no icon font; only the Omarchy bar icon is a Nerd Font glyph, as Omarchy's own widgets are.

## Out of scope

- Adding, editing, removing or adopting hosts and tunnels; testing connections; showing identities or host details. The panel links to none of them: the TUI is one command away.
- Choosing or remembering a container user, and container engines other than Docker (both on the roadmap).
- A live status in the bar icon, which would need polling while closed.
- Packaging the panel files (AUR, release tarball; a "Production readiness" row), and an Omarchy menu entry.
- Any change to Portico itself: the slice uses the existing CLI and `--json` contract unchanged.

## Testing

- **JavaScript against the golden files (`desktop/quickshell/tests/tst_portico.qml`, Qt Quick Test):** `qmltestrunner` runs `portico.js` in Qt's own JavaScript engine, the one Quickshell uses, with `QT_QPA_PLATFORM=offscreen`. It reads every file in `tests/fixtures/json/` (file reads enabled with `QML_XHR_ALLOW_FILE_READ=1`) and checks that each parses with schema 1, then checks the rows: `hosts.json` gives three hosts with `bastion` once and its duplicate warning; `tunnels.json` gives Port in use (`127.0.0.1:5433`, no holder), Stopped with a collision warning, Failed with its reason, and a dynamic tunnel's description, each with the right action; `containers-on-host.json` and `containers.json` give two rows with their IDs and names; `tunnel-start.json` gives a failed row for `preview` (with the server's `next` step) and an `unknown-tunnel` error for `nope`; `tunnel-stop-all.json` gives no results; `containers-docker-down.json`, `error-usage.json` and `error-unknown-host.json` give their messages and steps. Synthetic cases cover schema 2, schema 0, a missing `schema`, invalid JSON, empty stdout, exit status 2 with no JSON, an unknown `error.kind` and warning kind (treated as `other`), an unknown `status.state` (no action), a running tunnel (Stop), and the filter.
- **Lint:** Qt 6's `qmllint` (`/usr/lib/qt6/bin/qmllint`; `/usr/bin/qmllint` may be Qt 5's) on every QML file, with Quickshell's installed modules on the import path. The Omarchy entry points import `qs.Commons` and `qs.Ui`, which exist only inside Omarchy's shell, so they are linted with unresolved-import warnings disabled.
- **One command:** `scripts/ci-checks.sh desktop` runs both (`qmllint` with `--max-warnings 0`, since its default exit status ignores warnings), plus Omarchy's own `omarchy-plugin-validate` on the directory when Omarchy is installed, and fails with the package to install (`qt6-declarative`, `quickshell`) when a tool is missing. With Omarchy installed, the Omarchy entry points are linted against its shell (`qs.*` resolved through a temporary import directory).
- **CI:** no new required job. An optional `desktop` job, outside branch protection, runs `scripts/ci-checks.sh desktop` when `desktop/`, `tests/fixtures/json/` or `docs/json-output.md` change, through a new `desktop` output of the existing `changes` job (set only by a diff, so a release never runs it). The job itself always runs and only its steps are skipped, because the release workflow reuses a CI run only when every job of it concluded `success`, and a skipped job would make every release run CI again; `continue-on-error` keeps a failure from failing the workflow. Branch protection names its required checks, so the `desktop` job cannot block a merge, and no job depends on it; Arch's `extra` repository carries `quickshell`, so the job installs `qt6-declarative` and `quickshell` in an `archlinux:base-devel` container. In the required `test` job (`tests/docs.rs`, which docs-only changes already run): `portico.js` declares `SCHEMA = 1` and every golden file has the same `schema`, so a schema bump cannot leave the panel behind unnoticed; every `entryPoints` file named in `manifest.json` exists; and the README's install steps name `desktop/quickshell`.
- **Fake `portico` (`desktop/quickshell/tests/fake-portico`):** a shell script that answers the panel's commands with the golden files (start answers after a delay, to show the busy state), so the panel can be run and screenshotted without real hosts: `PORTICO_BIN=desktop/quickshell/tests/fake-portico qs -p desktop/quickshell`, then `ipc call portico open`.
- **The argument forms the panel runs** (`connect --new-terminal -- <alias>`, `shell --new-terminal --on <alias> -- <id>`, `tunnel start --json -- <name>`, `tunnel stop --json -- <name>`, `containers --json --on <alias>`) are each pinned by a CLI test in the Rust suite, so a later CLI change cannot break the panel unnoticed.
- **Surviving a reload:** with the fake, a start that is still running when `qs` exits finishes and leaves its marker, because it runs under `setsid --fork --wait`; without `setsid` it would be killed with the panel.
- **Manual, on the maintainer's machine:** the plain Quickshell popup is run from the repository path with the fake and with the real `portico` (open, filter, expand a host, connect, open a container shell, start and stop a tunnel, the not-found and schema messages via `PORTICO_BIN`), without touching `~/.config`. The Omarchy entry points cannot be verified here without installing into `~/.config/omarchy/plugins`, which this slice must not do: they are linted, and their manual check list (link the directory, enable the plugin, use the bar icon and the keybinding, then remove the link) is in the pull request description for the maintainer, not in the repository's documentation.

## Decided in review

1. **No live status in the bar icon.** The icon stays static and nothing runs while the panel is closed, in keeping with "poll the CLI on open". It stays under "Out of scope".
2. **No packaging in this slice.** Shipping the panel in the AUR package and the release tarball (under `/usr/share/portico`) is a row of `ROADMAP.md`'s "Production readiness" table.
3. **No Omarchy menu entry.** The bar icon and the keybinding are the entry points; the roadmap's menu wording goes with the delivered item, and no manual menu steps are documented.
4. **Argument forms are pinned** by CLI tests (see "Testing").
5. **The optional CI job cannot block a merge** (see "Testing").
6. **The `portico` lookup takes its candidates as positional arguments** and expands `~/.cargo/bin` from the panel's `HOME`.
7. **Actions survive a reload** by running under `setsid --fork --wait`, and the panel recovers the real status on its next listing.
8. **The manual Omarchy check list is in the pull request description,** not in the repository.
