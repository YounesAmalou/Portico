# Portico desktop panel

A Quickshell panel for Portico: the saved tunnels with a start or stop action, and the hosts (and their containers) to open in a new terminal. This directory is both a plain Quickshell configuration (`shell.qml`) and an Omarchy plugin (`manifest.json`).

Installing and using it: the "Desktop panel" section of the [README](../../README.md#desktop-panel). What it reads: [`docs/json-output.md`](../../docs/json-output.md). Its design: [`docs/superpowers/specs/2026-10-19-desktop-panel-design.md`](../../docs/superpowers/specs/2026-10-19-desktop-panel-design.md).

| File | Role |
|---|---|
| `shell.qml` | Plain Quickshell entry: the popup, toggled over IPC |
| `OmarchyBarWidget.qml`, `OmarchyPanel.qml` | Omarchy entries: the bar icon with its dropdown, and the popup for a keybinding |
| `PorticoView.qml`, `PorticoPopup.qml` | The shared view, and the popup window around it |
| `PorticoModel.qml`, `PorticoRun.qml` | Runs `portico` and holds what it said |
| `portico.js` | Reads the JSON documents: pure functions, tested by `tests/tst_portico.qml` |
| `tests/fake-portico` | Answers the panel from the golden files, to run it without real hosts |
| `preview.png` | The panel with the fake, for the README and Omarchy's plugin listing |

The `portico` binary embeds these files, except `tests/` and `preview.png`, and `portico desktop install` writes them with a `VERSION` stamp, which the panel compares with `portico --version` to show a banner when they differ.

Check it with `scripts/ci-checks.sh desktop` from the repository root.
