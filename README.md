# Portico

Portico is a fast, keyboard-first interface for managing OpenSSH hosts on Linux. It provides both a terminal UI and a direct CLI while leaving OpenSSH in control of connections and effective configuration.

Portico is currently an early V1 focused on host management. Tunnels, identity-provider discovery, Quickshell integration, and herdr integration are planned follow-up work.

## What V1 does

- Lists Portico-managed and existing OpenSSH hosts in one inventory.
- Adds, edits, inspects, renames, and removes managed hosts.
- Shows the effective configuration reported by `ssh -G`.
- Connects in the current terminal or through `xdg-terminal-exec`.
- Provides a master-detail TUI when run without a subcommand.
- Supports normal terminal paste into filters and host fields.
- Preserves existing SSH configuration and treats externally configured hosts as read-only.

Portico owns only `~/.ssh/portico/config`. Initialization adds an `Include ~/.ssh/portico/config` line to `~/.ssh/config` when needed and previews the change before applying it. Managed updates are validated with OpenSSH before activation and keep the previous valid fragment at `~/.ssh/portico/config.bak`.

Private keys remain where they already live. Portico stores identity-file references but never reads, copies, or takes ownership of private-key contents.

## Requirements

- Linux
- A recent stable Rust toolchain with Cargo
- OpenSSH client tools (`ssh`)
- `xdg-terminal-exec` for the new-terminal option only
- Python 3 and `stty` to run the complete PTY integration test

## Run from the repository

Build and inspect the available commands:

```sh
cargo build
cargo run -- --help
```

Initialize Portico. Without `--yes`, Portico displays the plan and asks before changing SSH configuration:

```sh
cargo run -- init
```

Other commands report that Portico is not initialized and direct you to run `portico init`.

Open the TUI:

```sh
cargo run --
```

Run any CLI command by placing it after `--`:

```sh
cargo run -- hosts
cargo run -- show dev-vps
```

For repeated local use, install the current checkout into Cargo's binary directory:

```sh
cargo install --path .
portico --help
```

Re-run `cargo install --path . --force` after rebuilding a version already installed this way.

## CLI

```text
portico                         Open the TUI
portico init [-y|--yes]         Initialize the managed SSH fragment
portico hosts                   List managed and external hosts
portico show <alias>            Show declared and resolved configuration
portico add <alias> -H <host>   Add a managed host
portico edit <alias> [options]  Change only the supplied fields
portico remove <alias> [-f]     Remove a managed host
portico connect <alias> [-n]    Connect using OpenSSH
```

Host fields and their aliases:

| Field | Options |
|---|---|
| Alias when editing | `--alias`, `-a` |
| Hostname | `--hostname`, `-H` |
| Username | `--user`, `-u` |
| Port | `--port`, `-p` |
| Identity file | `--identity`, `-i` |
| ProxyJump host | `--jump`, `-J` |

Add a host:

```sh
portico add dev-vps \
  --hostname dev.example.com \
  --user deploy \
  --port 2222 \
  --identity ~/.ssh/id_ed25519_dev \
  --jump bastion
```

Edit or rename a host:

```sh
portico edit dev-vps --hostname new-dev.example.com
portico edit dev-vps --alias development
```

Clear optional fields with repeatable `--unset`/`-U`. Accepted fields are `user`, `port`, `identity`, and `jump`:

```sh
portico edit development -U jump -U port
```

Inspect and connect:

```sh
portico hosts
portico show development
portico connect development
portico connect development --new-terminal
```

New-terminal connections are launched in the background, so the CLI returns immediately and the TUI remains usable.

Remove a managed host. Interactive terminals ask for confirmation; scripts must pass `--force`/`-f`:

```sh
portico remove development
portico remove development --force
```

Use `portico <command> --help` for the authoritative options for a command.

## TUI keys

| Key | Action |
|---|---|
| `↑` / `↓` | Move through hosts |
| `/` | Filter hosts |
| `a` | Add a host |
| `e` | Edit the selected managed host |
| `x` | Remove the selected managed host |
| `Delete` | Erase text in a filter or form field |
| `Backspace` / `Esc` | Cancel or return to the host list |
| `Enter` | Connect in the current terminal |
| `n` | Connect in a new terminal |
| `q` | Quit |

External hosts can be inspected and connected to, but Portico will not edit or remove them.
Use the terminal's normal paste shortcut, commonly `Ctrl+Shift+V`, to paste into the active filter or form field.

## Development checks

```sh
cargo test --all-targets
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

The automated tests use temporary SSH homes and do not connect to remote machines.

## Current scope

V1 deliberately does not include tunnels, port-conflict management, SSH-agent or password-manager discovery, key generation, Quickshell, herdr, a daemon, or a machine-readable API. Those features remain deferred until their workflows and integration contracts are clear.
