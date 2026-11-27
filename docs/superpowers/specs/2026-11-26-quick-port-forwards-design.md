# Quick Port Forwards Design

**Status:** Approved; implemented

## Purpose

Saved port forwards work, but setting one up means knowing the target's address and filling in the tunnel form first. Most of the time the user is already looking at the thing they want to reach: a host in the hosts tab, or a container or Proxmox CT in a container tab. This slice lets them start there: press one key, pick the port (Portico suggests the ones the target listens on), and get it on `127.0.0.1` at once. The forward is temporary until the user saves it.

It implements v6 slice 2 of `ROADMAP.md`, "Quick port forwards". The next slice, "Forwards to containers and CTs", builds on it (see "Left to the next slice").

## Goals

- A "forward a port" action (`f`) on every row of the hosts tab and of each container tab (Docker, Podman, Proxmox), and a matching CLI command, `portico forward`.
- It asks only what it cannot work out: the remote port, suggested from what the target reports, and the local port, prefilled.
- The forward starts at once, bound to `127.0.0.1`, and appears in the tunnels tab marked as temporary, with its local address ready to copy or open in a browser.
- A save action turns a temporary forward into a saved tunnel that stays listed.
- The tunnels tab lists temporary and saved forwards in one list, each showing what it reaches by name (host, container or CT), not only by address.

## Non-goals

- Re-resolving a container's address each time a forward starts, saving a forward by container name, and reporting that nothing answers inside a container: the next slice.
- Remote (`-R`) and dynamic (`-D`) quick forwards: a quick forward is always local (`-L`). The tunnel form keeps every kind.
- Binding anywhere but `127.0.0.1`. Saving and then editing the tunnel can change it.
- UDP.

## Anti-goals

- A temporary forward that writes anything persistent: nothing goes to `~/.ssh/portico/tunnels` until the user saves.
- A suggestion list that hides the free entry: the user can always type any port.
- A quick forward that silently binds somewhere else than the address it shows.
- A new daemon, database or dependency.

## Rules this design keeps

No daemon: a temporary forward is an ordinary `ssh` process, started and found again exactly like a saved tunnel's. No database: it lives only as its runtime record (`<runtime dir>/<name>.run` and `<name>.log`), the same small runtime files saved tunnels already use. OpenSSH stays authoritative: every start goes through the same `ssh -G` check as a saved tunnel's. No credential is stored.

## Settled questions

Decided by the maintainer before the spec was written.

- **How a temporary forward is recorded:** its runtime record only. Stopping it, or a reboot, leaves nothing behind; saving copies it into the tunnels file.
- **What a container row's forward reaches:** the container's address, looked up once when the forward starts (a published port goes through the host's published port instead, see below). The next slice adds the lookup on every start.
- **Containers of this machine** (the containers tab's "this machine" target): no forward, since there is no ssh host to go through. The action shows the address the port is already reachable at, ready to copy or open.
- **The CLI command:** `portico forward`, mirroring `portico shell`.
- **Saving a forward to a container address:** saves it as an ordinary tunnel to the current address, and the save message says it will not follow a recreated container. The next slice changes this to save by name.
- **The next slice's branch:** cut from `main` after this slice merges.

## Approaches

1. **Generalise the runtime record and the start (chosen).** A temporary forward is a tunnel that has a runtime record but no entry in the tunnels file. `start_tunnel` splits into "find the saved tunnel" and a `launch` of any `Tunnel`, so a quick forward gets every check a saved tunnel gets (port free, collisions, `ssh -G`, readiness, the Windows detach) for free; listing adds the temporary records to the saved tunnels.
2. **A separate quick-forward module with its own runtime directory.** Keeps the saved-tunnel code untouched, but duplicates start, stop, status and rediscovery, and the two lists would drift. Rejected.
3. **Temporary entries in the tunnels file with a flag.** One source for listing, but a crash or reboot leaves stale entries to clean up and the hand-editable file churns. Rejected by the maintainer.

## Design

### Temporary forwards in the runtime directory

- **Name.** A temporary forward's name is `_` followed by a slug of what it reaches and the remote port: `_prod-5432` for a host, `_web-80` for a container shown as `web`, `_101-22` for CT 101. Characters outside `[A-Za-z0-9._-]` become `-`. A saved tunnel's name must start with a letter or digit, so the leading `_` can never clash with one, and whether a name is temporary is known from the name alone, even when a record has to be rebuilt from the process table. See "Same target again" below for a name already in use.
- **Record.** The running record gains one optional line, `reaches <label>`: what the forward reaches by name, for showing it (`prod`, `web on prod`, `CT 101 on pve`). A record rebuilt from the process table has none and shows the destination address instead. Saved tunnels' records do not write it.
- **Lifetime.** A temporary forward that started stays listed while its record exists. One whose ssh has exited (the network dropped, the host rebooted) is listed as failed, like a saved tunnel, and can be started again from its record (host and forward are in it). Stopping it removes its record and log, so it disappears. A first start that fails writes nothing: the failure is reported where the user asked for the forward, not left in the list. A restart of a listed temporary forward that fails keeps it listed as failed and restartable: a temporary forward's failed record keeps the `host`, `forward` and `reaches` lines (optional in a failed record; saved tunnels' failed records do not write them).
- **Same target again** (builder's call): forwarding the same target and remote port again reuses the name. A failed temporary forward of that name is replaced (it is the same forward); a running one gets the next free suffix, `-2`, `-3`, and so on.
- **Collisions.** A temporary forward takes part in the port checks like a running saved tunnel: starting a saved tunnel on its port is refused with "port 8080 on 127.0.0.1 is in use by tunnel _web-80", and the reverse.

### Choosing the ports

**The remote port** is suggested from what the target reports, most likely first; the user can always type another.

- **A host:** Portico runs `ss -Hltn` on it through ssh (falling back to `netstat -ltn`), and suggests its listening TCP ports, ignoring 22 and duplicates across addresses. A port that listens only on loopback is still suggested: that is what forwards are for. When neither tool answers, there are no suggestions and the field is free entry, with a line saying why.
- **A Docker or Podman container:** its published ports (from the listing's ports column, `0.0.0.0:8080->80/tcp`) and its exposed ports (from `inspect`). A published port is reached through the host's published port (`-L <local>:localhost:<published> <host>`), which also works for rootless containers. Any other port is reached at the container's address on its network (`-L <local>:<container IP>:<port> <host>`), from `inspect`. A container on the host's network is reached at `localhost`. A container with no address the host can reach (rootless Podman without a published port) gets no forward: the dialog says to publish the port, which the next slice revisits.
- **A Proxmox CT:** its listening TCP ports, from `ss -Hltn` run inside it with `pct exec` (as the listing runs `pct`, as root or through `sudo -n`), and its address, from the CT's network interface (`pct exec <id> -- ip -4 -o addr show scope global`, first address). A port listening only on the CT's loopback is listed but marked "loopback only", since it cannot be reached from the host (the next slice says so when the forward starts too).

Each backend adapter gains one operation, beside listing and the shell command: what a container can be reached at (its addresses and its ports, each port published or not). The app runs its commands with the same transport as the listing. The exact trait shape is the builder's call.

**The local port** defaults to the remote port when `127.0.0.1` can bind it, otherwise the next free port above it. A privileged remote port (below 1024) where this machine needs privileges to bind it starts from the remote port plus 8000 instead (80 becomes 8080, 443 becomes 8443). The user can change it before starting.

### The TUI

- **`f` on a row** of the hosts tab or a container tab opens a small dialog over the tab: what it reaches (`web on prod`), the remote port with the suggestions in a dropdown (the same dropdown as the tunnel form's host field), and the local port, prefilled. While the suggestions load, the dialog says so and the remote port can already be typed. Enter starts the forward in the tunnels tab's start worker (the interface stays responsive, Esc cancels a start still connecting); on success the TUI switches to the tunnels tab with the new row selected. A failure stays in the dialog with its explanation, as a saved tunnel's failure does.
- **On "this machine"** the dialog shows where each suggested port is already reachable (`127.0.0.1:8080` for a published port, the container's address on Linux) instead of a local port, with `y` to copy and `b` to open it. On macOS and Windows, where a container's address is inside Docker Desktop's VM and not reachable, only published ports are shown.
- **The tunnels tab** lists saved and temporary forwards together, temporary ones after the saved ones, newest first. Each row shows the name (a temporary one marked `temporary`), what it reaches by name, the local address and the status. New keys: `w` saves the selected temporary forward (it asks for a name, prefilled with the name without its `_`), `y` copies the selected row's local address, `b` opens `http://<local address>` in the browser. `e` (edit) does not apply to a temporary forward. `x` on a temporary forward stops and removes it after a confirmation; `s` starts a failed one again.
- **Copying** uses the terminal's OSC 52 escape sequence, so it works over ssh and in every major terminal without a clipboard tool; the status line says "copied 127.0.0.1:8080". **Opening** runs the system's opener from the platform layer: `xdg-open` on Linux, `open` on macOS, `start` (through `cmd /c`) on Windows.

### Saving

`w` in the tunnels tab, or `portico tunnel save <temporary name> [--name <name>]`, writes the forward into the tunnels file under the new name, exactly as the tunnel form would (same validation, same `ssh -G` check). The default name is the temporary name without its leading `_`; an existing name is refused, and the TUI asks for another. Saving a forward that reaches a container address adds a note: "saved with the container's current address 172.18.0.4; it will not follow the container if it is recreated".

**A running temporary forward is restarted under its saved name** (decided by the maintainer): Portico finds a tunnel's process by the log path in its command line, which cannot change while ssh runs. Saving writes the file entry, stops the temporary forward (its record and log go), and starts the saved tunnel at once. The port is down for about a second and open connections through it drop; the save message says so. If that start fails, the tunnel stays saved and shows the failure, as any saved tunnel's failed start does. Saving a failed temporary forward saves it without starting it.

**What a saved tunnel reaches** (builder's call, recorded here): the tunnels file does not change in this slice, so a saved tunnel shows its host alias as what it reaches, and JSON `reaches` is the host alias for a saved tunnel. Saving a container forward therefore loses its container label until the next slice adds targets named by container.

### The CLI

```
portico forward [CONTAINER] [--on [ALIAS]] [--backend docker|podman|proxmox] [--port REMOTE] [--local LOCAL] [--json]
```

- `--on ALIAS` without `CONTAINER` forwards a port of the host. With `CONTAINER`, a port of that container on the host, found as `portico shell` finds it (name, `project/service`, service, CT ID; every installed backend unless `--backend`). Without `--on`, `CONTAINER` is on this machine, and the command prints where the port is reachable instead of forwarding. `--on` without a value, and no arguments at all, open the inline host picker, as `portico shell` does.
- Without `--port`, an interactive terminal gets an inline picker of the suggested ports with free entry; a non-interactive run fails and lists the suggestions. `--local` overrides the default local port.
- On success it prints the local address and how to stop or keep it:
  ```
  Forwarding 127.0.0.1:8080 → web:80 on prod (temporary forward _web-80).
  Stop it with `portico tunnel stop _web-80`, keep it with `portico tunnel save _web-80`.
  ```
- `portico tunnel save <name> [--name NAME]` saves a temporary forward. `portico tunnel stop`, `start` and `log` accept temporary names. `portico tunnels` lists temporary forwards after the saved ones, marked `temporary`.

### JSON

- `portico tunnels --json`: every tunnel object gains `temporary` (`true` for a temporary forward) and `reaches` (the label; the host alias for a saved tunnel; the destination address for a temporary forward whose record was rebuilt without one). Temporary forwards follow the saved tunnels. Both keys are additive; the desktop panel ignores them until it is updated, and can already stop a temporary forward by its name.
- `portico forward --json` prints the started forward as one tunnel object of the same shape, or the error document. On this machine it prints the reachable addresses instead. `docs/json-output.md` documents both with golden files.

### Errors

Every failure names what to do: the host does not answer (the connection test's explanations, as for a saved tunnel), the local port is in use (and by which tunnel), the container has no reachable address (publish the port), the CT's address could not be read, no port is listening and none was typed.

## Testing

- Unit tests for the name slug and its uniqueness, the `reaches` record line (written, parsed, absent when rebuilt), the local-port default (same, next free, +8000 for privileged ports), and parsing `ss`, `netstat`, `docker inspect`, `podman inspect` and the CT's `ip` output.
- App tests with fake `ssh` and backend scripts, as the existing tunnel and container tests use: a quick forward to a host, to a published port, to a container address, to a CT; its listing, collisions with saved tunnels, stop, restart after failure and save.
- TUI tests for `f` on each tab, the dialog, the tunnels tab's new keys and the jump to the tunnels tab.
- End-to-end tests against the throwaway `sshd`: `portico forward --on <host> --port <port>` reaches a port listening on it, `portico tunnels` lists it as temporary, `portico tunnel save` keeps it, and `stop` removes it. On Linux, macOS and Windows.

## Documentation

The README's tunnels section, `docs/json-output.md`, `docs/architecture.md` (the new backend operation and the opener in `platform::launcher`), and `ROADMAP.md` (the slice leaves it). This spec's status becomes "Approved; implemented".

## Left to the next slice

"Forwards to containers and CTs": a forward named by its container (Compose project and service, container name or CT ID) whose address is looked up every time it starts, saved by name; reporting that nothing answers when an app listens only on the container's loopback; and containers on networks the host cannot route to.

## Left to the builder

The exact shape of the backend operation and its result type, the dialog's layout, the inline picker's reuse, the order of suggestions within a source, and where the slug and local-port helpers live, within the layer rules of `docs/architecture.md`.
