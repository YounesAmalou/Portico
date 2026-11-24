# Podman Container Shells Design

**Status:** Approved; implemented

## Purpose

Portico lists the running containers of this machine or of a host and opens a shell in one, with Docker or, on a Proxmox VE node, with `pct`. It never runs `podman`, so the containers of a Fedora, RHEL or other Podman host cannot be reached. This slice adds Podman as a third adapter on the backend interface (`docs/architecture.md`, "Add a backend"), behind the same picker, listing and shell.

It implements v4 slice 1 of `ROADMAP.md`, "Podman". Scope stops at listing running containers and opening a shell, as for Docker.

With three backends, the rule "the first installed backend wins" (Docker, then Proxmox) hides a second backend on the same target. This slice replaces it: every backend is listed, each in its own list.

## Settled questions

These were decided by the maintainer before the slice started.

- **Rootless Podman lists the SSH user's own containers.** Portico runs `podman ps` as the user it connects as: that user's rootless containers, or root's when it connects as root. It never runs `sudo podman`. To see root's containers, connect as root. An empty listing by a user other than root says so (below).
- **Every backend is listed, each in its own list.** No backend waits for another to be missing.
  - **TUI:** one top tab per backend: `1 Hosts │ 2 Tunnels │ 3 Docker │ 4 Podman │ 5 Proxmox`. Tab, Shift+Tab and `1`–`5` switch. The three container tabs share one target; showing a target lists it with all three backends at once, in parallel. A backend whose tool is missing shows its `not-installed` issue in its tab like any other issue: tabs are never hidden here (turning a backend off is the Settings slice's job).
  - **CLI:** `portico containers` without `--backend` prints one section per installed backend; `portico shell` without `--backend` searches every installed backend's listing and its picker offers all of them.
  - **JSON:** `portico containers --json` without `--backend` keeps today's single-backend document: the first installed backend, in the order Docker, Podman, Proxmox (`backend::AUTOMATIC`). Docker's and Proxmox's documents stay byte-identical. `--backend` picks another.
- **Order:** Docker, Podman, Proxmox. It is the order of the TUI tabs, of the CLI's sections, and of the JSON's choice.

## Decisions

### The adapter

`src/backend/podman.rs` implements `Backend` for `BackendId::Podman` (spelled `podman` in JSON and on the command line).

**Listing.** The adapter runs one POSIX script on the target, through the app's transport (`sh -c <script>`, locally or as ssh's remote command):

```sh
echo "portico-user=$(id -un 2>/dev/null)"
exec podman ps --format json
```

The marker names the user whose containers these are, for the empty-listing note and the advice below. Without `podman`, `exec` fails with the shell's `not found` and status 127.

**Parsing.** `podman ps --format json` prints one JSON array of objects. The adapter reads it with `serde_json`, starting at the first line that starts with `[` (lines before it, such as the marker or a login banner, are not part of it). Output with no array, or an array that cannot be parsed, is an `other` issue: `podman printed a listing Portico could not read on <target>.` Each object becomes a `Container`:

| Field | From |
|---|---|
| `id` | `Id` |
| `name` | `Names[0]` |
| `image` | `Image` |
| `command` | `Command`, its words joined by spaces |
| `running_for` | empty (Podman reports a timestamp, and `status` already says how long) |
| `status` | `Status` (`Up 2 hours`), or `State` when `Status` is empty |
| `ports` | `Ports`, each as `host_ip:host_port->container_port/protocol` (`0.0.0.0` when the IP is empty), joined by `, ` |
| `networks` | `Networks`, joined by `,` |
| `mounts` | `Mounts`, joined by `,` |
| `project`, `service`, `number`, `working_dir` | The Compose labels Docker uses (`com.docker.compose.project`, `.service`, `.container-number`, `.project.working_dir`), which both `podman-compose` and `docker compose` on Podman's socket set |

An object without an `Id` string counts in `skipped`. A pod's infra container (`IsInfra: true`) is left out: it only holds the pod's namespaces. Pods are not otherwise shown; containers of a Compose project group by project as with Docker.

**Empty listing.** When the listing is empty and the marker's user is not `root`, the list's `note` reads `Podman lists <user>'s own containers; connect as root to list root's.` A listing with containers has no note, and Podman's `via` is always `None`.

**Failures.** From the script's exit status and stderr:

| What happened | `Problem` | Headline | Next steps |
|---|---|---|---|
| Status 127 and `not found` | `not-installed` | `Podman is not installed on <target>, or it is not on the PATH of a non-interactive shell there.` | `Install Podman from the distribution's packages, then list again.` |
| stderr names `subuid`, `subgid`, `newuidmap` or `newgidmap` | `other` | Podman's last line on stderr | `Give <user> subordinate IDs on <target>: add a range for them to /etc/subuid and /etc/subgid, then run podman system migrate.` |
| stderr contains `permission denied` | `permission-denied` | Podman's last line on stderr | `Check that <user> can run podman on <target>: podman info says why it cannot.` |
| anything else | `other` | `podman failed on <target>: <its last line on stderr>` | none |

`<user>` is the marker's user, or `your user` without it. Podman has no daemon to be down, so `daemon-down` never occurs. There is no `Offer`: every fix above edits root-owned files or needs the user's own session, and Portico does not write on the target. (Had there been one, its key would have had to avoid the panel's keys listed in `Offer::key`'s documentation.)

**Shell.** `podman exec -it [-u <user>] <id> sh -c '<the shared shell script>'`, as Docker's: bash when the container has it, sh otherwise. Podman's shells can run as a chosen user, so `-u` and the TUI's `u` work as for Docker (`chooses_user` is true). The container's ungrouped heading is `Other`, as Docker's.

### The app

- `ContainerPrograms` gains `podman`, mapped like the others for local commands and tests.
- `App::containers(target, backend, …)` is unchanged: with `Some(backend)` it lists with it; with `None` it returns the first installed backend of `AUTOMATIC`, now `[Docker, Podman, Proxmox]`. The JSON output and the desktop panel use it.
- The CLI and the TUI list every backend themselves, each with `Some(backend)`.
- A shell in a new terminal re-runs `portico shell --hold … --backend podman -- <id>` for a Podman container, as for Proxmox.

### CLI

- `--backend docker|podman|proxmox` on `containers` and `shell`.
- **`portico containers` without `--backend`, without `--json`:** lists with each backend in order. A backend that is not installed is skipped silently. A backend that fails for another reason gets today's handling: its offer is proposed when the session is interactive, then its issue is printed on stderr, and the listing goes on with the next backend. An ssh failure stops the command, since every backend reaches the target the same way.
  - When exactly one backend lists, the output is exactly today's.
  - When several list, each section starts with the backend's name on a line of its own (`Docker`, `Podman`, `Proxmox`), then that listing as today, sections separated by a blank line.
  - When no backend is installed, Docker's `not-installed` issue is printed, as today.
  - The exit status is 1 when any installed backend failed, 0 otherwise.
- **`portico shell [<container>]` without `--backend`:** lists every installed backend the same way (a failing backend's issue is printed and the others are still searched) and then:
  - with a reference, resolves it in each listing. One match in all of them is opened. Matches in several backends are ambiguous: `error: web matches several containers on dev: web (docker), web (podman); choose one with --backend`. No match is today's `no container matches` error.
  - without one, the picker offers the containers of every listing. With more than one backend, each row's group label reads `<Backend> · <project or heading>`, so `Docker · shop` and `Podman · Other`.
  - when no backend listed, the command fails with the issue of the first installed one, or Docker's when none is installed.
- Each section's skipped-lines note names its backend's program, as today.
- Listing several backends on a host costs one ssh round trip per backend, one after the other: the CLI lets ssh prompt, and prompts cannot run in parallel.

### TUI

- `Tab::Containers` becomes `Tab::Containers(BackendId)`, and `Tab::ALL` lists the three backends in order after Hosts and Tunnels. The tab bar reads `1 Hosts │ 2 Tunnels │ 3 Docker │ 4 Podman │ 5 Proxmox`, and the hosts footer `Tab/1-5 tabs`.
- Each container tab has its own panel (its backend, list state, selection, filter, user, scroll) and its own listing worker, so the three listings of a target run in parallel and a slow one never delays another.
- The target is shared: `c` on the Hosts tab, `h` and `L` set the target of the three panels and list it in each where it is not listed or kept. `r` and `F5` list again in the current tab only; Esc cancels only the current tab's running listing.
- `c` shows the container tab last shown (Docker the first time), for the selected host when on the Hosts tab.
- The panel's title is its backend's name instead of `Containers` (`Podman /<filter>`). Rows, details, the user line and failures read as today; the Proxmox tab keeps its root-only user line.
- A tab shows only its own backend's listing; it never falls back to another.

### JSON

- A Podman document is a containers document with `"backend": "podman"`, like Proxmox's: `image`, `project`, `service` and the rest as listed; `via` is `null`; `note` is the empty-listing note or `null`.
- A Podman failure is an error document with `kind` `podman` and the problems above, never with an offer.
- New golden files `containers-podman.json` (a listing with a Compose project and a lone container) and `containers-podman-missing-subuid.json` (the subordinate-ID failure), documented in `docs/json-output.md`. The desktop panel's QML tests read them as they read every golden file; its code needs no change (it opens shells with `portico shell <id> --on <alias> --new-terminal`, which now searches every backend, and Podman IDs are full hex IDs).

## Testing

- **Adapter** (`src/backend/podman.rs`): parsing a recorded `podman ps --format json` (Compose labels, ports, infra container left out, objects without `Id` skipped, text before the array ignored, unreadable output); the empty-listing note for root and for another user; each failure row; the shell words with and without a user.
- **App** (`src/app/containers.rs`), with fake `ssh`, `docker`, `podman`, `sh` and `pct` programs: the automatic order now including Podman, `--backend podman`, the shell over ssh, and the new-terminal arguments.
- **CLI** (`src/cli/mod.rs`, `tests/containers.rs`): sections for several backends; one backend's output unchanged; a failing backend reported while another lists, with exit status 1; `shell` resolving across backends, ambiguity across backends, and the picker's labels.
- **JSON** (`tests/json.rs`): the two new golden documents; Docker's and Proxmox's unchanged.
- **TUI** (`src/tui/mod.rs`, `src/tui/containers.rs`): the five tabs and their keys, a shared target listed in all three panels, `r` in one tab only, the per-backend title.
- **Docs guard**: `cargo test --test docs` checks the golden blocks in `docs/json-output.md`.

The end-to-end suite has no Podman and gains no test; the fake programs cover the commands Portico runs.

## Not in this slice

- Root's containers next to a rootless user's (`sudo podman ps`).
- Pods as a grouping of their own.
- Recognising `podman-docker`'s `docker` wrapper: on such a host the Docker tab lists through the wrapper, as far as it answers Docker's format.
- A multi-backend JSON document.
