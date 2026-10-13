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

Portico owns only `~/.ssh/portico/config`. Initialization adds an `Include ~/.ssh/portico/config` line to `~/.ssh/config` when needed and previews the change before applying it. Managed updates are validated with OpenSSH before activation and keep the previous valid fragment at `~/.ssh/portico/config.bak`. Each update probes only the added or changed aliases, so its cost does not grow with the number of managed hosts. Host values are validated up front: hostnames and users are single values, identity files may contain inner spaces but not blank or padded values, and ProxyJump values contain no whitespace (OpenSSH validates their destinations).

Private keys remain where they already live. Portico stores identity-file references but never reads, copies, or takes ownership of private-key contents.

## Requirements

- Linux on x86_64 (verified on Arch Linux; other platforms are on the [roadmap](ROADMAP.md))
- OpenSSH client tools (`ssh`)
- `xdg-terminal-exec` for the new-terminal option only
- Rust 1.88 or newer, only when building from source
- Python 3 and `stty` to run the complete PTY integration test

## Install

### Arch Linux (AUR)

```sh
yay -S portico
```

The package builds from the tagged source; `cargo` is only a build dependency and is installed and removed for you.

### Release binary

Download the tarball and its checksum from the [latest release](https://github.com/YounesAmalou/Portico/releases/latest), then verify and unpack it:

```sh
sha256sum -c portico-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz.sha256
tar -xzf portico-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
install -Dm755 portico-vX.Y.Z-x86_64-unknown-linux-gnu/portico ~/.local/bin/portico
```

Release binaries are built on current Arch Linux, so they need a similarly recent glibc. On an older distribution, install with Cargo instead.

### Cargo

The crate is published as `portico-ssh` and installs the `portico` binary:

```sh
cargo install portico-ssh
```

### From source

```sh
git clone https://github.com/YounesAmalou/Portico
cd Portico
cargo install --path .
```

Then run `portico init` to set up, or `portico` to open the TUI.

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

Setting and unsetting the same field in one command (for example `--user deploy --unset user`) is rejected.

Inspect and connect:

```sh
portico hosts
portico show development
portico connect development
portico connect development --new-terminal
```

New-terminal connections are handed to `xdg-terminal-exec` in the background, so the CLI returns without waiting for the session and the TUI remains usable. If the launcher fails right away (for example no terminal is available) its exit status is reported; once it has started, Portico can only report that the launch was requested, not whether the SSH session succeeds.

Remove a managed host. Unknown and external (read-only) aliases are rejected before any prompt. Interactive terminals ask for confirmation; scripts must pass `--force`/`-f`:

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
| `Enter` | Connect in the current terminal |
| `Ctrl+Enter` / `n` | Connect in a new terminal |
| `Esc` | Cancel a filter edit or form, or clear the applied filter |
| `q` | Quit |

In the filter and the host form, type normally: `Backspace` and `Delete` erase text, `←` / `→` / `Home` / `End` move the cursor, `Enter` applies the filter or saves the form, and `Esc` cancels.
Form errors appear next to the field they belong to. `ProxyJump` takes `[user@]host[:port]` or an existing alias, with commas between several hops and no spaces.
Form fields only accept characters they can hold, whether typed or pasted: alias, hostname, user, port (digits only), and ProxyJump never take whitespace, and IdentityFile takes spaces inside a path but not at its start or end. Saving still validates everything.

`Ctrl+Enter` needs a terminal that reports modified keys, such as kitty, WezTerm, Ghostty, or foot, and is only shown in the footer when Portico detects one. `n` works everywhere.

Host details show each field once, as OpenSSH resolves it with `ssh -G`. Resolution runs in the background, so moving through the list never waits on it. A Portico-declared value is listed separately only when OpenSSH overrides it.

External hosts can be inspected and connected to, but Portico will not edit or remove them.
Use the terminal's normal paste shortcut, commonly `Ctrl+Shift+V`, to paste into the active filter or form field.

## Development checks

```sh
scripts/ci-checks.sh fmt
scripts/ci-checks.sh clippy
scripts/ci-checks.sh test
```

The same script also runs `msrv`, `audit`, `deny` and `package`, which is what continuous integration runs. The automated tests use temporary SSH homes, do not connect to remote machines, and must run as a regular user. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Contributions are accepted under the same terms.

## Current scope

V1 deliberately does not include tunnels, port-conflict management, SSH-agent or password-manager discovery, key generation, Quickshell, herdr, a daemon, or a machine-readable API. Those features remain deferred until their workflows and integration contracts are clear.
