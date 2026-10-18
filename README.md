# Portico

Portico is a fast, keyboard-first interface for managing OpenSSH hosts on Linux. It provides both a terminal UI and a direct CLI while leaving OpenSSH in control of connections and effective configuration.

Portico manages hosts, adopts existing ones from your own SSH configuration, tests connections on demand, and opens shells in Docker containers. Tunnels, identity-provider discovery, and a Quickshell integration are planned follow-up work (see the [roadmap](ROADMAP.md)).

## What V1 does

- Lists Portico-managed and existing OpenSSH hosts in one inventory.
- Adds, edits, inspects, renames, and removes managed hosts.
- Shows the effective configuration reported by `ssh -G`.
- Connects in the current terminal or through `xdg-terminal-exec`.
- Adopts hosts from your own SSH configuration into Portico, and exports them back, with a preview first.
- Tests on demand whether a host's login works, without opening a shell.
- Lists the running Docker containers on this machine or a host, grouped by Compose project, and opens a shell in one.
- Provides a master-detail TUI when run without a subcommand.
- Supports normal terminal paste into filters and host fields.
- Preserves existing SSH configuration and treats externally configured hosts as read-only until you adopt them.

Portico owns only `~/.ssh/portico/config`. Besides `init`'s one-line `Include`, `adopt` and `export` are the only operations that change your own SSH configuration files, and each shows a preview and asks for confirmation first (unless `--yes`). Trusting a host key in the connection test appends it to your `known_hosts` file. Initialization adds an `Include ~/.ssh/portico/config` line to `~/.ssh/config` when needed and previews the change before applying it. Managed updates are validated with OpenSSH before activation and keep the previous valid fragment at `~/.ssh/portico/config.bak`. Each update probes only the added or changed aliases, so its cost does not grow with the number of managed hosts. Host values are validated up front: hostnames and users are single values, identity files may contain inner spaces but not blank or padded values, and ProxyJump values contain no whitespace (OpenSSH validates their destinations).

Private keys remain where they already live. Portico stores identity-file references but never reads, copies, or takes ownership of private-key contents.

## Requirements

- Linux on x86_64 (verified on Arch Linux; other platforms are on the [roadmap](ROADMAP.md))
- OpenSSH client tools (`ssh`)
- `xdg-terminal-exec` for the new-terminal option only
- `docker`, only for container shells, on this machine or on the host (Docker only; Podman is not supported)
- `fzf`, optional, for the `portico shell` picker (a numbered menu is used without it)
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
portico test <alias> [--raw]    Check that logging in works, without a shell
portico containers [--on <alias>]
                                List running containers, here or on a host
portico shell [<container>] [--on <alias>] [-u <user>] [-n]
                                Open a shell in a container
portico adopt [alias...] [-A|--all] [-y|--yes]   Move external hosts into Portico
portico export [alias...] [-A|--all] [-y|--yes]  Move managed hosts back to ~/.ssh/config
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

Test a connection:

```sh
portico test development
portico test development --accept-host-key SHA256:...
```

A test passes only when OpenSSH completes authentication. It never opens a shell or runs a command, so hosts that only allow port forwarding pass too, and it is never run automatically. It works for managed and external hosts, prints one line on a pass (`development: authenticated as deploy@10.0.0.5:22 (publickey)`, with `via bastion` before the method when the host is reached through a jump host, or `via proxy` through a `ProxyCommand`) and exits 0. A failure exits 1 and prints `development: failed — <what went wrong>` followed by labelled lines such as `user:`, `offered:` (the keys ssh tried), `key:`, `file:`, `line:`, `jump:` and `next:` (what to do about it). `--raw` also prints ssh's own output under `ssh said:`; for a failure Portico does not recognise, that output is always shown. Interrupting a test (`Ctrl+C`) stops its `ssh`, prints `<alias>: cancelled` and exits 130. The test never prompts for a password or passphrase, so a key with a passphrase must be loaded in an agent; if it is not, the test says so and suggests `ssh-add`.

The first time a host is tested its key is usually unknown. In a terminal, Portico shows the key's fingerprint and asks before trusting it. Without a terminal it trusts nothing, prints the fingerprint and fails; `--accept-host-key` trusts the key only if its fingerprint matches the one given exactly, then tests again. A changed host key is never offered for trust.

### Container shells

```sh
portico containers
portico containers --on dev-vps
portico shell --on dev-vps
portico shell web --on dev-vps
portico shell myapp/api#2 -u root
portico shell 3f9a1c -n
```

`portico containers` lists the running containers on this machine, or on a host with `--on <alias>`, grouped by Compose project with the rest under "Other". `portico shell` opens a shell in one (bash when the container has it, sh otherwise), as the image's default user unless `-u` names another, and `-n` does it in a new terminal. Without a container, `portico shell` asks in a terminal: `fzf` when it is installed, a numbered menu otherwise. Cancelling exits with 130.

A container is referred to by its name, `project/service` (add `#n` for the n-th replica), a Compose `service` name, or an ID prefix of at least 4 hex characters, and is resolved against a fresh listing. A reference that matches nothing, or several containers, is an error naming the target and listing the candidates.

Only Docker is supported, and Portico stores nothing for this: no container list, user or setting is ever written to disk. Listing on a host uses `ssh` with your configuration; the CLI lets ssh prompt, the TUI never does (`BatchMode`), so a host that needs a password or passphrase must be reachable through an agent or an open connection.

When Docker fails, Portico explains why and, where it can, offers the fix. The full command is shown first and `sudo` prompts in your terminal; Portico never sees a password. Declining prints the options to do it yourself.

- **Not in the `docker` group:** it offers `sudo` to create the group, add you to it and give the group the socket. Members of the group have root-equivalent access.
- **Daemon not running:** it offers `sudo systemctl start docker`, only when `systemctl` exists on the target.
- **Group added but this session is older:** it runs docker through `sg docker` (or `newgrp docker`), so no logout is needed. Other shells need `newgrp docker` or a new login.

### When something fails

- **`portico test`** explains the failure as described above: the cause, the facts that matter (user, keys offered, jump host) and a next step. Add `--raw` to see ssh's output too.
- **`portico connect`** shows ssh's own output unchanged and always exits with ssh's exit code, or 128 plus the signal number when a signal ends ssh, as a shell reports it. When ssh exits with 255 for a cause Portico recognises, it adds a blank line and the explanation. A connection opened in a new terminal stays open after such a failure, showing `Press Enter to close`, so the message can be read.
- **Saving** (`add`, `edit`, `init`): when OpenSSH rejects a value, the error names the host and the field (the TUI form focuses that field). When the problem is a line in your own SSH configuration, it names that file and line (`line 12 of ~/.ssh/config`), and nothing is saved until you fix it. `portico test` and `portico connect` explain a configuration OpenSSH rejects the same way, naming the file and line.
- **In the TUI**, the status line shows the short explanation and the details pane shows the facts and next steps for the last test and the last connection. Press `o` to unfold ssh's own output in the details pane and `o` again to fold it; in a form, `Ctrl+O` does the same for the test result. The footer offers the key only when there is output to show.

Remove a managed host. Unknown and external aliases (adopt them first) are rejected before any prompt. Interactive terminals ask for confirmation; scripts must pass `--force`/`-f`:

```sh
portico remove development
portico remove development --force
```

### Adopt and export hosts

`portico adopt` moves a host from your own SSH configuration into Portico, comments included, so you can edit it there; `ssh <alias>` behaves exactly as before. Only a simple block qualifies: a single concrete alias, declared once, using only `HostName`, `User`, `Port`, `IdentityFile` and `ProxyJump`, in a file you own and can write, where OpenSSH reports the same effective configuration after the move. Every other host is listed with the reason it cannot be adopted.

```sh
portico adopt                   # choose from a list
portico adopt dev-vps --yes
portico adopt --all
```

Portico shows an exact preview of both files first and, unless you pass `--yes`, asks before changing anything. Without `--yes` it asks in a terminal; scripts must pass `--yes` and name hosts or `--all`. Each file is backed up beside the original for the duration of the move and the backup is deleted once it succeeds; if anything fails, the files are restored. If the configuration changed since the preview, nothing is written and you run the command again.

`portico export` is the way back: it moves managed hosts, comments included, into `~/.ssh/config` below the Portico include, with the same preview and options.

Use `portico <command> --help` for the authoritative options for a command.

## TUI keys

| Key | Action |
|---|---|
| `↑` / `↓` | Move through hosts |
| `/` | Filter hosts |
| `a` | Add a host |
| `e` | Edit the selected managed host |
| `x` | Remove the selected managed host |
| `r` / `F5` | Refresh: re-read the inventory and discard cached host details |
| `Enter` | Connect in the current terminal |
| `Ctrl+Enter` / `n` | Connect in a new terminal |
| `o` | Unfold or fold ssh's output for the last test or connection (`Ctrl+O` in a form) |
| `m` | Adopt the selected external host, or export the selected managed host, after a preview |
| `t` | Test the connection to the selected host (`Ctrl+T` tests the values typed in an add or edit form) |
| `Tab` / `Shift+Tab` | Move the focus between the hosts and the containers panel |
| `Esc` | Cancel a filter edit or form, or clear the applied filter |
| `q` | Quit |

In the filter and the host form, type normally: `Backspace` and `Delete` erase text, `←` / `→` / `Home` / `End` move the cursor, `Enter` applies the filter or saves the form, and `Esc` cancels.
Form errors appear next to the field they belong to. `ProxyJump` takes `[user@]host[:port]` or an existing alias, with commas between several hops and no spaces.
While you type a ProxyJump hop, a dropdown lists the known hosts whose alias contains what you typed, ignoring case and accents: `↑` / `↓` choose, `Enter` or `Tab` fills in the hop, and `Esc` closes the list (a second `Esc` cancels the form). In the ProxyJump field, `Ctrl+Space` opens the list even before you type.
Form fields only accept characters they can hold, whether typed or pasted: alias, hostname, user, port (digits only), and ProxyJump never take whitespace, and IdentityFile takes spaces inside a path but not at its start or end. Saving still validates everything.

`Ctrl+Enter` needs a terminal that reports modified keys, such as kitty, WezTerm, Ghostty, or foot, and is only shown in the footer when Portico detects one. `n` works everywhere.

Host details show each field once, as OpenSSH resolves it with `ssh -G`. Resolution runs in the background, so moving through the list never waits on it. A Portico-declared value is listed separately only when OpenSSH overrides it. Portico re-reads the inventory and re-resolves details when it changes the configuration itself; edits made elsewhere, such as in `~/.ssh/config` or an included file, appear after `r` or `F5`. The selection and filter are kept, and if the files cannot be read the list stays as it was and the footer reports why.

A test shows `✓` or `✗` beside the host and its result in the details until the host is edited or removed or the inventory is refreshed. `Esc` cancels a running test. An unknown host key opens a prompt showing its fingerprint: `y` trusts it and tests again, `n` or `Esc` declines. If you have moved on by the time the result arrives (into the filter or a form, say), no prompt interrupts you: the host is marked untrusted, and `t` tests it again to review the key. Editing a form cancels its running test.

The containers panel sits under the hosts and follows the selected host. It lists only when it gains focus for a host not yet listed, or on `r`, and keeps the result for the session. Its details show the selected container. Keys in the panel:

| Key | Action |
|---|---|
| `↑` / `↓` / `j` / `k` | Move through containers |
| `Enter` | Open a shell in this terminal |
| `n` | Open a shell in a new terminal |
| `u` | Set the user for the next shell (empty means the image's user; not saved) |
| `r` | List again |
| `L` | List this machine instead of the selected host |
| `g` / `s` | Add yourself to the docker group / start docker, shown only when the error offers it |
| `Esc` | Cancel a listing, otherwise back to the hosts |
| `Tab` / `Shift+Tab` | Back to the hosts |
| `q` | Quit |

External hosts can be inspected, tested and connected to, but Portico will not edit or remove them until you adopt them with `m` or `portico adopt`.
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

V1 deliberately does not include Podman, container logs or lifecycle, tunnels, port-conflict management, SSH-agent or password-manager discovery, key generation, Quickshell, a daemon, or a machine-readable API. Those features remain deferred until their workflows and integration contracts are clear.
