# Portico tray app

Portico in the macOS menu bar and the Windows notification area: the saved tunnels with a start or stop action, and the hosts (and their containers) to open in a new terminal. It runs `portico` and reads its `--json` documents ([`docs/json-output.md`](../../docs/json-output.md)), as the Linux [Quickshell panel](../quickshell/README.md) does. Its design: [`docs/superpowers/specs/2026-11-26-desktop-interfaces-design.md`](../../docs/superpowers/specs/2026-11-26-desktop-interfaces-design.md).

`portico desktop install` downloads it from the GitHub release of the same version. It is its own Cargo project (package `portico-tray`, never published), outside Portico's crate.

| Path | Role |
|---|---|
| `src/model.rs` | Reads Portico's JSON documents into rows, and builds the argument lists the app runs: pure |
| `src/layout.rs` | The app's state, how answers and clicks change it, and the menu it describes as plain data: pure |
| `src/portico.rs` | Finds and runs `portico` (argument lists, never a shell) off the menu's thread, and starts the self-update |
| `src/instance.rs` | One app per user session |
| `src/menu.rs`, `src/app.rs` | macOS and Windows only: the native menu, and the event loop with the tray icon |
| `src/main.rs` | Arguments (`--portico <path>`, `--version`); on Linux it only says to use the Quickshell panel |
| `build.rs` | Windows: embeds the icon, the version and a manifest in the executable |
| `macos/Info.plist` | The bundle's plist (`@VERSION@` is set when the bundle is built) |
| `assets/` | `tray-template.png` (the macOS menu bar template, from `tray-template.svg`), `tray.png` (the Windows notification icon), `portico.ico` and `portico.icns`, all from `packaging/desktop/portico.svg` |

On Linux the pure parts build and are tested; the menu and the event loop are checked for Windows with `cargo clippy --target x86_64-pc-windows-gnu --all-targets` and run on the macOS and Windows CI runners. Check it with `scripts/ci-checks.sh tray` from the repository root.

The icons were rendered with `rsvg-convert` (`tray.png` at 32 px, `tray-template.png` at 44 px from `tray-template.svg`, the glyph of `portico.svg` in black on transparent), and `portico.ico` and `portico.icns` written with Pillow from renders of `portico.svg`.
