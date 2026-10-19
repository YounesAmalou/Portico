# Identity Awareness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Portico shows which identities each host would offer and whether each is available (in an agent, a key file, missing or unchecked), lists every known identity with `portico identities`, and suggests identities in the TUI IdentityFile field, without ever reading private key material.

**Architecture:** A pure module, `src/identity.rs`, holds the credential-free data model, path expansion, agent socket resolution, provider labels, the `ssh-add -l`/`ssh-keygen -l` line parser and the classification. `src/app/identities.rs` runs `ssh-add` and `ssh-keygen` (with a deadline and a prompt-free environment), scans `~/.ssh`, caches agent listings in `App`, and builds the inventory. The CLI and TUI only render.

**Tech Stack:** Rust 2024, the existing crates. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-10-19-identity-awareness-design.md`

## Global Constraints

- Never open a private key file: only `std::fs::metadata` on it. `ssh-keygen -l -f` is only given paths ending in `.pub` that are regular files.
- `ssh-add` runs as `ssh-add -l -E sha256` with stdin null, `SSH_AUTH_SOCK` set to the socket, `SSH_ASKPASS` removed and `SSH_ASKPASS_REQUIRE=never`, in its own process group, killed and reaped at a 3 s deadline.
- An agent socket is asked at most once per `App` (one CLI process, one TUI session) until `App::forget_identities()`; a listing is never cancelled by a selection change.
- Every line `portico show` printed before stays byte-for-byte; the identities block is appended before the warnings.
- Type, field and variant names in `identity.rs` are the future `--json` contract.
- Before each commit run `scripts/ci-checks.sh fmt`, `clippy` and `test` as a regular user; the e2e suite before the PR.
- Conventional Commits, no AI attribution.

## Review Focus

1. Deadline: a fake `ssh-add` that sleeps is killed at the deadline and its process is gone (`list_agent_kills_a_listing_past_the_deadline`).
2. Environment: a fake `ssh-add` records its environment (`list_agent_runs_without_askpass`).
3. Private files are never opened: e2e `identities_never_open_private_keys` makes the private key mode 000 and expects identical output.
4. Agent asked once: two hosts on one socket run `ssh-add` once (`agent_listing_is_cached_per_socket`), and again after `forget_identities`.
5. `show` compatibility: `show_keeps_existing_lines_unchanged` compares the pre-slice lines.

---

### Task 1: Pure model (`src/identity.rs`)

- [ ] Types: `PublicKey`, `Provider`, `AgentProblem`, `Agent`, `MissingReason`, `Availability`, `Identity`, `HostIdentities`, `InventoryAgent`, `InventoryFile`, `IdentityInventory`.
- [ ] `expand(value, &Expansion) -> Result<PathBuf, String>`: `~`, `%d %u %h %r %p %n %i %%`, `${VAR}`; any other token is returned as the error.
- [ ] `AgentSocket::from_config(identity_agent, auth_sock, &Expansion)`: absent/`SSH_AUTH_SOCK` → environment, `$VAR` → that variable, `none` → disabled, else a path.
- [ ] `provider(socket)`, `parse_listing(text)` (bits, fingerprint, comment with spaces, `(TYPE)`), `public_key_path(path)`.
- [ ] `classify(...)` from the resolved list, the defaults, per-identity facts and the agent; defaults collapsed; `also_offered` only without IdentitiesOnly.
- [ ] Display helpers shared by CLI and TUI: `Availability::word()`, `Agent::summary()`, `tilde(path, home)`, `short_fingerprint`.
- [ ] Unit tests for each function and every availability branch.

### Task 2: OpenSSH adapter (`src/openssh.rs`)

- [ ] `ResolvedConfig` gains `identity_agent: Option<String>`, `identities_only: bool`, `pkcs11_provider: Option<String>`; parser keeps them; existing literals updated.
- [ ] `OpenSsh` gains `ssh_add` (`with_ssh_add`), and `ssh_add_program()`/`keygen_program()` accessors.
- [ ] `default_identity_files()` runs `ssh -F none -G portico-defaults`.
- [ ] Parser unit test against captured output.

### Task 3: Process side (`src/app/identities.rs`)

- [ ] `run_with_deadline(openssh, command, deadline)` over `cancellable_output`, with a watchdog flag; `Timeout` distinguished from failure.
- [ ] `list_agent(socket)`: missing/not-socket checks, environment, exit 0/1/2 mapping, timeout → `NoAnswer`.
- [ ] `fingerprint(pub_path)`: regular `.pub` files only.
- [ ] `App` gains `identities: IdentityState` (defaults `OnceLock`, agent cache with one lock per socket); `host_identities(alias, resolved)`; `forget_identities()`; `identity_inventory()` (every host's `ssh -G`, `~/.ssh/*.pub`, every agent).
- [ ] `HostDetails` gains `identities: Option<HostIdentities>`, filled in `show`.
- [ ] Tests with fake `ssh-add`/`ssh-keygen`: deadline kill, environment, exit codes, cache once per socket and after forget.

### Task 4: CLI (`src/cli.rs`)

- [ ] `show` appends the identities block; test that the old lines are unchanged.
- [ ] `identities` subcommand: agents with keys and their paired `.pub`, then identity files with availability and "used by".
- [ ] CLI tests through fakes.

### Task 5: TUI

- [ ] `src/tui/identities.rs`: detail lines with `✓ ✗ ?` and words, colours; replaces the IdentityFile resolved row.
- [ ] Refresh calls `app.forget_identities()`.
- [ ] `suggest::Origin::Note`; IdentityFile dropdown: whole value as query, candidates from the inventory, missing last, pick inserts `~/` form; Ctrl+Space; footer hint.
- [ ] Inventory worker spawned in `browse`, requested when a form opens, results recompute an open dropdown.
- [ ] Unit tests for lines, dropdown behaviour, ProxyJump unchanged.

### Task 6: End to end (`tests/e2e.rs`)

- [ ] Helper starting `ssh-agent -a <short tmp>/a.sock -D`, killed on drop; keys made with `ssh-keygen -N ''` and loaded with `ssh-add`.
- [ ] `show` in agent / key file / missing / `.pub`-only in agent / `IdentityAgent none` / removed socket; `portico identities`; private key mode 000 changes nothing.

### Task 7: Docs and roadmap

- [ ] README: `portico identities`, `show` block, details section, IdentityFile suggestions, what Portico never touches.
- [ ] Spec status "Approved; implemented"; ROADMAP: remove v3 item 4, renumber, add the unscheduled "Pin an agent key to a host by writing its public half"; v1 design "Delivered follow-up slices" bullet.
- [ ] Full `scripts/ci-checks.sh`, rebase on `origin/main`, PR.
