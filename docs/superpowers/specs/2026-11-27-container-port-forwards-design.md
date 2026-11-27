# Container Port Forwards Design

**Status:** Approved

## Purpose

A port forward to a container is only as good as the address it was saved with. Since the "Quick port forwards" slice, saving a forward to a container writes the container's current address into the tunnels file, and the save says it will not follow the container: a Coolify redeploy, a `docker compose up` that recreates the service, or a CT that gets a new DHCP lease, and the saved forward points at nothing.

This slice lets a forward name its container the way the pickers name it (a Compose `project/service`, a container name, a Proxmox CT ID) and look its address up through the backend every time it starts. It implements v6 slice 2 of `ROADMAP.md`, "Forwards to containers and CTs", the last of v6.

## Goals

- A saved tunnel can have a container target: a backend and a reference, resolved at every start (and every restart of a temporary forward) to the container's current address, so the forward survives the container being recreated.
- The far port can be any port inside the container, including one the host itself already uses.
- Saving a quick forward to a container saves it by name.
- Container targets can be set and changed from the CLI (`portico tunnel add` and `edit`) and from the TUI's tunnel form, and by hand in the tunnels file.
- A forward that cannot reach its container says why, and one whose port answers nothing inside the container says so instead of failing silently.

## Non-goals

- Reaching an app that listens only on `127.0.0.1` inside its container (decided in `ROADMAP.md`: revisited only if a clean way installs nothing in the container and needs no extra privileges).
- Reaching containers on networks the ssh host cannot route to (decided by the maintainer, see below).
- Remote (`-R`) and dynamic (`-D`) forwards to containers: a container target is always a local forward.
- Containers of this machine (no ssh host): as in quick forwards, they are already reachable directly.

## Anti-goals

- Anything installed in or started next to the container (no `nc`, `socat` or helper container).
- A forward that starts and silently reaches the wrong container, such as a container of another host or backend with the same name.
- A tunnels file that OpenSSH-style hand editing can no longer follow.

## Rules this design keeps

No daemon: the lookup runs when a start runs, nothing watches containers. No database: the container target is one line of the tunnels file (or of a temporary forward's runtime record). OpenSSH stays authoritative: the resolved forward goes through the same `ssh -G` check as any tunnel. Nothing is installed in a container and no extra privilege is used: the lookup is the listing and the `reach` operation the backends already run.

## Settled questions

Decided by the maintainer before the spec was written.

- **Containers on networks the host cannot route to** (rootless Podman without a published port, Docker Desktop's VM, a macvlan network the host itself cannot reach): the start is refused with the reason and the fix (publish the port, or use a network the host routes). No relay.
- **Where a container target is set:** the CLI (`tunnel add` and `edit`) and the TUI tunnel form, besides saving a quick forward.
- **A Compose service with several replicas:** the forward reaches the lowest-numbered running replica; `project/service#n` pins one.

## Approaches

1. **An optional container target on `Tunnel`, resolved into a concrete forward before the existing start checks (chosen).** Every check a start makes today (`ssh -G`, collisions, the bind probe, readiness) runs on the resolved forward, so nothing is duplicated.
2. **A separate container-tunnel type.** Duplicates start, stop, status and listing. Rejected.
3. **Resolve at save time only.** What the previous slice does; it does not survive a redeploy. Rejected.

## Design

### The model

`Tunnel` gains `container: Option<ContainerTarget>`, with `ContainerTarget { backend: BackendId, reference: String, port: u16 }`: the backend that lists the container, the reference as the pickers accept it (`resolve` in `backend::listing`), and the port inside the container. A tunnel with a container target is a local forward; its forward's destination is not stored, it is resolved at each start. A tunnel without one is unchanged.

### The tunnels file

A container tunnel is written with one directive in place of `LocalForward`:

```
Tunnel api
    Host prod
    ContainerForward 127.0.0.1:8080 docker shop/api 80
```

The value is the listen address and port (as `LocalForward` writes them), the backend (`docker`, `podman` or `proxmox`), the reference and the port inside the container. The reference may contain `/` and `#` but no whitespace. A file with an unknown backend or a malformed line is refused on that line, as other mistakes in the file are. The file is not read by OpenSSH, so the new directive is safe. Files without it read and write exactly as before.

### Resolving at start

A start of a container tunnel (saved, or the restart of a temporary forward):

1. Lists the containers of the tunnel's host with its backend (batch mode, cancellable, as the TUI's listing runs).
2. Resolves the reference with `backend::resolve`, except that a reference naming a Compose service with several running replicas takes the lowest-numbered one instead of being ambiguous. A reference that matches nothing is refused: "web is not running on prod (docker)".
3. Runs the backend's `reach` for that container and builds the forward with the same destination rule as quick forwards: a published port through the host (at its specific address when published on one), the host network at `localhost`, otherwise the container's address with the port inside the container.
4. Hands the resolved tunnel to the existing start checks and launch.

Refusals, each naming the fix: the container is not running; it has no address the host can route to ("publish the port, or attach it to a network prod routes"); a CT's address could not be read. A listing that fails (ssh refused, Docker down) is the listing's own explained failure.

The resolved address is not written back to the tunnels file. The running record keeps the resolved `-L` (as every record does) plus a `container <backend> <reference> <port>` line, so the tunnels tab shows what it reaches and a restart resolves again.

### Nothing answers

After a container forward comes up, Portico makes one test connection to its local port and waits briefly for ssh's log to report the channel. If the log says the far side refused it (`open failed: connect failed`), the start succeeds with a warning: "started, but nothing answers on port 80 inside web; an app that listens only on 127.0.0.1 inside its container cannot be reached". The tunnels list shows the same warning for a running container forward whose log has that line. The test connection is closed at once and is not reused.

### Quick forwards

A quick forward to a container (from the previous slice) records its container target, so restarting it resolves again, and `w` or `portico tunnel save` saves it as a container tunnel. The previous slice's "saved with the container's current address" note goes away. A quick forward to a host is unchanged.

### Editing

- **CLI:** `portico tunnel add <name> --on <alias> --container <ref> [--backend docker|podman|proxmox] --port <port inside> [--local <port>]`. Without `--backend`, Portico lists the host with every installed backend, as `portico shell` does, and records the backend that has the container; a container that is not running then needs `--backend`. `--local` defaults as in quick forwards. `portico tunnel edit` accepts `--container`, `--backend` and `--port`, and `--address` turns a container tunnel back into an address one. `-L` and `--container` are mutually exclusive.
- **TUI tunnel form:** a "Container" field after "Host" (with the backend chosen by Space or ←→ next to it, as Kind is). When it is filled, Kind is local and fixed, "Target host" is hidden, and "Target port" is the port inside the container.
- `portico tunnel show` and the tunnels tab show the target as `shop/api:80 (docker)`.

### JSON

Tunnel objects gain `container`: `{"backend": "docker", "reference": "shop/api", "port": 80}`, or `null`. For a container tunnel the forward's `target_host` is the reference (still a string, so no consumer breaks; decided by the maintainer), `target_port` is the port inside the container, and `description` reads `127.0.0.1:8080 → shop/api:80`. `reaches` for a saved container tunnel is `<reference> on <host>`. The changes are additive; `docs/json-output.md` and the goldens are updated.

### Documentation

The README's tunnels section documents container tunnels, the replica rule and the two limits (loopback-only apps, unroutable networks), and the "nothing answers" warning. `docs/architecture.md` notes the container target in `domain::tunnel`. `ROADMAP.md` loses its v6 section. This spec's status becomes "Approved; implemented".

## Testing

- Domain: `ContainerForward` parse and render round trip, refusal of malformed lines and unknown backends, the lowest-replica resolution.
- App, with fake ssh and backend scripts: a saved container tunnel starts against the current address; after the fake container "moves" (new address), the next start follows it; a stopped container, an unroutable one and a CT without an address are refused with their reasons; the "nothing answers" warning from a log line; a quick container forward restarts with a new address and saves by name.
- CLI and JSON: `tunnel add --container`, `edit`, `show`, the goldens.
- TUI: the form's Container field and backend chooser, and the tunnels tab's target text.
- End to end: the throwaway sshd has no containers, so the e2e suite gains a fake `docker` on the server side's `PATH` that lists one container whose address is `127.0.0.1` and whose port is the test's listener; a container tunnel reaches it, and after the fake's port changes, a restart follows.

## Calls made after the design review

Settled by the author, for the maintainer to check while reading.

- **The reference a save writes:** the one that survives a redeploy, `backend::remember_key`: `project/service` for a Compose container (whatever its replica, so the lowest-replica rule applies), its name otherwise, the VMID for a CT. A plain container whose name changes on every deploy cannot be followed; the README says to give it a Compose service or a fixed name.
- **The lowest-replica rule** applies only among containers of one project and service. A bare service name matching services of several projects stays ambiguous and is refused, listing them.
- **Where resolution happens:** only in a start. Everywhere else (listing, collisions between saved tunnels, status of a stopped tunnel, the `ssh -G` check of `add` and `edit`) a container tunnel is its listen side; the `ssh -G` check of `add` and `edit` uses `localhost:<port inside>` as a stand-in destination, which validates the host and the listen address.
- **The running record's `container` line** is written for temporary forwards only (they have no file entry); a saved tunnel's target is read from the file.
- **Several starts** (`tunnel start a b`, `--all`, the desktop apps): each tunnel resolves on its own, and a failed resolution fails that tunnel only.
- **The "nothing answers" check** runs on every start, opens one connection and closes it; ssh logs the refused channel (`channel N: open failed: connect failed`) at its default level, which every start's log already records. It is not configurable.
- **Editing:** `--container` is refused with `-R`, `-D` or `-L`; `--backend` and `--port` without a container target are refused; `--address <host>` turns a container tunnel back into an address tunnel (keeping its listen side and port). A running tunnel is refused, as edits are today.

## Left to the builder

The exact Rust shapes (`ContainerTarget` can live in `domain::tunnel` and name `backend::BackendId`, the plain enum `domain::settings` already imports; `domain` calls nothing in `backend`, so resolution lives in `app`; `docs/architecture.md` records the second sideways import), the wait for the "nothing answers" check (bounded, under a second), the form's layout, and the help texts.
