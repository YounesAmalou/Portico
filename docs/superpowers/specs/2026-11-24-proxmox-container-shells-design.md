# Proxmox Container Shells Design

**Status:** Approved; implemented

## Purpose

Portico lists the running Docker containers of this machine or of a host and opens a shell in one. A Proxmox VE node runs its workloads as LXC containers (CTs), managed with `pct`, and reaching one today means connecting to the node, running `pct list` and typing `pct exec <vmid> -- bash`. This slice gives CTs the same list-and-shell flow, from the CLI and from the TUI's containers tab.

It implements v4 slice 1 of `ROADMAP.md`, "Proxmox container shells". It is a second adapter on the backend interface (`docs/architecture.md`, "Add a backend"), behind the same picker, listing and shell, not a separate feature. Scope stops at listing running CTs and opening a shell: starting, stopping and any other CT management stay in the roadmap's "Not planned".

## Settled questions

These were decided by the maintainer before the slice started.

- **Root on the node.** `pct` needs root. Portico runs `pct` directly when the SSH user is root, and otherwise `sudo -n pct` (non-interactive: it never prompts). When neither works, the listing reports `permission-denied` with steps telling the user to connect as root or allow passwordless sudo for `pct`. There is no automatic fix (no `Offer`): Portico never writes a sudoers file on the node.
- **Running CTs only,** as `docker ps` lists only running containers. No start or stop.
- **The shell runs as root inside the CT.** `pct exec <vmid>` runs `bash` when the CT has it and `sh` otherwise, with the same script as Docker's shell. Portico does not switch to another user inside the CT; the user can run `su` themselves.

## Decisions

### The adapter

`src/backend/proxmox.rs` implements `Backend` for `BackendId::Proxmox` (spelled `proxmox` in JSON and on the command line).

**Listing.** The adapter runs one POSIX script on the target, through the app's transport (`sh -c <script>`, locally or as ssh's remote command):

1. It appends `/usr/sbin` and `/sbin` to `PATH`, because Debian leaves them out of a non-root user's `PATH` and `pct` lives in `/usr/sbin`.
2. Without `pct` it prints `portico: no pct` on stderr and exits 127.
3. As root (`id -u` is 0) it prints `via=none` on stdout and runs `pct list`.
4. Otherwise, without `sudo` it prints `portico: no sudo` and exits 1; with it, it prints `via=sudo` and the user's name (`user=<name>`) and runs `sudo -n pct list`.

One script keeps a listing to one ssh round trip, and the decision ("root, otherwise sudo") is made where the user's identity is known. The markers are Portico's own, so the adapter does not depend on the wording of `sh` or `sudo`.

**Parsing `pct list`.** Its output is a header line (`VMID Status Lock Name`) and one fixed-width row per CT. The adapter reads the columns at the offsets of the header's words, so an empty `Lock` cell is not mistaken for the name; without a header it falls back to whitespace-separated fields, taking the last as the name when there are four. Lines whose first column is not a number are ignored (login banners, the markers). Only rows whose status is `running` are kept. A row becomes a `Container` with:

| Field | Value |
|---|---|
| `id` | The VMID (`101`) |
| `name` | The CT's name (its hostname in Proxmox), or the VMID when the name is empty |
| `status` | `running` |
| `project`, `service`, `number`, `working_dir` | none |
| every other field | empty |

`skipped` counts rows that start with a number but have no status.

**Failures.** From the script's exit status, its markers and stderr:

| What happened | `Problem` | Headline and next steps |
|---|---|---|
| `portico: no pct` | `not-installed` | `pct is not available on <target>: it is not a Proxmox VE node.` |
| `portico: no sudo` | `permission-denied` | `pct needs root on <target>, and <user> is not root and sudo is not installed.` |
| `via=sudo` and the command failed with sudo's own message (`sudo: …` on stderr) | `permission-denied` | `pct needs root on <target>, and sudo would ask <user> for a password.` (or sudo's message when it is another refusal) |
| anything else | `other` | `pct failed on <target>: <its last line on stderr>` |

Both permission headlines take the same two next steps:

- `Connect to <alias> as root: portico edit <alias> --user root`
- `Or let <user> run pct without a password: add "<user> ALL=(root) NOPASSWD: /usr/sbin/pct" with visudo on <target>.`

On this machine (`Target::Local`) the first step reads `Run Portico as root on this machine.` When the user is unknown (an issue explained without the script's markers), `<user>` reads `your user`. `offer` is always `None`.

**Via.** A listing that needed sudo returns `via: sudo`, as Docker returns `via: sg` when it ran inside the docker group. The shell uses it: `sudo -n pct exec …`. Docker's `GroupTool` becomes `Via` with variants `Sg` and `Sudo`, since it is no longer only a group tool. A Proxmox listing has no `note`: unlike `sg`, sudo leaves nothing for other shells to catch up on.

**Shell.** `pct exec <vmid> -- sh -c '<the Docker shell script>'`, prefixed with `sudo -n` when `via` is `sudo`. `pct exec` attaches to the terminal ssh allocates, so the shell is interactive.

### The user option

`-u/--user` (CLI) and the `u` key (TUI) choose the user Docker's shell runs as. A Proxmox shell always runs as root, so:

- `portico shell … -u <user>` on a Proxmox container fails before opening the shell (after the listing that found it to be a Proxmox container): `error: a Proxmox container's shell runs as root; leave out --user and run su inside it`, exit status 1.
- In the TUI the user line reads `user: root (Proxmox)` for a Proxmox listing and `u` does nothing there; the footer leaves out `u user`. The typed user is kept for when the tab shows a Docker listing again.

The adapter's `shell_words` ignores `user`; the check lives in the app (`container_shell`), so the CLI and the TUI share it. Docker's behaviour is unchanged.

### Choosing the backend for a target

Portico chooses for each listing, without configuration:

1. It lists with **Docker** first, exactly as today.
2. Only when Docker's issue is `not-installed` (no `docker` on the target, locally or over ssh), it lists with **Proxmox**.
3. When Proxmox is not installed either, the listing fails with **Docker's** issue, so a target with neither reports exactly what it reports today.

On this machine, a listing whose first command cannot be started at all (no `docker`; no `sh` for Proxmox's script) counts as that backend not being installed, as a missing `docker` did before. For a shell, a missing backend tool (`docker`, `pct`) is reported the same way.

The order is one constant, `backend::AUTOMATIC: [BackendId; N]`, in `src/backend/mod.rs`: the app walks it and moves to the next backend only on `not-installed`. A third adapter (Podman, the next slice) is a third entry, and its slice decides where in the order it goes. A host that runs Docker never runs `pct`, so Docker hosts cost no extra round trip; a Proxmox node without Docker costs one extra (`docker ps` failing with 127).

A node that has both Docker and `pct` lists its Docker containers. `portico containers --backend proxmox` and `portico shell --backend proxmox` choose a backend explicitly and skip the others. The TUI has no switch: the settings slice (v4, "Settings and preferences") lets the user turn a backend off, which covers this case there. Listing both backends side by side on one target was not chosen, because one listing then needs per-container backends and partial failures (Docker down, Proxmox listed) in the TUI, the CLI and the JSON, and because Docker's JSON must not change (below).

`ContainerList` gains `backend: BackendId`, the backend that produced it. Everything downstream (shell, details, JSON) reads it from the list.

### CLI

- `portico containers [--on <alias>] [--backend docker|proxmox] [--json]`. The human listing of a Proxmox node prints its CTs under the heading `Proxmox containers`, one line each: VMID, name, status, columns aligned (Docker's listing is unchanged). With no running CT: `No running containers on <target>.`
- `portico shell [<container>] [--on <alias>] [--backend docker|proxmox] [-u <user>] [-n]`. A CT is referenced by its exact name or its exact VMID. `resolve` gains an exact-ID step, tried after the existing name and Compose forms and before the hex-prefix one; Docker IDs are 64 hex digits, so no reference that resolved before resolves differently. The picker shows the same rows as for Docker (display name, name, image, status), which for a CT are its name, VMID, nothing and `running`.
- A shell in a new terminal re-runs `portico shell --hold --on <alias> -- <id>` as today; it adds `--backend proxmox` for a Proxmox container and `--via sudo` when the listing needed sudo. Docker's arguments are unchanged. `--via` (hidden) now accepts `sg` and `sudo`, and `--hold` without `--backend` means Docker, as before.
- `--backend` with `--hold` names the backend of the given ID; without `--hold` it restricts the listing to that backend, whose issue is then reported even when it is `not-installed`.

### TUI

The containers tab is unchanged in shape: the target line, the list, the user line, the details. For a Proxmox listing:

- The group heading is `Proxmox containers` instead of `Other`, and the CTs are sorted by name, as Docker containers outside a project are.
- Each row shows the name and the status, as for Docker.
- The details show `name`, `id` (the VMID), `status`, and `user: root (Proxmox)`.
- Enter and `n` open the shell as for Docker, passing the list's backend and via.
- The note about skipped lines says `pct` instead of `docker` (`Skipped N lines pct printed that Portico could not read.`); Docker's wording is unchanged.

Failures appear as Docker's do: the headline and next steps, with no offer key.

### JSON

Docker's documents stay byte-identical: the golden files `containers.json`, `containers-on-host.json` and `containers-docker-down.json` do not change. A Proxmox listing is the same document with one more key, `"backend": "proxmox"`, at the top level. Docker's documents do not carry `backend`; this is a deliberate, documented exception to "every documented key is always present", so that a consumer written before Proxmox reads a Docker listing exactly as before. A consumer reads a document without `backend` as Docker's.

In a Proxmox document:

| Field | Value |
|---|---|
| `backend` | `proxmox` |
| `containers[].id` | The VMID, as a string (`"101"`): what `portico shell <id> --on <alias> --backend proxmox` opens |
| `containers[].display_name`, `.name` | The CT's name |
| `containers[].image` | `null` (CTs have no image) |
| `containers[].project`, `.service` | `null` |
| `containers[].status` | `running` |
| `via` | `sudo` when `pct` ran through `sudo -n`, otherwise `null` |
| `note` | `null` |
| `skipped` | Rows of `pct list` Portico could not read |

A Proxmox failure is an error document with `kind` `proxmox` and the same `problem` values as Docker's; it never has an offer, so no "Run portico containers in a terminal" step is added. Adding `proxmox` to `error.kind` and `sudo` to `via` is allowed within schema 1 (consumers treat an unknown kind as `other`). New golden files: `containers-proxmox.json` and `containers-proxmox-denied.json`, both printed by the test suite.

The desktop panel's code needs no change (its QML tests read the two new golden files, as they read every golden file): it opens a shell with `portico shell <id> --on <alias> --new-terminal`, which chooses the backend itself and resolves a VMID exactly.

## Testing

- **Adapter** (`src/backend/proxmox.rs`): parsing `pct list` with and without locks, stopped CTs left out, banners ignored, an unreadable row counted; every failure row of the table above; the shell words with and without sudo.
- **App** (`src/app/containers.rs`), with fake `ssh`, `docker` and `sh` programs as Docker's tests use: the automatic order (Docker listed and `pct` never run; Docker missing, Proxmox listed; both missing, Docker's issue), `--backend` restricting the listing, the shell words over ssh with and without sudo, `--user` refused for Proxmox, and the new-terminal arguments.
- **CLI** (`src/cli/mod.rs`, `src/cli/json.rs`): the human listing, `--backend` parsing, the user refusal, and the two new golden documents (the Docker ones unchanged).
- **TUI** (`src/tui/containers.rs`): the Proxmox heading, details and user line, `u` ignored, and the shell action carrying the backend.
- **Docs guard**: `cargo test --test docs` checks the golden blocks in `docs/json-output.md`.

The end-to-end suite has no Proxmox node and gains no test; the fake programs cover the commands Portico runs.

## Not in this slice

- Starting, stopping or creating CTs; Proxmox VMs (`qm`), which have no `exec` shell.
- Listing Docker and Proxmox side by side on one target.
- A preference for a target's backend (the settings slice).
