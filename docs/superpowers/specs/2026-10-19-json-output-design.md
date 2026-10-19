# Machine-Readable Output Design

**Status:** Draft; awaiting review

## Purpose

The desktop panel of the next v3 slice (a Quickshell component) lists hosts and their containers to connect to, and the saved tunnels with their status and a start or stop action. It has no daemon to ask: it runs the `portico` CLI when it opens and parses what comes back. Today every command prints text for people, aligned in columns that can change at any time.

This slice adds a `--json` flag to the commands that panel reads, printing one JSON document per run with stable field names and a schema version, documented for consumers and pinned by golden files. It carries no credentials and nothing the human output does not already show.

It implements v3 item 1 of `ROADMAP.md`, "Machine-readable output", which the v1 design moved there from the dropped herdr slice. It is defined by the panel's needs: anything the panel does not read is left out, and can be added later without breaking consumers (see "Versioning").

## Settled questions

- **The commands that get `--json`** are the read commands the panel needs, plus the two actions it runs, plus `identities` because its report already exists:

  | Command | Why |
  |---|---|
  | `portico hosts` | The panel's host list |
  | `portico show <alias>` | One host's details, with its identities (asked for by the roadmap item) |
  | `portico containers [--on <alias>]` | A host's (or this machine's) containers to open a shell in |
  | `portico tunnels` | The saved tunnels and their status |
  | `portico tunnel show <name>` | One tunnel, with its command line and, when failed, its log |
  | `portico tunnel start <name>...` | The panel's start action, reporting the new status |
  | `portico tunnel stop <name>... \| --all` | The panel's stop action, reporting the new status |
  | `portico identities` | Cheap: `IdentityInventory` is already built for the human output |

  Not `init`, `add`, `edit`, `remove`, `adopt`, `export` or `tunnel add|edit|remove`: management stays in the TUI and CLI (ROADMAP v3 item 2: "does not duplicate management"). Not `connect` or `shell`: they hand the terminal to `ssh`, and the panel runs them as they are (`portico connect <alias> --new-terminal`, `portico shell <id> --on <alias> --new-terminal`) and reads only their exit status. Not `test`: the panel does not test connections, and its result has a rich model of its own; it can be added later.
- **`--json` is a flag of each of those commands,** not a global flag: `portico add --json` is a usage error rather than a flag silently ignored. It has no short form.
- **One document per run, on stdout, success or failure.** A consumer reads stdout to its end and parses it once; it never has to read stderr, and never has to tell a partial line from a finished one. The document is a JSON object, pretty-printed with two-space indentation and a trailing newline, in UTF-8. Whitespace and key order are not part of the contract (keys come out sorted).
- **Every document has `"schema": 1`** at the top level, including error documents. The number changes only on a breaking change (see "Versioning"). It is not the Portico version.
- **JSON mode never reads the terminal.** No picker, no confirmation, no offer to fix docker, no ssh password prompt: a missing name is a `usage` error, docker's offers are reported but not run, and the ssh that lists a host's containers runs with `BatchMode=yes` (the batch path the TUI already uses), so a host that needs a password fails with an explanation instead of waiting for input nobody sees. Tunnels already start in batch mode. Even with a terminal on stdin, `--json` behaves as if there were none.
- **Exit status is the human one:** 0 when the command did what was asked (including "already running" and "not running", as today), 1 when it failed or any named tunnel failed, never 130 (nothing can be cancelled). Clap's own argument errors (an unknown flag, `--json` on a command without it) stay human text on stderr with exit status 2: they happen before Portico knows what was asked, and a consumer that sees 2 has a bug, not a condition to handle.
- **In JSON mode Portico writes nothing to stderr.** What it printed there (explanations, warnings, ssh's captured stderr for a failed listing) moves into the document. The consumer may still discard or log stderr: a child program Portico does not capture could in principle write there.
- **`start` and `stop` report JSON results,** one per name in order, each with the tunnel's report as it stands after the action, so the panel redraws without running `portico tunnels` again. A per-name failure is a result, not an error document; the document is an error document only when the command failed before acting on any name (no name given, the tunnels file unreadable).
- **Serialization is a rendering of the existing report types,** not a parallel model: `HostInventory`, `HostDetails` (with `HostIdentities`), `IdentityInventory`, `ContainerList`, `TunnelReport`, `StartOutcome`, `StopOutcome`, `ContainerError` and `AppError`, built by the same `App` functions the human output uses. One module, `src/cli/json.rs`, maps each type to a `serde_json::Value`, the way `print_details` and its siblings map them to text. The domain types gain no derive and no serde attribute, so the field names of the contract are spelled out in one file, next to each other, and a rename in the code cannot change the contract by accident.
- **The dependency is `serde_json`** (MIT OR Apache-2.0), with its default features. `serde`, `itoa`, `ryu` and `memchr` are already in `Cargo.lock` as dev-dependency transitives, so the build graph grows by about one crate. Hand-written JSON would need its own escaping of control characters, lone surrogates in lossy text and non-UTF-8 paths, which is exactly where strict consumer parsers fail; `serde_json` is the standard answer. `cargo deny` and `cargo audit` check it in CI, which runs them because `Cargo.toml` changes. No `derive` feature is needed, since nothing derives.
- **The contract is documented in `docs/json-output.md`,** a consumer page with each document, a field table and the versioning rules. Its examples are the golden files of the tests, copied verbatim, and a doc guard checks that they stay identical (see "Testing").

## Decisions

### Conventions

- **Field names** are `snake_case`. **Enumerated values** are lowercase with hyphens (`port-in-use`, `in-agent`), the form `TunnelStatus::name()` already uses.
- **Every key documented for an object is always present.** A value that is unknown or does not apply is `null` (a scalar or an object) or `[]` (a list), never an absent key, so a consumer never needs `has`. Where an object has variants (a tunnel's status, an agent's state, an error's kind), the keys documented for that variant are always present and the others are absent.
- **Paths** are absolute strings, as Portico resolved them, not shortened with `~` (that is display). A path that is not valid UTF-8 is converted lossily (`U+FFFD`); it is still shown, as the human output shows it. Values Portico prints as OpenSSH printed them (an IdentityFile as `ssh -G` wrote it, a `ProxyJump` list) are strings as written.
- **Times** are Unix seconds as integers, as `TunnelStatus` holds them. The panel formats them; Portico's `HH:MM` display is not part of the contract.
- **Warnings** are a list of objects with a `message` string, the human line without its `Warning:` prefix. They are objects so a later version can add a `kind` without breaking anyone.
- **Error objects** (top level or inside a tunnel result) are:

  ```json
  {
    "kind": "unknown-host",
    "message": "unknown host: prod",
    "next": []
  }
  ```

  `message` is the human `error:` line without the prefix, or an explanation's headline; `next` the explanation's next steps, in order (empty when there are none). `kind` is one of:

  | Kind | When |
  |---|---|
  | `usage` | A required name is missing (the human output would have opened a picker) |
  | `unknown-host` | The alias is not in the inventory |
  | `unknown-tunnel` | No saved (or recorded) tunnel has that name |
  | `ssh` | ssh could not reach or authenticate to the host (listing containers, starting a tunnel) |
  | `docker` | docker failed on the target; adds `"problem"`: `not-installed`, `daemon-down`, `permission-denied` or `other` |
  | `other` | Anything else: an unreadable config, a refused start (`port 5432 on 127.0.0.1 is in use by tunnel db`), a runtime directory error |

  The kind is for choosing an icon or a hint, not for parsing the message. New kinds may be added without a schema change; a consumer treats an unknown kind as `other`.

- **An error document** is `{"schema": 1, "error": {...}}`, printed with exit status 1. A document that has `error` has nothing else; a document without it is the command's success shape.

### `portico hosts --json`

Runs no `ssh`, as today.

```json
{
  "hosts": [
    {
      "alias": "bastion",
      "ownership": "external",
      "source": "/home/me/.ssh/config"
    },
    {
      "alias": "prod-db",
      "ownership": "managed",
      "source": "/home/me/.ssh/portico/config"
    }
  ],
  "schema": 1,
  "warnings": []
}
```

| Field | Type | Meaning |
|---|---|---|
| `hosts[]` | list | Every host of the inventory, ordered case-insensitively by alias, as the human list |
| `.alias` | string | What `ssh <alias>` and every Portico command accept |
| `.ownership` | `managed` \| `external` | Whether Portico manages the block |
| `.source` | path | The file that declares it |
| `warnings[]` | list | Discovery warnings (an unreadable include, a parse error, a duplicate alias) |

The panel's list shows aliases. Each host's hostname and user would need one `ssh -G` per host, which `hosts` never runs; the panel asks `show` for the host it opens.

### `portico show <alias> --json`

```json
{
  "host": {
    "alias": "prod-db",
    "declared": {
      "hostname": "db.example.com",
      "identity_file": "~/.ssh/id_ed25519",
      "port": null,
      "proxy_jump": "bastion",
      "user": "deploy"
    },
    "identities": {
      "agent": {
        "key_count": 2,
        "provider": "1password",
        "socket": "/home/me/.1password/agent.sock",
        "state": "listed"
      },
      "also_offered": [
        {
          "bits": 256,
          "comment": "work laptop",
          "fingerprint": "SHA256:Zm9vYmFyYmF6cXV4cXV1eGNvcmdlZ3JhdWx0Z2FycGx5",
          "type": "ED25519"
        }
      ],
      "defaults": false,
      "identities": [
        {
          "availability": "in-agent",
          "configured": "/home/me/.ssh/id_ed25519",
          "path": "/home/me/.ssh/id_ed25519",
          "public_key": {
            "bits": 256,
            "comment": "deploy@prod",
            "fingerprint": "SHA256:YmF6cXV4Zm9vYmFycXV1eGNvcmdlZ3JhdWx0Z2FycGx5",
            "type": "ED25519"
          },
          "reason": null
        }
      ],
      "identities_only": false,
      "pkcs11_provider": null
    },
    "ownership": "managed",
    "resolution_error": null,
    "resolved": {
      "hostname": "db.example.com",
      "identity_files": ["/home/me/.ssh/id_ed25519"],
      "port": 22,
      "proxy_jump": "bastion",
      "user": "deploy"
    },
    "source": "/home/me/.ssh/portico/config"
  },
  "schema": 1,
  "warnings": []
}
```

- `declared` is the managed host's own fields (`hostname`, `user`, `port`, `identity_file`, `proxy_jump`), `null` for an external host, as the human `Declared …` lines.
- `resolved` is what `ssh -G` says, the same five fields the human `Resolved …` lines print (`identity_files` is the list). When resolution fails, `resolved` and `identities` are `null` and `resolution_error` is ssh's message; the command still succeeds, as today.
- `identities` renders `HostIdentities`:
  - `agent.state` is `listed` (`provider`, `socket`, `key_count`), `unavailable` (`provider`, `socket`, `reason`: the human reason text), `disabled` (`IdentityAgent none`, no further keys) or `not-configured` (`variable`: the unset variable's name). `provider` is `1password`, `gpg-agent`, `gnome-keyring` or `generic`, guessed from the socket path for display as today. `show` gives the agent's key count, as the human output does; `identities` lists the keys.
  - `identities[]` is each IdentityFile in OpenSSH's order: `configured` (as `ssh -G` printed it), `path` (expanded, `null` when a token could not be expanded), `public_key` (from the `.pub` file, or `null`), `availability` (`in-agent`, `key-file`, `missing`, `unchecked`) and `reason` (the human reason, `null` when the word says it all).
  - `defaults` says the list is OpenSSH's built-in one, `identities_only` mirrors `IdentitiesOnly`, `also_offered[]` lists the other keys the agent offers (empty when `IdentitiesOnly` is set or the agent was not listed, which is when the human output omits the line), and `pkcs11_provider` is the configured provider or `null`.
  - A public key object is `type`, `bits`, `fingerprint` (SHA-256, as `ssh-add` printed it) and `comment`.

`resolved` leaves out what the human output does not show: `ProxyCommand` (a command line can hold a token), `ProxyUseFdpass`, the known hosts files and `IdentityAgent`'s raw value.

### `portico identities --json`

```json
{
  "agents": [
    {
      "environment": true,
      "keys": [
        {
          "bits": 256,
          "comment": "work laptop",
          "fingerprint": "SHA256:Zm9vYmFyYmF6cXV4cXV1eGNvcmdlZ3JhdWx0Z2FycGx5",
          "type": "ED25519"
        }
      ],
      "provider": "1password",
      "socket": "/home/me/.1password/agent.sock",
      "state": "listed",
      "used_by": ["prod-db"]
    }
  ],
  "files": [
    {
      "availability": "missing",
      "configured": "/home/me/.ssh/old_rsa",
      "path": "/home/me/.ssh/old_rsa",
      "public_key": null,
      "reason": "no such file",
      "used_by": ["legacy"]
    }
  ],
  "schema": 1,
  "warnings": []
}
```

Agents are those the human output lists (with a socket: `listed` or `unavailable`, the latter with `reason` and `keys: []`), with `environment` (it is `SSH_AUTH_SOCK`'s) and `used_by` (the hosts that use it). Files are every known identity file, with the fields of a host identity plus `used_by`.

### `portico containers [--on <alias>] --json`

```json
{
  "containers": [
    {
      "display_name": "shop/web",
      "id": "4f1c2a9b7d3e",
      "image": "nginx:1.27",
      "name": "shop-web-1",
      "project": "shop",
      "service": "web",
      "status": "Up 3 hours"
    }
  ],
  "note": null,
  "schema": 1,
  "skipped": 0,
  "target": {
    "alias": "prod-db",
    "kind": "host"
  },
  "via": null
}
```

| Field | Meaning |
|---|---|
| `target` | `{"kind": "local", "alias": null}` for this machine, `{"kind": "host", "alias": …}` for `--on` |
| `containers[]` | The running containers, in the human output's order (grouped by Compose project, then the others) |
| `.id` | Docker's short ID: what `portico shell <id> --on <alias>` accepts, so the panel needs no name resolution |
| `.name`, `.image`, `.status` | Docker's own, as the human columns |
| `.display_name` | The name the human output shows first (`project/service`, numbered when a service has several) |
| `.project`, `.service` | The Compose labels, or `null` |
| `via` | `sg` when docker had to run inside the docker group, otherwise `null` |
| `note` | The human note said once when `via` is set, or `null` |
| `skipped` | Lines of `docker ps` that could not be parsed |

Left out: the container's command, mounts, ports, networks and working directory. The TUI shows them on demand, but the command line in particular can hold secrets passed as arguments, and the panel does not need any of them. An empty list is a success with `containers: []`.

A docker failure is an error document of kind `docker`, with `problem` and the issue's headline and next steps. When the human output would offer a fix (join the docker group, start the service), JSON mode does not run it, and `next` ends with the command to run in a terminal (`portico containers --on prod-db`) to be offered it. ssh's own failure to reach the host is kind `ssh`, with the explanation's headline and next steps.

### `portico tunnels --json` and `portico tunnel show <name> --json`

Both render `TunnelReport`, built by the function the human list and the TUI use. No `ssh` runs, as today.

```json
{
  "schema": 1,
  "tunnels": [
    {
      "forward": {
        "bind_address": "127.0.0.1",
        "bind_port": 5432,
        "description": "127.0.0.1:5432 → localhost:5432",
        "kind": "local",
        "spec": "127.0.0.1:5432:localhost:5432",
        "target_host": "localhost",
        "target_port": 5432
      },
      "host": "prod-db",
      "name": "db",
      "status": {
        "pid": 41237,
        "started": 1781860921,
        "state": "running"
      },
      "warnings": []
    }
  ]
}
```

- `forward.kind` is `local`, `remote` or `dynamic`; `bind_address` is `null` when ssh's default applies; `target_host` and `target_port` are `null` for a dynamic forward. `spec` is the forward in ssh's own syntax (the argument of `-L`, `-R` or `-D`), and `description` the human column.
- `status` renders `TunnelStatus` under the names the tunnels design fixed for this purpose:

  | `state` | Further keys |
  |---|---|
  | `stopped` | none |
  | `running` | `pid`, `started` |
  | `failed` | `reason` (one sanitized line), `finished` (or `null` when unknown) |
  | `port-in-use` | `address` (`127.0.0.1:5432`), `by` (the saved tunnel holding it, or `null`) |

- `warnings[]` adds a `kind` to the message, since the model has one: `unknown-host` (with `host`) or `collides-with` (with `tunnel`).

`portico tunnel show <name> --json` is `{"schema": 1, "tunnel": <report>, "command": [...], "log": [...]}`: the exact argument list `start` runs, and for a failed tunnel the last 20 meaningful lines of its log (otherwise `[]`), sanitized as the human output's are (terminal escapes and control characters removed). The report itself is the same object as in the list, so a consumer has one parser for both.

### `portico tunnel start|stop --json`

```json
{
  "results": [
    {
      "error": null,
      "name": "db",
      "outcome": "started",
      "pid": 41237,
      "tunnel": { "name": "db", "status": { "state": "running", "pid": 41237, "started": 1781860921 }, "...": "the full report" }
    },
    {
      "error": {
        "kind": "other",
        "message": "port 8080 on 127.0.0.1 is in use",
        "next": []
      },
      "name": "preview",
      "outcome": "failed",
      "pid": null,
      "tunnel": { "name": "preview", "status": { "state": "port-in-use", "address": "127.0.0.1:8080", "by": null }, "...": "the full report" }
    }
  ],
  "schema": 1
}
```

(In the real document and in `docs/json-output.md` the reports are complete; they are abbreviated here.)

- One result per name, in the order given; `stop --all` has one per running tunnel (and `results: []` when none runs, exit 0).
- `outcome` for `start`: `started`, `already-running` or `failed`; for `stop`: `stopped`, `not-running` or `failed`. `pid` is set for `started` and `already-running`, otherwise `null`; `error` is set exactly when the outcome is `failed`.
- A failed start uses the start's reason as `message` and its explanation's next steps as `next` (kind `ssh` when the explanation is a connection failure, `other` otherwise); a refusal (unknown tunnel, unknown host, port in use, a running collision) uses the refusal's kind and text.
- `tunnel` is the report after the action, or `null` when the name is not a saved tunnel (an unknown name, or a stopped tunnel that was no longer saved).
- The exit status is 1 when any result failed, as today.

### Versioning

The contract is `schema` 1. Without changing the number, a later Portico may:

- add keys to any object (a consumer ignores keys it does not know);
- add values to `error.kind`, to a warning's `kind` and to `problem` (a consumer treats an unknown value as `other`);
- add commands that accept `--json`.

Anything else is breaking and raises `schema` to 2: removing or renaming a key, changing a value's type or meaning, turning a nullable value into an absent key, or adding a value to `status.state`, `outcome`, `availability`, `agent.state`, `ownership` or `forward.kind` (the closed sets a consumer switches on). A breaking change is a `feat!:` commit, and its release notes say so. Portico prints only its current schema; there is no flag to ask for an older one. A consumer checks `schema` first and says "this Portico is too new (or too old) for this panel" rather than misreading a document; with additive changes kept within one schema, that should be rare.

### What is never printed

The rule is: nothing the human output does not show, and no credential at all. Concretely:

- No key material: private keys are only ever `stat`ed (identity awareness design), and public keys appear as type, size, fingerprint and comment, as `portico identities` prints them.
- No environment: no variable's value, except the agent socket path that `SSH_AUTH_SOCK` names, which the human output also shows; for an unset variable only its name.
- No command lines that Portico did not build: no `ProxyCommand`, no container command. The one command line printed is the tunnel's own `ssh` invocation, built from saved values.
- From logs and ssh's stderr only what the human output shows, sanitized the same way: a tunnel's one-line reason, `show`'s log tail, and an explanation's headline and next steps.

A test pins this (see "Testing").

### Code layout

- **`src/cli/json.rs` (new):** one function per report type returning a `serde_json::Value`, the error mapping from `AppError`, `ContainerError`, `TunnelRefusal` and the start outcome to the error object, and `print(output, value)` that adds `schema` and writes the document. Unit-tested on its own.
- **`src/cli.rs`:** a `json: bool` flag on `Hosts`, `Show`, `Identities` and `Containers`; in JSON mode the command checks for a missing name before anything else, calls the same `App` function, and prints the document instead of the text. `containers` calls `app.containers(target, true, None)` directly instead of `list_with_offers`. `run` turns any error into an error document when the command was in JSON mode, instead of printing `error:` to stderr.
- **`src/cli/tunnels.rs`:** the same flag on `tunnels`, `tunnel show`, `tunnel start` and `tunnel stop`, and the result list for the two actions.
- **`Cargo.toml`:** `serde_json = "1"`.
- **`src/tui.rs`, `src/app*`, the domain modules:** unchanged. If a value the contract needs is not public today, it is exposed with the smallest change (a `pub` getter), not computed again.
- **`docs/json-output.md` (new):** the consumer page.
- **`README.md`:** a short "Machine-readable output" section listing the commands and linking the consumer page.
- **Docs in the same pull request:** this spec becomes "Approved; implemented", item 1 leaves `ROADMAP.md` v3 (the desktop surface becomes item 1, the rest renumbered), and the v1 design's "Delivered follow-up slices" gains a "Machine-readable output" bullet.

## Out of scope

- `--json` for management commands, `connect`, `shell` and `test`.
- A streaming or watch mode (`--follow`): the panel polls on open, and nothing in Portico stays running to stream from.
- Hostnames and users in the host list, the containers' ports, mounts and command, and tunnel logs in the list: not needed by the panel, and each can be added later as a new key without a schema change.
- A JSON Schema file: the consumer page and the golden files are the specification, and the only consumer is in this repository. One can be added if outside consumers ask.
- Machine-readable output from the TUI.

## Testing

- **Unit (`src/cli/json.rs`):** each `TunnelStatus` variant and its keys; each agent state; each availability; a dynamic forward's null targets; `declared` for an external host; an error object for each kind, including docker's `problem`; a non-UTF-8 path rendered lossily; control characters in a reason escaped (`\u001b`) rather than emitted.
- **Golden files (`tests/fixtures/json/*.json`), from CLI tests with the existing fake `ssh`, `ssh-add`, `ssh-keygen` and `docker`:** `hosts` (with a warning), `show` of a managed host with identities, `show` of an external host whose resolution fails, `identities`, `containers` on a host and on this machine, a docker failure with an offer, `tunnels` with a stopped, a failed and a port-in-use tunnel, `tunnel show` of a failed tunnel, `start` with a started and a refused name, `stop --all` with nothing running, and error documents for a missing name and an unknown host. The temporary home is replaced by `/home/me` before comparing, so the files are stable; `PORTICO_BLESS=1` rewrites them. Each test also checks the exit status and that stderr is empty.
- **Doc guard (`tests/docs.rs`):** every golden file appears verbatim in `docs/json-output.md`, in a fenced block that follows a `<!-- golden: <file> -->` marker, and every marker names an existing golden file. The documented examples therefore cannot drift from what Portico prints.
- **Never interactive:** with `interactive` set to true and a picker that fails the test when asked, every `--json` command without a name gives the `usage` error document; `containers --on <alias> --json` passes `BatchMode=yes` to the fake `ssh`; a docker failure with an offer runs no `sudo`.
- **Nothing sensitive:** a fixture whose `ProxyCommand` holds `TOKEN=s3cret`, whose container command holds `--password=s3cret`, and whose environment sets `PORTICO_SECRET=s3cret`, produces no `s3cret` in any document; and no golden file contains `PRIVATE KEY`.
- **End to end (`tests/e2e/json.rs`, real `ssh` against the throwaway `sshd`):** `show --json` resolves the test host; `tunnel start db --json` reports `started` with the report's status `running` and the same PID that `tunnels --json` then reports; `tunnel stop db --json` reports `stopped` and the report `stopped`; a start on a busy port reports `failed` with the report's status `port-in-use` and exit 1. Each document is parsed with `serde_json` and checked for `schema: 1`.

## Decided in review

1. **The contract is public.** A separately installed panel (the Omarchy plugin) may run against another Portico release, so the schema number and the versioning rules above are a promise to any consumer, and `docs/json-output.md` says so.
2. **`show` may wake an agent.** Listing a host's agent can make a locked 1Password show its unlock prompt. That is fine for an explicit request, so the consumer page says to run `show` only when the user opens a host, never to prefetch every host. `hosts --json` stays free of `ssh`, `ssh-add` and `ssh-keygen`, pinned by a test whose fake programs fail when run.
3. **Sorted keys are guarded.** `serde_json`'s `Map` is a `BTreeMap` unless its `preserve_order` feature is on, and the golden files rely on sorted keys, so nothing enables that feature and a unit test fails if feature unification ever does.
4. **A failure before any output is still a document.** An unreadable or unparsable configuration, or any other error before the command has anything to print, is an error document on stdout with exit status 1, never text on stderr. Pinned by a test.
5. **Start and stop block.** `start` returns when ssh has established the tunnel or given up, which can take as long as the host's `ConnectTimeout`, and `stop` can wait up to 3 seconds for ssh to exit. The consumer page says to run both asynchronously; the document arrives when they finish.
