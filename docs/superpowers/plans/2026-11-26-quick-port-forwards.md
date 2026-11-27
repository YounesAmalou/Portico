# Quick Port Forwards Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Forward a port of a host, container or Proxmox CT to `127.0.0.1` in one step (`f` in the TUI, `portico forward` in the CLI), as a temporary tunnel that can be saved.

**Architecture:** A temporary forward is a tunnel with a runtime record and no entry in the tunnels file; its name starts with `_`. `App::start_tunnel` is split so any `Tunnel` can be launched with the existing checks. Each backend adapter gains a `reach` operation (addresses and ports of a container); hosts are asked with `ss`/`netstat` over ssh. The TUI gets a forward dialog and a reworked tunnels tab; the CLI gets `portico forward` and `portico tunnel save`.

**Tech Stack:** Rust (edition and MSRV as in `Cargo.toml`), ratatui/crossterm, clap, serde_json; no new dependency.

**Spec:** `docs/superpowers/specs/2026-11-26-quick-port-forwards-design.md`

## Global Constraints

- No new crate dependency. No daemon, no database: temporary forwards live only in the runtime directory (`<name>.run`, `<name>.log`).
- Layer rules of `docs/architecture.md`: `domain` never calls `libc`/`std::os::unix`, `app`, `cli` or `tui`; `backend` never calls `app` or an interface.
- A quick forward is always `-L`, bound to `127.0.0.1`.
- Temporary names: `_` + slug of what it reaches + `-` + remote port; characters outside `[A-Za-z0-9._-]` become `-`.
- Local port default: the remote port if `127.0.0.1` binds it, else the next free above it; when binding the remote port is `Bind::Denied`, start at remote + 8000.
- `reaches` labels: a host `prod`; a container `web on prod`; a CT `CT 101 on pve`; a saved tunnel its host alias; a rebuilt record without the line its destination `host:port`.
- Do not touch the desktop-integration area of the TUI settings overlay (`src/tui/settings.rs` desktop rows, `src/desktop*`): another session changes it.
- Commits: Conventional Commits, no AI attribution lines. Run `scripts/ci-checks.sh` as a regular user before the final commit.
- Copy (exact): CLI success
  `Forwarding 127.0.0.1:<local> → <reaches-short>:<port> (temporary forward <name>).` then
  ``Stop it with `portico tunnel stop <name>`, keep it with `portico tunnel save <name>`.``
  Save note for a container address: `saved with the container's current address <ip>; it will not follow the container if it is recreated`.

## Review Focus

1. Suggestions fail to load (ssh refused, no `ss`/`netstat`, `pct exec` denied): the dialog and the CLI still accept a typed port, with a one-line reason. Test in Task 7 and Task 5.
2. The default local port is taken between the suggestion and the start: the start is refused with `PortInUse` and the dialog keeps the values. Test in Task 4.
3. `ss` lines with IPv6 and wildcard listeners (`[::]:80`, `*:80`, `[::1]:5432`, `0.0.0.0:22`), and `netstat` lines `tcp6 0 0 :::80 :::* LISTEN`: ports parsed, duplicates merged, 22 dropped. Test in Task 2.
4. A container on several networks: the address chosen is the first by network name, so it is deterministic. Test in Task 3.
5. A container name with characters outside the slug set (`shop_api.1`, `wéb`, `a/b`) gives a valid, stable temporary name. Test in Task 1.

---

### Task 1: Temporary names, records and the local-port default (domain)

**Files:**
- Modify: `src/domain/tunnel/mod.rs` (helpers + tests)
- Modify: `src/domain/tunnel/runtime.rs` (record lines + tests)

**Interfaces:**
- Produces:
  - `pub fn temporary_name(label: &str, port: u16) -> String` — `_<slug(label)>-<port>`; label is the short name (`prod`, `web`, `101`).
  - `pub fn is_temporary(name: &str) -> bool` — starts with `_`.
  - `pub fn unique_name(base: &str, taken: impl Fn(&str) -> bool) -> String` — `base`, else `base-2`, `base-3`, …
  - `pub fn saved_name_for(temporary: &str) -> String` — strips the leading `_`.
  - `pub fn default_local_port(remote: u16, probe: impl Fn(u16) -> Bind) -> Option<u16>` (in `runtime.rs`, `Bind` from there).
  - `RunningRecord` gains `pub reaches: Option<String>`; `Record::Failed` gains `host: Option<String>, forward: Option<String>, reaches: Option<String>` (written only when `Some`).

- [ ] **Step 1: Write failing tests** in `mod.rs` tests: `temporary_name("prod", 5432) == "_prod-5432"`, `temporary_name("shop_api.1", 80) == "_shop_api.1-80"`, `temporary_name("wéb", 80) == "_w-b-80"`, `temporary_name("a/b", 80) == "_a-b-80"`; `validate_name(&temporary_name(..))` is `Err` (cannot clash); `is_temporary("_x-1")`, `!is_temporary("x")`; `unique_name("_w-80", |n| n == "_w-80") == "_w-80-2"`; `saved_name_for("_w-80") == "w-80"`. In `runtime.rs`: `default_local_port(5432, all free) == Some(5432)`; `5432` in use → `Some(5433)`; `80` Denied, `8080` free → `Some(8080)`; record round trip with `reaches web on prod`; a record text without `reaches` parses with `None`; a failed record with host/forward/reaches round-trips; a saved tunnel's failed record renders no `host` line.
- [ ] **Step 2: Run** `cargo test --lib tunnel` — expect the new tests to fail to compile.
- [ ] **Step 3: Implement** the helpers and the record fields (`rebuild_record` sets `reaches: None`; all existing constructors pass `None`).
- [ ] **Step 4: Run** `cargo test --lib tunnel` — PASS.
- [ ] **Step 5: Commit** `feat: name and record temporary forwards`

### Task 2: Parsing listening ports (domain)

**Files:**
- Create: `src/domain/tunnel/ports.rs` (declare `pub mod ports;` in `mod.rs`)

**Interfaces:**
- Produces:
  - `pub struct Listener { pub address: String, pub port: u16 }`
  - `pub fn parse_ss(output: &str) -> Vec<Listener>` (`ss -Hltn`: local address is column 4)
  - `pub fn parse_netstat(output: &str) -> Vec<Listener>` (`netstat -ltn`: `LISTEN` lines, local address column 4)
  - `pub fn suggested_ports(listeners: &[Listener]) -> Vec<(u16, bool)>` — sorted ascending, 22 dropped, one entry per port; the bool is `loopback_only` (every listener of that port is `127.*` or `::1`).
  - `pub const HOST_PORTS_SCRIPT: &str = "ss -Hltn 2>/dev/null || netstat -ltn 2>/dev/null"` and `pub fn parse_listeners(output: &str) -> Vec<Listener>` (tries `ss` shape, else `netstat`).

- [ ] **Step 1: Write failing tests** with literal outputs: `LISTEN 0 4096 0.0.0.0:22 0.0.0.0:*`, `LISTEN 0 511 [::]:80 [::]:*`, `LISTEN 0 128 *:8080 *:*`, `LISTEN 0 244 127.0.0.1:5432 0.0.0.0:*`, `LISTEN 0 244 [::1]:5432 [::]:*` → `suggested_ports == [(80,false),(5432,true),(8080,false)]`; netstat `tcp6 0 0 :::80 :::* LISTEN` and `tcp 0 0 127.0.0.1:6379 0.0.0.0:* LISTEN` → `[(80,false),(6379,true)]`; empty and garbage input → `[]`.
- [ ] **Step 2: Run** `cargo test --lib ports` — FAIL.
- [ ] **Step 3: Implement.** The port is the text after the last `:`; the address is what precedes it with brackets removed.
- [ ] **Step 4: Run** `cargo test --lib ports` — PASS.
- [ ] **Step 5: Commit** `feat: read the ports a host listens on`

### Task 3: The backend `reach` operation

**Files:**
- Modify: `src/backend/mod.rs` (types, trait method, dispatch `reach`)
- Modify: `src/backend/listing.rs` (`published_ports`)
- Modify: `src/backend/docker.rs`, `src/backend/podman.rs`, `src/backend/proxmox.rs`
- Modify: `docs/architecture.md` (backend interface: list, shell, **reach**; the "Add a backend" arm list gains `reach`)

**Interfaces:**
- Consumes: `domain::tunnel::ports::{parse_ss, Listener}` (Proxmox).
- Produces:
  - `pub struct PortOffer { pub port: u16, pub published: Option<u16>, pub loopback_only: bool }`
  - `pub struct Reach { pub address: Option<String>, pub host_network: bool, pub ports: Vec<PortOffer> }`
  - trait `Backend`: `fn reach<E>(&self, target: &Target, container: &Container, run: &mut dyn FnMut(&[String]) -> RunResult<E>) -> Result<Reach, ListFailure<E>>`
  - `pub fn reach<E>(backend: BackendId, …same…)` dispatch.
  - `listing::published_ports(ports: &str) -> Vec<(u16 /*host*/, u16 /*container*/)>` — parses `0.0.0.0:8080->80/tcp, [::]:8080->80/tcp, 5432/tcp`; tcp only; deduplicated.
- Docker and Podman run `[<program>, "inspect", "--format", "{{json .NetworkSettings.Networks}}|{{json .Config.ExposedPorts}}|{{.HostConfig.NetworkMode}}", <id>]` (through `sg` when the listing's `via` was `Sg`: reuse how `shell_words` applies `via`). Address: the `IPAddress` of the first network by name that has a non-empty one. `host_network`: NetworkMode `host`. Ports: published ones from `container.ports` (`published: Some(host)`), then exposed ones not published, ascending.
- Proxmox runs, through the same root/`sudo -n` path as its listing: `pct exec <vmid> -- sh -c 'ip -4 -o addr show scope global; echo ---; ss -Hltn 2>/dev/null || true'`. Address: first `inet a.b.c.d/nn`, without the prefix. Ports from `parse_ss` through `suggested_ports`.

- [ ] **Step 1: Write failing tests** (adapter unit tests with a fake `run` returning canned `Output`): `published_ports("0.0.0.0:8080->80/tcp, [::]:8080->80/tcp, 5432/tcp, 53/udp") == [(8080,80)]`; Docker inspect with networks `{"zeta":{"IPAddress":"10.0.0.9"},"alpha":{"IPAddress":"172.18.0.4"}}` → address `172.18.0.4`; exposed `{"80/tcp":{},"443/tcp":{}}` with row ports `0.0.0.0:8080->80/tcp` → `[PortOffer{port:80,published:Some(8080)}, PortOffer{port:443,published:None}]`; NetworkMode `host` → `host_network: true`; rootless Podman with empty `IPAddress` everywhere → `address: None`; Proxmox output `2: eth0    inet 192.168.1.50/24 brd …\n---\nLISTEN 0 4096 127.0.0.1:5432 0.0.0.0:*\nLISTEN 0 511 0.0.0.0:80 0.0.0.0:*` → address `192.168.1.50`, ports `[(80,false),(5432,true)]`; a non-zero exit maps to the adapter's `plain_issue`.
- [ ] **Step 2: Run** `cargo test --lib backend` — FAIL.
- [ ] **Step 3: Implement** the types, the trait method on each adapter, and the dispatch.
- [ ] **Step 4: Run** `cargo test --lib backend` — PASS.
- [ ] **Step 5: Commit** `feat: ask a backend where a container can be reached`

### Task 4: Launching any tunnel, and the lifecycle of temporary forwards (app)

**Files:**
- Modify: `src/app/tunnels.rs`
- Modify: `src/domain/tunnel/mod.rs` (`TunnelReport` fields)
- Test: `tests/tunnels.rs` (fake `ssh` already there)

**Interfaces:**
- Consumes: Task 1 helpers and record fields.
- Produces:
  - `TunnelReport` gains `pub temporary: bool, pub reaches: String`.
  - `fn launch_checked(&self, tunnel: &Tunnel, reaches: Option<&str>, cancelled: Option<&AtomicBool>, in_tui: bool) -> Result<StartOutcome, AppError>` — the body of today's `start_tunnel` after the file lookup (lock, already-running, `check_tunnel`, collisions with running saved **and temporary** tunnels, bind checks, `launch`). `start_tunnel(name)` finds a saved tunnel, else a temporary record (its `host`/`forward`/`reaches`), and calls it.
  - `pub fn start_temporary(&self, tunnel: Tunnel, reaches: &str, cancelled: Option<&AtomicBool>, in_tui: bool) -> Result<StartOutcome, AppError>` — `tunnel.name` must be temporary; on a first-start `Failed` it removes the record (spec: a first start that fails writes nothing); on a restart failure it keeps a failed record with host/forward/reaches.
  - `App::tunnels()` returns saved reports, then temporary ones (names from `runtime.recorded_names()` plus the process scan, filtered by `is_temporary`), newest `started` first; `reaches` per Global Constraints.
  - `stop_tunnel` on a temporary name stops it and removes its record and log (already the behaviour once `saved_name` falls back to records).
  - `TunnelRefusal::NotTemporary(String)` for saving a saved tunnel.

- [ ] **Step 1: Write failing tests** in `tests/tunnels.rs`: `a_temporary_forward_starts_lists_and_stops` (start `_prod-<port>`; `tunnels --json` has it last with `"temporary": true, "reaches": "prod"`; stop removes `_prod-<port>.run`); `a_temporary_forward_that_fails_first_leaves_nothing`; `a_failed_restart_keeps_the_temporary_forward_listed` (kill the ssh, `tunnel start _prod-<port>` against a failing fake → listed `failed`, record keeps `host`); `a_saved_tunnel_cannot_start_on_a_temporary_forwards_port` (refusal names `_prod-<port>`); `a_busy_default_local_port_is_refused` (Review Focus 2).
- [ ] **Step 2: Run** `cargo test --test tunnels` — FAIL.
- [ ] **Step 3: Implement** the split, the temporary listing and the record handling. Existing tunnel tests stay green unchanged except for the two new report fields.
- [ ] **Step 4: Run** `cargo test --test tunnels && cargo test --lib` — PASS.
- [ ] **Step 5: Commit** `feat: start, list and stop temporary forwards`

### Task 5: Suggestions, quick forwards and saving (app)

**Files:**
- Create: `src/app/forwards.rs` (declare in `src/app.rs`)
- Test: `tests/tunnels.rs`, `src/app/forwards.rs` unit tests with the container fixture of `src/app/containers.rs`

**Interfaces:**
- Consumes: Task 2 (`HOST_PORTS_SCRIPT`, `parse_listeners`, `suggested_ports`), Task 3 (`backend::reach`), Task 4 (`start_temporary`).
- Produces:
  - `pub enum ForwardTarget { Host(String), Container { target: Target, backend: BackendId, container: Container, via: Option<Via> } }` with `fn label(&self) -> String` (`web on prod` / `CT 101 on pve` / `prod`) and `fn short(&self) -> String` (`web` via `display_name` / `101` / `prod`).
  - `pub struct PortSuggestion { pub port: u16, pub note: Option<String> }` (`published 8080`, `loopback only`).
  - `pub struct Suggestions { pub ports: Vec<PortSuggestion>, pub reach: Option<Reach>, pub unavailable: Option<String> }`
  - `pub fn forward_suggestions(&self, target: &ForwardTarget, cancelled: Option<&AtomicBool>) -> Suggestions` — never fails: errors go to `unavailable`.
  - `pub fn quick_forward(&self, target: &ForwardTarget, reach: Option<&Reach>, remote: u16, local: Option<u16>, cancelled: Option<&AtomicBool>, in_tui: bool) -> Result<(Tunnel, StartOutcome), AppError>` — destination: host → `localhost:<remote>`; published → `localhost:<published>`; `host_network` → `localhost:<remote>`; else `<address>:<remote>`; no address → `TunnelRefusal::Unreachable { label }` ("… has no address the host can reach; publish the port"); a `Target::Local` container → `TunnelRefusal::LocalTarget`. Name: `unique_name(temporary_name(short, remote), running temporary names)`, replacing a failed one of the same name. Local: `local.or(default_local_port(remote, |p| try_bind("127.0.0.1", p)))`.
  - `pub fn local_addresses(&self, container: &Container, reach: &Reach) -> Vec<(u16, String)>` — published → `127.0.0.1:<host>`; on Linux also `<address>:<port>` for unpublished ports (cfg `target_os = "linux"`).
  - `pub fn save_tunnel(&self, temporary: &str, name: &str) -> Result<SaveOutcome, AppError>` with `pub struct SaveOutcome { pub tunnel: Tunnel, pub restarted: Option<StartOutcome>, pub note: Option<String> }` — `add_tunnel`, then if running: `stop_tunnel(temporary)` and `start_tunnel(name)`; if failed: `forget_run(temporary)`; `note` per Global Constraints when the destination is not `localhost`.

- [ ] **Step 1: Write failing tests:** host suggestions from a fake `ssh` printing the `ss` sample of Task 2; suggestions when `ssh` exits 255 → `ports: []`, `unavailable: Some(..)` (Review Focus 1); `quick_forward` to a published port builds `-L 127.0.0.1:8080:localhost:8080`; to an unpublished port `-L 127.0.0.1:443:172.18.0.4:443`; rootless Podman without address → `Unreachable`; a second forward to the same running target gets `-2`; `save_tunnel` on a running temporary forward leaves the saved tunnel running under the new name and no `_` record; saving a container-address forward returns the note.
- [ ] **Step 2: Run** `cargo test --lib forwards && cargo test --test tunnels` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same — PASS.
- [ ] **Step 5: Commit** `feat: forward a port of a host or container in one step`

### Task 6: The CLI and its JSON

**Files:**
- Modify: `src/cli/mod.rs` (`Command::Forward`), `src/cli/tunnels.rs` (`TunnelCommand::Save`, listing marker), `src/cli/json.rs` (`temporary`, `reaches`, `forward` document)
- Create: `src/cli/forward.rs`
- Modify: `docs/json-output.md`, `tests/fixtures/json/tunnels.json` (+ new `forward.json`), `tests/json.rs`, `tests/cli.rs`

**Interfaces:**
- Consumes: Task 5.
- Produces: `portico forward [CONTAINER] [--on [ALIAS]] [--backend docker|podman|proxmox] [--port REMOTE] [--local LOCAL] [--json]` and `portico tunnel save <NAME> [--name NAME] [--json]`, as the spec's CLI section says. Container lookup reuses the `portico shell` resolution path (`backend::resolve` over every installed backend unless `--backend`). Without `--port`: interactive → `picker` list of suggestions with free entry; non-interactive → exit 2 with `--port is required; suggested: 80, 443`. `portico tunnels` prints temporary rows after saved ones with a `temporary` marker.

- [ ] **Step 1: Write failing tests:** `tests/cli.rs` `forward_requires_a_port_when_not_interactive`, `forward_prints_the_address_and_how_to_keep_it` (exact copy from Global Constraints), `forward_to_a_local_container_prints_where_it_is_reachable`, `tunnel_save_keeps_a_temporary_forward`; `tests/json.rs` goldens: `tunnels.json` gains `"reaches": "prod", "temporary": false` per tunnel and one temporary entry; `forward.json` for `portico forward --json`.
- [ ] **Step 2: Run** `cargo test --test cli --test json` — FAIL.
- [ ] **Step 3: Implement** and document both commands and fields in `docs/json-output.md` (golden markers as the file already uses).
- [ ] **Step 4: Run** `cargo test --test cli --test json --test docs` — PASS.
- [ ] **Step 5: Commit** `feat: add portico forward and portico tunnel save`

### Task 7: The forward dialog in the hosts and container tabs (TUI)

**Files:**
- Create: `src/tui/forward.rs` (the dialog and its suggestions worker)
- Modify: `src/tui/hosts.rs`, `src/tui/containers.rs` (`f` → action), `src/tui/mod.rs` (`UiAction::OpenForward(ForwardTarget)`, `UiAction::Tunnels(TunnelAction::Forward{..})`, help lines), `src/tui/tunnels.rs` (start worker accepts a quick forward)
- Modify: `src/platform/launcher.rs` + per-system modules: `pub fn open_url(url: &str) -> io::Result<()>` (`xdg-open`; `open`; `cmd /c start "" <url>`)
- Create in `src/tui/terminal.rs`: `pub(super) fn copy_osc52(text: &str) -> io::Result<()>` (writes `ESC ] 52 ; c ; <base64> BEL` to stdout; base64 written by hand, no dependency)

**Interfaces:**
- Consumes: Task 5 (`ForwardTarget`, `forward_suggestions`, `quick_forward`, `local_addresses`).
- Produces: `PanelAction::Forward { target: Target, backend: BackendId, container: Container, via: Option<Via> }` and a hosts-tab `UiAction::OpenForward(ForwardTarget::Host(alias))`; `TunnelAction::Forward { target: ForwardTarget, reach: Option<Reach>, remote: u16, local: Option<u16> }`.

- [ ] **Step 1: Write failing tests** (TUI unit tests in the style of `press(...)`): `f` on a host row opens the dialog titled `Forward a port of prod`; typing digits fills the remote port while suggestions load; the dropdown lists `80`, `5432  loopback only`, `8080  published`; Enter sends `TunnelAction::Forward` with the values; on `Started` the tab becomes `Tab::Tunnels` with `_prod-80` selected; a `Failed` outcome keeps the dialog open with the reason; suggestions `unavailable` shows its line and keeps free entry (Review Focus 1); `f` on a "this machine" container row shows `127.0.0.1:8080` with `y`/`b` hints and sends no forward; `copy_osc52("127.0.0.1:8080")` produces `\x1b]52;c;MTI3LjAuMC4xOjgwODA=\x07`.
- [ ] **Step 2: Run** `cargo test --lib tui` — FAIL.
- [ ] **Step 3: Implement.** Suggestions load on a worker thread as `lister` does; the start goes through the tunnels start worker so Esc cancels it.
- [ ] **Step 4: Run** `cargo test --lib tui` — PASS.
- [ ] **Step 5: Commit** `feat: forward a port from the hosts and container tabs`

### Task 8: The reworked tunnels tab (TUI)

**Files:**
- Modify: `src/tui/tunnels.rs`, `src/tui/mod.rs` (`TunnelAction::Save { temporary, name }`)

**Interfaces:**
- Consumes: Task 4 report fields, Task 5 `save_tunnel`, Task 7 `copy_osc52`, `open_url`.

- [ ] **Step 1: Write failing tests:** rows show name (temporary ones followed by `temporary`), reaches, local address, status, with temporary rows after saved ones; `w` on a temporary row opens a name prompt prefilled `prod-80`, Enter sends `TunnelAction::Save`; `w` on a saved row does nothing; `e` on a temporary row does nothing; `x` on a temporary row asks `Stop and remove _prod-80? (y/n)`; `y` sets the status `copied 127.0.0.1:8080`; `b` calls the opener with `http://127.0.0.1:8080`; `s` on a failed temporary row sends `TunnelAction::Start("_prod-80")`; the save note is shown in the status line.
- [ ] **Step 2: Run** `cargo test --lib tui::tunnels` — FAIL.
- [ ] **Step 3: Implement**, updating the help line.
- [ ] **Step 4: Run** `cargo test --lib` — PASS.
- [ ] **Step 5: Commit** `feat: list temporary and saved forwards together in the tunnels tab`

### Task 9: End-to-end tests and documentation

**Files:**
- Modify: `tests/e2e/json.rs` (or a new `tests/e2e/forward.rs` registered in `tests/e2e.rs`)
- Modify: `README.md` (tunnels section: quick forwards, `portico forward`, `tunnel save`), `ROADMAP.md` (remove v6 item "Quick port forwards", renumber), the spec's status to `Approved; implemented`, `docs/architecture.md` (`domain::tunnel` row mentions temporary forwards and `ports`; `app` row adds `forwards`; `platform::launcher` row adds `open_url`)

- [ ] **Step 1: Write the e2e test** `a_quick_forward_reaches_a_port_and_is_saved`: start a TCP listener on `127.0.0.1:<free>` on the test sshd's side (the same machine), run `portico forward --on box --port <free> --local <other> --json`, connect to `127.0.0.1:<other>` and read a byte, `tunnels --json` shows `"temporary": true`, `tunnel save _box-<free> --name kept` keeps it running as `kept`, `tunnel stop kept` ends it. Drop guard stops all tunnels.
- [ ] **Step 2: Run** `scripts/ci-checks.sh e2e` as a regular user — PASS on Linux.
- [ ] **Step 3: Update the docs** listed above; run `cargo test --test docs` — PASS.
- [ ] **Step 4: Run** `scripts/ci-checks.sh` (all default checks) — PASS.
- [ ] **Step 5: Commit** `docs: document quick port forwards` (and `test:` for the e2e file if separate).
