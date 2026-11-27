# Container Port Forwards Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A tunnel can name its container (`docker shop/api`, `proxmox 101`) and resolve the container's current address at every start, so it survives the container being recreated.

**Architecture:** `Tunnel` gains `container: Option<ContainerTarget>`; the tunnels file writes it as `ContainerForward <listen> <backend> <reference> <port>`. `App` resolves it (listing → `resolve_lowest` → `backend::reach` → the quick-forward destination rule) into a concrete `-L` right before the existing start checks. Quick container forwards record the target and save by name. CLI, JSON and the TUI form expose it.

**Tech Stack:** Rust, ratatui/crossterm, clap, serde_json; no new dependency.

**Spec:** `docs/superpowers/specs/2026-11-27-container-port-forwards-design.md`

## Global Constraints

- No new dependency, no daemon, no database; nothing installed in or started next to a container.
- Layer rules (`docs/architecture.md`): `domain::tunnel` may name `backend::BackendId` (plain enum, as `domain::settings` does) but calls nothing in `backend`; resolution lives in `app`.
- File line, exact: `ContainerForward <listen> <backend> <reference> <port>`, e.g. `ContainerForward 127.0.0.1:8080 docker shop/api 80`; `<listen>` written as `LocalForward` writes its listen part; backends `docker`, `podman`, `proxmox`.
- Refusal texts, exact: `<reference> is not running on <host> (<backend>)`; `<reference> on <host> has no address the host can route to; publish the port, or attach it to a network <host> routes`; CT: `could not read the address of CT <id> on <host>` (exists).
- Warning text, exact: `started, but nothing answers on port <port> inside <reference>; an app that listens only on 127.0.0.1 inside its container cannot be reached`.
- JSON: tunnel objects gain `container: {"backend","reference","port"} | null`; for a container tunnel `forward.target_host` = reference (string), `target_port` = port inside, `description` = `<listen> → <reference>:<port>`; `reaches` for a saved container tunnel = `<reference> on <host>`.
- Saved reference = `backend::remember_key(container)` (Compose `project/service`, else name; a CT's VMID).
- Commits: Conventional Commits, no AI attribution. Run `scripts/ci-checks.sh` checks as a regular user.

## Review Focus

1. A tunnels file written by the previous release (no `ContainerForward`) reads and renders byte-identically. Test in Task 1.
2. A container reference that resolves to nothing on start, while the host is reachable: refused with the exact text, no record left. Test in Task 3.
3. Two Compose projects both with a service `web`: bare `web` is refused as ambiguous, `shop/web` resolves. Test in Task 2.
4. A container recreated with a new address between two starts: the second start uses the new one. Test in Task 3 and e2e in Task 6.
5. `tunnel edit` on a container tunnel with `-L` given: refused, file unchanged. Test in Task 4.

---

### Task 1: The container target in the model and the tunnels file (domain)

**Files:** Modify `src/domain/tunnel/mod.rs` (model, parse, render, display, tests); `docs/architecture.md` (`domain::tunnel` row; note the second sideways import of `BackendId`).

**Interfaces — Produces:**
- `pub struct ContainerTarget { pub backend: BackendId, pub reference: String, pub port: u16 }` (Clone, Debug, Eq, PartialEq).
- `Tunnel` gains `pub container: Option<ContainerTarget>`; `Tunnel::new(..)` keeps its signature (container `None`); `pub fn Tunnel::to_container(name, host, listen: Forward /* kind Local, destination ignored */, target: ContainerTarget) -> Result<Tunnel, TunnelError>` — validates the reference (non-empty, no whitespace or control characters) and forces `forward.destination = Some(Destination { host: reference, port })` so every existing reader of `destination` sees the reference string.
- `TunnelsFile::parse` accepts `ContainerForward`; `render` writes it for container tunnels.
- `BackendId::from_str_name(&str) -> Option<BackendId>` (in `backend/mod.rs`) if none exists.

- [ ] Step 1: failing tests — round trip of the spec's example block; an old file (only `LocalForward`) renders byte-identically; `ContainerForward 127.0.0.1:8080 lxc web 80` refused on its line (unknown backend); missing port refused; `Display` of a container tunnel `api (127.0.0.1:8080 → shop/api:80 through prod)`.
- [ ] Step 2: `cargo test --lib tunnel` — FAIL. Step 3: implement. Step 4: PASS (fix every `Tunnel { .. }` literal in the crate with `container: None`).
- [ ] Step 5: commit `feat: name a container as a tunnel's target in the tunnels file`.

### Task 2: Resolving a reference to the lowest replica (backend listing)

**Files:** Modify `src/backend/listing.rs`.

**Interfaces — Produces:** `pub fn resolve_lowest<'a>(reference: &str, containers: &'a [Container]) -> Result<&'a Container, ResolveError>` — as `resolve`, except that when every match shares one `project` and `service`, it returns the one with the lowest `number`.

- [ ] Step 1: failing tests — `shop/api` with `api#1`,`api#2` → `#1`; `shop/api#2` → `#2`; bare `web` in projects `shop` and `blog` → `Ambiguous`; a plain name → that container; nothing → `NotFound`.
- [ ] Steps 2-4: run, implement, pass (`cargo test --lib listing`).
- [ ] Step 5: commit `feat: resolve a Compose service to its lowest replica`.

### Task 3: Resolving at start, the record line, and "nothing answers" (app)

**Files:** Modify `src/app/tunnels.rs`, `src/app/forwards.rs`, `src/domain/tunnel/runtime.rs` (record `container` line), `src/domain/tunnel/mod.rs` (`TunnelWarning::NothingAnswers { port, reference }`, `TunnelRefusal` texts live in app).

**Interfaces:**
- Consumes: Task 1 `ContainerTarget`/`Tunnel.container`; Task 2 `resolve_lowest`; existing `App::containers`, `backend::reach`, `forwards::destination`.
- Produces:
  - `fn resolve_container(&self, host: &str, target: &ContainerTarget, cancelled: Option<&AtomicBool>) -> Result<Tunnel /* concrete -L */, AppError>` used by `launch_checked`'s callers for any tunnel with `container: Some(_)` (saved start and temporary restart).
  - `TunnelRefusal::NotRunning { reference, host, backend }`, `TunnelRefusal::Unroutable { reference, host }` (exact texts in Global Constraints); the existing `NoCtAddress` for a CT.
  - `RunningRecord`/`Record::Failed` gain `container: Option<String>` written as `container <backend> <reference> <port>` for temporary forwards only.
  - `StartOutcome::Started { pid, warning: Option<String> }` (or a sibling field) carrying the "nothing answers" text; after `Started` for a container tunnel, connect once to the local port, wait up to 800 ms for the log to gain `open failed: connect failed`, close.
  - `App::tunnels()` adds `TunnelWarning::NothingAnswers` for a running container tunnel whose log has that line; `reaches` = `<reference> on <host>` for saved container tunnels.
  - `quick_forward` on a container records `ContainerTarget { backend, reference: remember_key(container), port: remote }` in the temporary record; `save_tunnel` saves a container tunnel (no address note any more; delete that note's code and test).
- [ ] Step 1: failing tests (fake ssh + fake docker, as `src/app/forwards.rs` tests): saved container tunnel starts against `172.18.0.4:80`; fake address changes to `172.18.0.9`, restart uses it; container missing → `NotRunning` with exact text, no record; rootless (no address, not published) → `Unroutable`; log containing `channel 2: open failed: connect failed: Connection refused` → `Started` with the warning and `tunnels()` warning; quick container forward restart re-resolves; `save_tunnel` writes a `ContainerForward` line with `shop/api`.
- [ ] Steps 2-4: run (`cargo test --lib app::`), implement, pass; `cargo test --lib` and `cargo test --test tunnels` stay green.
- [ ] Step 5: commit `feat: look a container tunnel's address up at every start`.

### Task 4: CLI and JSON

**Files:** `src/cli/tunnels.rs` (`add`/`edit` flags, `show`, list text), `src/cli/json.rs` (`container`, `target_host`, `reaches`), `docs/json-output.md`, `tests/fixtures/json/*.json` goldens, `tests/cli.rs`, `tests/json.rs`, desktop QML test if the golden row count changes (`desktop/quickshell/tests/tst_portico.qml`; tests only).

**Interfaces — Produces:** `portico tunnel add <name> --on <alias> --container <ref> [--backend docker|podman|proxmox] --port <inside> [--local <port>]`; `portico tunnel edit <name> [--container <ref>] [--backend ..] [--port ..] [--address <host>]`; flag rules exactly as the spec's "Calls" section. Without `--backend`, `add` lists the host with every installed backend (as `portico shell` resolves) and stores the backend that has the container; a container that is not running then needs `--backend` (`error: shop/api is not running on prod; give --backend to save it anyway`). The `ssh -G` check uses `localhost:<port>` as a stand-in destination. Start output prints the warning on its own line after `Started <name> …`.

- [ ] Step 1: failing tests — `tunnel add api --on prod --container shop/api --port 80` writes the `ContainerForward` line with the detected backend; `edit` with `-L` on it refused (file unchanged, Review Focus 5); `--backend` alone on an address tunnel refused; `tunnel show api` shows `shop/api:80 (docker)`; `tunnels --json` golden gains a container tunnel with `container` and `target_host: "shop/api"`; a start printing the nothing-answers warning.
- [ ] Steps 2-4: run (`cargo test --test cli --test json --test docs`), implement and document, pass.
- [ ] Step 5: commit `feat: add and edit container tunnels from the CLI`.

### Task 5: The TUI form and tunnels tab

**Files:** `src/tui/tunnels.rs`, `src/tui/mod.rs` (only if dispatch changes).

**Interfaces:** Consumes Task 1 (`Tunnel::to_container`) and Task 4's flag rules. The form gains a `Container` field after `Host` and a backend chooser beside it (Space / ←→, Docker → Podman → Proxmox, as Kind cycles); when Container is non-empty: Kind fixed to local and shown as such, Target host hidden, Target port labelled `Port inside`. Editing a container tunnel prefills both. The tunnels tab's target text shows `shop/api:80 (docker)`; a running tunnel with the NothingAnswers warning shows it like other warnings.

- [ ] Step 1: failing tests — typing a container hides Target host and saves `TunnelAction::Add` with `container: Some(ContainerTarget{Docker,"shop/api",80})`; Space on the backend chooser cycles; editing prefills; clearing Container restores an address form; the warning renders.
- [ ] Steps 2-4: run (`cargo test --lib tui::`), implement, pass.
- [ ] Step 5: commit `feat: set a tunnel's container in the TUI form`.

### Task 6: End-to-end test and documentation

**Files:** `tests/e2e/tunnels.rs` (+ helpers in `tests/e2e/sshd.rs` or `client.rs` to put a fake `docker` on the server side's `PATH`), `README.md`, `ROADMAP.md` (remove the v6 section; v6 is done), `docs/architecture.md` if not already complete, this spec's status → `Approved; implemented`, the v1 design's "Delivered follow-up slices" list.

- [ ] Step 1: e2e test `a_container_tunnel_follows_its_container`: the fake `docker` (a `sh` script; on Windows the e2e already runs scripts through Git for Windows' `sh` — check how other e2e fakes are installed and follow that; if the server side cannot run a fake there, mark the test `#[cfg(unix)]` and say so) answers `ps` with one container `web` and `inspect` with address `127.0.0.1` and exposed port `<listener port>`; `tunnel add api --on box --container web --backend docker --port <p> --local <l>`; start; read a byte; switch the fake to a second listener's port is not possible (port is fixed) — instead switch the fake's address from `127.0.0.1` to `127.0.0.2` (Linux only, else skip that half), restart, read again.
- [ ] Step 2: `scripts/ci-checks.sh e2e` — PASS.
- [ ] Step 3: docs; `cargo test --test docs` — PASS.
- [ ] Step 4: `scripts/ci-checks.sh` fmt, clippy, test, docs, desktop, e2e, package — PASS; clippy for `x86_64-pc-windows-gnu` and `x86_64-apple-darwin`.
- [ ] Step 5: commits `test: follow a container tunnel end to end`, `docs: document container tunnels`.
