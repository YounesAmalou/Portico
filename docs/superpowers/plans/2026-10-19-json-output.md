# Machine-Readable Output Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `--json` flag on `hosts`, `show`, `identities`, `containers`, `tunnels` and `tunnel show|start|stop` that prints one JSON document per run on stdout (`"schema": 1`), success or failure, never prompting, carrying nothing the human output does not show, documented in `docs/json-output.md` and pinned by golden files.

**Architecture:** `src/cli/json.rs` renders the existing report types (`HostInventory`, `HostDetails`, `HostIdentities`, `IdentityInventory`, `ContainerList`, `TunnelReport`, the start and stop outcomes) and the errors (`AppError`, `ContainerError`, `TunnelRefusal`, a missing name) as `serde_json::Value`s, and prints a document with `schema` added. `src/cli.rs` and `src/cli/tunnels.rs` gain a `json` flag per command; in JSON mode a command checks for a missing name first, calls the same `App` function as the human path (containers with `batch = true`, never `list_with_offers`) and prints the document. Every error of a JSON-mode command becomes an error document on stdout with exit status 1.

**Tech Stack:** Rust 2024, clap derive, `serde_json` 1 (new; default features, no `preserve_order`).

**Spec:** `docs/superpowers/specs/2026-10-19-json-output-design.md`

## Global Constraints

- The domain types gain no derive and no serde attribute; field names of the contract are spelled only in `src/cli/json.rs`.
- Keys come out sorted (`serde_json::Map` is a `BTreeMap`); a unit test fails if `preserve_order` is ever unified in.
- In JSON mode nothing is written to stderr by Portico, no picker or prompt runs even with a terminal, and ssh for a host's containers runs in batch mode.
- `hosts --json` runs no `ssh`, `ssh-add` or `ssh-keygen`.
- Exit status: 0 success, 1 failure (any failed tunnel result), never 130.
- No credentials, environment values (other than the agent socket path), `ProxyCommand` or container command lines in any document.
- Before each commit run `scripts/ci-checks.sh fmt`, `clippy` and `test` as a regular user; run `e2e` and `docs` in the last task.
- Conventional Commits; no AI attribution anywhere.

## Review Focus

- **One document, always:** errors before output (an unreadable config), missing names and per-command failures all print an error document on stdout with exit 1 and an empty stderr. Pinned by `tests/json.rs` (`errors_are_documents_on_stdout`).
- **Never interactive:** pinned by a unit test in `src/cli.rs` that runs every JSON command without a name with `interactive = true` and a picker that panics, and by `containers_on_a_host_run_ssh_in_batch_mode`.
- **Nothing sensitive:** pinned by `nothing_sensitive_is_printed`.
- **Docs cannot drift:** every golden file appears verbatim in `docs/json-output.md` after a `<!-- golden: <file> -->` marker. Pinned in `tests/docs.rs`.

---

### Task 1: Renderers (`src/cli/json.rs`)

**Files:** modify `Cargo.toml`, `src/cli.rs` (module declaration); create `src/cli/json.rs`.

- [ ] Add `serde_json = "1"`; check `cargo tree -e features -i serde_json` shows no `preserve_order`.
- [ ] `hosts(&HostInventory)`, `details(&HostDetails)`, `host_identities(&HostIdentities)`, `inventory(&IdentityInventory)`, `containers(&ContainerList)`, `report(&TunnelReport)`, `status(&TunnelStatus)`, `forward(&Forward)`, `public_key`, `agent`, `availability`, `warning(&DiscoveryWarning)`; `error(kind, message, next)` and the mappings from `AppError`, `ContainerError` (docker `problem`, the `portico containers` hint when an offer exists, ssh explanations) and the start outcome; `print(output, Value)` that inserts `schema: 1` and writes pretty JSON plus a newline.
- [ ] Unit tests: each status variant, agent state and availability; dynamic forward nulls; external `declared: null`; docker problem; lossy non-UTF-8 path; escaped control characters; sorted keys guard.

### Task 2: Wire the flag

**Files:** modify `src/cli.rs`, `src/cli/tunnels.rs`.

- [ ] `#[arg(long)] json: bool` on `Hosts`, `Show`, `Identities`, `Containers`, `Tunnels`, `TunnelCommand::{Show, Start, Stop}`.
- [ ] `execute` computes `json` for the command; `run` prints the error as a document when it is set. Missing names give `usage` errors before anything runs.
- [ ] `containers --json` calls `app.containers(&target, true, None)` and maps a failure to its error document.
- [ ] `tunnel start|stop --json` build one result per name (`name`, `outcome`, `pid`, `error`, `tunnel` after the action) and exit 1 when any failed.
- [ ] Unit test: every JSON command without a name, `interactive = true`, a panicking picker → `usage` document.

### Task 3: CLI tests and golden files (`tests/json.rs`, `tests/fixtures/json/`)

- [ ] A `golden(name, stdout, home)` helper: replace the temporary home with `/home/me`, compare with `tests/fixtures/json/<name>`, rewrite when `PORTICO_BLESS=1`.
- [ ] Golden documents: hosts (with a warning), show managed with identities, show external with a resolution error, identities, containers local and on a host, docker failure with an offer, tunnels (stopped, failed, port in use), tunnel show of a failed tunnel, start (started and refused), stop --all with nothing running, usage error, unknown host error.
- [ ] `hosts_runs_no_ssh`, `errors_are_documents_on_stdout` (unreadable config), `containers_on_a_host_run_ssh_in_batch_mode`, `docker_offers_are_not_run`, `nothing_sensitive_is_printed`; every test checks stderr is empty.

### Task 4: End to end (`tests/e2e/json.rs`)

- [ ] `show --json` against the throwaway sshd; `tunnel start --json` reports `started` and `running` with the PID `tunnels --json` then shows; `stop --json` reports `stopped`; a busy port gives `failed`, `port-in-use`, exit 1.

### Task 5: Docs

- [ ] `docs/json-output.md`: conventions, each document with its golden example, the error kinds, versioning (public contract), the consumer advice (call `show` only on open; run `start` and `stop` asynchronously; check `schema`).
- [ ] Doc guard in `tests/docs.rs`; README section; spec "Approved; implemented"; ROADMAP v3 item removed and renumbered; v1 design's delivered slices bullet.
- [ ] `scripts/ci-checks.sh` (all checks) and `HOME=$(mktemp -d) SSH_AUTH_SOCK= XDG_RUNTIME_DIR= cargo test --all-targets`.
