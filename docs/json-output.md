# Machine-readable output

Portico's read commands, and starting and stopping tunnels, print a JSON document when given `--json`. This page is the contract for programs that read it, such as a desktop panel: what each command prints, what every field means, how errors look, and what may change between releases. Every example below is printed by Portico's own test suite, so it is exactly what Portico produces (with the home directory shown as `/home/me`).

## Commands

| Command | Document |
|---|---|
| `portico hosts --json` | [The host list](#hosts) |
| `portico show <alias> --json` | [One host's details and identities](#show) |
| `portico identities --json` | [Every agent and identity file](#identities) |
| `portico containers [--on <alias>] --json` | [The running containers of this machine or a host](#containers) |
| `portico tunnels --json` | [The saved tunnels and their status](#tunnels) |
| `portico tunnel show <name> --json` | [One tunnel, its command line and its log](#tunnel-show) |
| `portico tunnel start <name>... --json` | [What each start did](#start-and-stop) |
| `portico tunnel stop <name>... --json`, `portico tunnel stop --all --json` | [What each stop did](#start-and-stop) |

Other commands do not accept `--json`. To connect, a consumer runs `portico connect <alias> --new-terminal`, and to open a container's shell `portico shell <id> --on <alias> --new-terminal` (without `--on` for this machine); both report through their exit status only.

## Rules every document follows

- **One document per run, on stdout,** whether the command succeeds or fails: one JSON object, UTF-8, pretty-printed, ending with a newline. Read stdout to its end and parse it once. Whitespace and key order are not part of the contract.
- **`"schema": 1`** is in every document, error documents included. Check it before reading anything else (see [Versioning](#versioning)).
- **Nothing on stderr.** Portico writes nothing there in JSON mode; explanations and warnings are in the document.
- **Never interactive.** No picker, no question and no password prompt, even when run from a terminal: a missing name is a `usage` error, and the ssh that lists a host's containers runs with `BatchMode=yes`.
- **Exit status:** 0 when the command did what was asked, 1 when it failed (including when any tunnel of a `start` or `stop` failed). Exit status 2 with text on stderr is an argument error (an unknown flag, `--json` on a command that does not take it): a bug in the consumer, not a condition to handle.
- **Every documented key is always present.** A value that is unknown or does not apply is `null`, or `[]` for a list. Where an object has variants (a tunnel's `status.state`, an agent's `state`, an error's `kind`), the keys listed for that variant are present and the others are absent.
- **Names** are `snake_case`; enumerated values are lowercase with hyphens (`port-in-use`).
- **Paths** are absolute (never shortened with `~`), except where a value is shown as OpenSSH or the user wrote it (`configured`, `identity_file`, `identity_files`). A path that is not valid UTF-8 is converted lossily.
- **Times** are Unix seconds.
- **Warnings** are objects with a `message`, the line the human output prints after `Warning:`.
- **Free text is one line.** Messages, next steps, reasons, warnings, key comments and docker's names, images and statuses never contain a newline, a terminal escape sequence or a control character, so any of them can go in a label as it is. Text from ssh, an agent or docker is cleaned the same way.

## Errors

A command that fails prints an error document and exits with status 1:

<!-- golden: error-unknown-host.json -->
```json
{
  "error": {
    "kind": "unknown-host",
    "message": "unknown host: nope",
    "next": []
  },
  "schema": 1
}
```

`message` is one line for people, and `next` the steps that may fix it, in order. When ssh fails in a way Portico has no explanation for, `message` ends with ssh's own last line (`ssh to prod failed: kex_exchange_identification: read: Connection reset`). `kind` says what failed:

| `kind` | When |
|---|---|
| `usage` | A required name is missing |
| `unknown-host` | The alias is not in the inventory |
| `unknown-tunnel` | No tunnel has that name |
| `ssh` | ssh could not reach or authenticate to the host, or did not establish a tunnel |
| `docker` | docker failed; `problem` is `not-installed`, `daemon-down`, `permission-denied` or `other` |
| `other` | Anything else, such as an unreadable `~/.ssh/config` or a busy port |

Show `message`; use `kind` to choose an icon or a hint, never parse `message`. A missing name looks like this:

<!-- golden: error-usage.json -->
```json
{
  "error": {
    "kind": "usage",
    "message": "give the name: portico show <alias> --json",
    "next": []
  },
  "schema": 1
}
```

## Hosts

`portico hosts --json` lists every host of the inventory, ordered by alias as `portico hosts` is. It reads the configuration files only: it runs no `ssh` and asks no agent, so it is cheap to run whenever the list is shown.

<!-- golden: hosts.json -->
```json
{
  "hosts": [
    {
      "alias": "bastion",
      "ownership": "external",
      "source": "/home/me/.ssh/config"
    },
    {
      "alias": "bastion",
      "ownership": "external",
      "source": "/home/me/.ssh/config"
    },
    {
      "alias": "legacy",
      "ownership": "external",
      "source": "/home/me/.ssh/config"
    },
    {
      "alias": "prod",
      "ownership": "managed",
      "source": "/home/me/.ssh/portico/config"
    }
  ],
  "schema": 1,
  "warnings": [
    {
      "message": "duplicate alias bastion: /home/me/.ssh/config, /home/me/.ssh/config"
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `hosts[].alias` | The name `ssh <alias>` and every Portico command accept |
| `hosts[].ownership` | `managed` (Portico manages the block) or `external` |
| `hosts[].source` | The file that declares it |
| `warnings[]` | Problems found while reading the configuration, such as a duplicate alias |

## Show

`portico show <alias> --json` gives one host's declared and resolved configuration and its identities.

<!-- golden: show.json -->
```json
{
  "host": {
    "alias": "prod",
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
        },
        {
          "availability": "missing",
          "configured": "/home/me/.ssh/old_rsa",
          "path": "/home/me/.ssh/old_rsa",
          "public_key": null,
          "reason": "no such file"
        }
      ],
      "identities_only": false,
      "pkcs11_provider": null
    },
    "ownership": "managed",
    "resolution_error": null,
    "resolved": {
      "hostname": "db.example.com",
      "identity_files": [
        "/home/me/.ssh/id_ed25519",
        "/home/me/.ssh/old_rsa"
      ],
      "port": 22,
      "proxy_jump": "bastion",
      "user": "deploy"
    },
    "source": "/home/me/.ssh/portico/config"
  },
  "schema": 1,
  "warnings": [
    {
      "message": "duplicate alias bastion: /home/me/.ssh/config, /home/me/.ssh/config"
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `host.alias`, `host.ownership`, `host.source` | As in the host list |
| `host.declared` | A managed host's own fields (`hostname`, `user`, `port`, `identity_file`, `proxy_jump`), or `null` for an external host |
| `host.resolved` | What OpenSSH resolves (`ssh -G`): `hostname`, `user`, `port`, `identity_files` (a list) and `proxy_jump`; `null` when resolution failed |
| `host.resolution_error` | OpenSSH's message when resolution failed, otherwise `null` |
| `host.identities` | The identities OpenSSH would offer, below; `null` when resolution failed |
| `warnings[]` | As in the host list |

`host.identities`:

| Field | Meaning |
|---|---|
| `agent.state` | `listed` (with `provider`, `socket`, `key_count`), `unavailable` (with `provider`, `socket`, `reason`), `disabled` (`IdentityAgent none`) or `not-configured` (with `variable`, the unset environment variable) |
| `agent.provider` | `1password`, `gpg-agent`, `gnome-keyring` or `generic`, guessed from the socket path |
| `identities[]` | Each identity file in OpenSSH's order: `configured` (as `ssh -G` printed it), `path` (expanded, or `null`), `public_key` (or `null`), `availability` and `reason` |
| `identities[].availability` | `in-agent`, `key-file` (the private key file exists), `missing` or `unchecked` (the path uses a token Portico does not expand) |
| `identities[].reason` | Why, when the availability alone does not say; otherwise `null` |
| `defaults` | `true` when the list is OpenSSH's built-in default list |
| `identities_only` | OpenSSH's `IdentitiesOnly` |
| `also_offered[]` | The agent's other keys, which ssh also offers; empty with `IdentitiesOnly` or when the agent was not listed |
| `pkcs11_provider` | The configured PKCS#11 provider, or `null` |

A public key is `type`, `bits`, `fingerprint` (SHA-256) and `comment`. Portico never reads a private key; these come from the agent's list and from `.pub` files.

When the host does not resolve, the command still succeeds:

<!-- golden: show-unresolved.json -->
```json
{
  "host": {
    "alias": "legacy",
    "declared": null,
    "identities": null,
    "ownership": "external",
    "resolution_error": "ssh exited with exit status: 255: legacy: line 9: Bad configuration option: frobnicate",
    "resolved": null,
    "source": "/home/me/.ssh/config"
  },
  "schema": 1,
  "warnings": [
    {
      "message": "duplicate alias bastion: /home/me/.ssh/config, /home/me/.ssh/config"
    }
  ]
}
```

**Run `show` only when the user opens a host,** never to prefetch every host: it asks the host's agent for its keys, which can make a locked agent (1Password, for example) show its unlock prompt.

## Identities

`portico identities --json` lists the agents with a socket and their keys, and every identity file the inventory references or `~/.ssh` holds. Like `show`, it asks the agents for their keys.

<!-- golden: identities.json -->
```json
{
  "agents": [
    {
      "environment": false,
      "keys": [
        {
          "bits": 256,
          "comment": "deploy@prod",
          "fingerprint": "SHA256:YmF6cXV4Zm9vYmFycXV1eGNvcmdlZ3JhdWx0Z2FycGx5",
          "type": "ED25519"
        },
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
      "used_by": [
        "prod"
      ]
    }
  ],
  "files": [
    {
      "availability": "in-agent",
      "configured": "~/.ssh/id_ed25519",
      "path": "/home/me/.ssh/id_ed25519",
      "public_key": {
        "bits": 256,
        "comment": "deploy@prod",
        "fingerprint": "SHA256:YmF6cXV4Zm9vYmFycXV1eGNvcmdlZ3JhdWx0Z2FycGx5",
        "type": "ED25519"
      },
      "reason": null,
      "used_by": [
        "prod"
      ]
    },
    {
      "availability": "missing",
      "configured": "~/.ssh/old_rsa",
      "path": "/home/me/.ssh/old_rsa",
      "public_key": null,
      "reason": "no such file",
      "used_by": [
        "prod"
      ]
    }
  ],
  "schema": 1,
  "warnings": [
    {
      "message": "legacy: could not resolve: ssh exited with exit status: 255: legacy: line 9: Bad configuration option: frobnicate"
    }
  ]
}
```

`agents[]` have the fields of a host's agent, with `keys[]` (the public keys; empty when `unavailable`) instead of `key_count`, plus `environment` (it is the agent `SSH_AUTH_SOCK` names) and `used_by` (the hosts that use it). `files[]` have the fields of a host's identity plus `used_by`.

## Containers

`portico containers --json` lists the running Docker containers of this machine, and `portico containers --on <alias> --json` those of a host, through `ssh`.

<!-- golden: containers-on-host.json -->
```json
{
  "containers": [
    {
      "display_name": "web",
      "id": "4f1c2a9b7d3e",
      "image": "nginx:1.27",
      "name": "shop-web-1",
      "project": "shop",
      "service": "web",
      "status": "Up 3 hours"
    },
    {
      "display_name": "cache",
      "id": "9a8b7c6d5e4f",
      "image": "redis:7",
      "name": "cache",
      "project": null,
      "service": null,
      "status": "Up 5 minutes"
    }
  ],
  "note": null,
  "schema": 1,
  "skipped": 0,
  "target": {
    "alias": "prod",
    "kind": "host"
  },
  "via": null
}
```

| Field | Meaning |
|---|---|
| `target` | `{"kind": "local", "alias": null}` for this machine, `{"kind": "host", "alias": …}` for a host |
| `containers[]` | In the order `portico containers` shows them: grouped by Compose project, then the rest |
| `containers[].id` | What `portico shell <id> [--on <alias>]` opens |
| `containers[].display_name` | The name Portico shows first: the Compose service (numbered when it has several containers), or the container's name |
| `containers[].name`, `.image`, `.status` | Docker's own |
| `containers[].project`, `.service` | The Compose project and service, or `null` |
| `via` | `sg` when docker had to run inside the docker group, otherwise `null` |
| `note` | A remark about `via`, or `null` |
| `skipped` | Lines of docker's output Portico could not read |

On this machine, `target` has no alias:

<!-- golden: containers.json -->
```json
{
  "containers": [
    {
      "display_name": "web",
      "id": "4f1c2a9b7d3e",
      "image": "nginx:1.27",
      "name": "shop-web-1",
      "project": "shop",
      "service": "web",
      "status": "Up 3 hours"
    },
    {
      "display_name": "cache",
      "id": "9a8b7c6d5e4f",
      "image": "redis:7",
      "name": "cache",
      "project": null,
      "service": null,
      "status": "Up 5 minutes"
    }
  ],
  "note": null,
  "schema": 1,
  "skipped": 0,
  "target": {
    "alias": null,
    "kind": "local"
  },
  "via": null
}
```

Containers' commands, mounts, ports and networks are not included. When docker fails, the error says why; if `portico containers` would offer a fix (start the service, join the docker group), JSON mode never runs it, and the last step names the command to run in a terminal:

<!-- golden: containers-docker-down.json -->
```json
{
  "error": {
    "kind": "docker",
    "message": "The Docker daemon is not running on this machine.",
    "next": [
      "Start it with: sudo systemctl start docker",
      "Run portico containers in a terminal to be offered this fix."
    ],
    "problem": "daemon-down"
  },
  "schema": 1
}
```

## Tunnels

`portico tunnels --json` lists the saved tunnels and their status. It runs no `ssh`.

<!-- golden: tunnels.json -->
```json
{
  "schema": 1,
  "tunnels": [
    {
      "forward": {
        "bind_address": "127.0.0.1",
        "bind_port": 5433,
        "description": "127.0.0.1:5433 → localhost:80",
        "kind": "local",
        "spec": "127.0.0.1:5433:localhost:80",
        "target_host": "localhost",
        "target_port": 80
      },
      "host": "prod",
      "name": "busy",
      "status": {
        "address": "127.0.0.1:5433",
        "by": null,
        "state": "port-in-use"
      },
      "warnings": []
    },
    {
      "forward": {
        "bind_address": "127.0.0.1",
        "bind_port": 1,
        "description": "127.0.0.1:1 → localhost:5432",
        "kind": "local",
        "spec": "127.0.0.1:1:localhost:5432",
        "target_host": "localhost",
        "target_port": 5432
      },
      "host": "prod",
      "name": "db",
      "status": {
        "state": "stopped"
      },
      "warnings": [
        {
          "kind": "collides-with",
          "message": "uses the same port as tunnel socks; only one of them can run at a time",
          "tunnel": "socks"
        }
      ]
    },
    {
      "forward": {
        "bind_address": null,
        "bind_port": 8080,
        "description": "remote 8080 → localhost:3000",
        "kind": "remote",
        "spec": "8080:localhost:3000",
        "target_host": "localhost",
        "target_port": 3000
      },
      "host": "prod",
      "name": "preview",
      "status": {
        "finished": 1781861043,
        "reason": "Error: remote port forwarding failed for listen port 8080",
        "state": "failed"
      },
      "warnings": []
    },
    {
      "forward": {
        "bind_address": "127.0.0.1",
        "bind_port": 1,
        "description": "socks 127.0.0.1:1",
        "kind": "dynamic",
        "spec": "127.0.0.1:1",
        "target_host": null,
        "target_port": null
      },
      "host": "prod",
      "name": "socks",
      "status": {
        "state": "stopped"
      },
      "warnings": [
        {
          "kind": "collides-with",
          "message": "uses the same port as tunnel db; only one of them can run at a time",
          "tunnel": "db"
        }
      ]
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `name`, `host` | The tunnel's name and the alias it goes through |
| `forward.kind` | `local`, `remote` or `dynamic` |
| `forward.bind_address` | The listen address, or `null` for ssh's default (loopback) |
| `forward.bind_port` | The listen port |
| `forward.target_host`, `forward.target_port` | Where the forward connects, or `null` for a dynamic forward |
| `forward.spec` | The forward in ssh's syntax (the argument of `-L`, `-R` or `-D`) |
| `forward.description` | The forward as Portico shows it |
| `status` | Below |
| `warnings[]` | `kind` `unknown-host` (with `host`: the alias is no longer known) or `collides-with` (with `tunnel`: another saved tunnel uses the same port), and `message` |

| `status.state` | Further keys |
|---|---|
| `stopped` | none |
| `running` | `pid`, `started` |
| `failed` | `reason` (one line), `finished` (or `null`) |
| `port-in-use` | `address` (the busy address and port), `by` (the saved tunnel holding it, or `null`) |

## Tunnel show

`portico tunnel show <name> --json` gives one tunnel's report (the same object as in the list), the exact command line `start` runs, and for a failed tunnel the end of ssh's log (otherwise `[]`).

<!-- golden: tunnel-show.json -->
```json
{
  "command": [
    "ssh",
    "-f",
    "-N",
    "-o",
    "ExitOnForwardFailure=yes",
    "-o",
    "BatchMode=yes",
    "-o",
    "ControlMaster=no",
    "-o",
    "ControlPath=none",
    "-o",
    "RemoteCommand=none",
    "-E",
    "/home/me/.ssh/portico/run/tunnels/preview.log",
    "-R",
    "8080:localhost:3000",
    "--",
    "prod"
  ],
  "log": [
    "Warning: remote port forwarding failed for listen port 8080",
    "Error: remote port forwarding failed for listen port 8080"
  ],
  "schema": 1,
  "tunnel": {
    "forward": {
      "bind_address": null,
      "bind_port": 8080,
      "description": "remote 8080 → localhost:3000",
      "kind": "remote",
      "spec": "8080:localhost:3000",
      "target_host": "localhost",
      "target_port": 3000
    },
    "host": "prod",
    "name": "preview",
    "status": {
      "finished": 1781861043,
      "reason": "Error: remote port forwarding failed for listen port 8080",
      "state": "failed"
    },
    "warnings": []
  }
}
```

## Start and stop

`portico tunnel start <name>... --json` and `portico tunnel stop <name>... --json` report one result per name, in order, each with the tunnel's report as it stands afterwards, so the consumer can redraw without listing again.

<!-- golden: tunnel-start.json -->
```json
{
  "results": [
    {
      "error": {
        "kind": "ssh",
        "message": "the server refused the remote forward on port 8080",
        "next": [
          "the port is in use on the server, or its sshd does not allow remote forwards"
        ]
      },
      "name": "preview",
      "outcome": "failed",
      "pid": null,
      "tunnel": {
        "forward": {
          "bind_address": null,
          "bind_port": 8080,
          "description": "remote 8080 → localhost:3000",
          "kind": "remote",
          "spec": "8080:localhost:3000",
          "target_host": "localhost",
          "target_port": 3000
        },
        "host": "prod",
        "name": "preview",
        "status": {
          "finished": 1781861043,
          "reason": "the server refused the remote forward on port 8080",
          "state": "failed"
        },
        "warnings": []
      }
    },
    {
      "error": {
        "kind": "unknown-tunnel",
        "message": "no tunnel named nope",
        "next": []
      },
      "name": "nope",
      "outcome": "failed",
      "pid": null,
      "tunnel": null
    }
  ],
  "schema": 1
}
```

| Field | Meaning |
|---|---|
| `name` | The tunnel |
| `outcome` | `start`: `started`, `already-running` or `failed`; `stop`: `stopped`, `not-running` or `failed` |
| `pid` | The ssh process for `started` and `already-running`, otherwise `null` |
| `error` | An error object when `outcome` is `failed`, otherwise `null` |
| `tunnel` | The tunnel's report after the action, or `null` when no tunnel has that name |

The exit status is 1 when any result failed. Once names are considered, every failure is a result, even an unreadable tunnels file: each name then fails with that message and `tunnel: null`, while `stop` still stops tunnels Portico has records of. The error document is only for a missing name and for failures before any name is considered. Starting a running tunnel or stopping a stopped one succeeds. `stop --all` stops every running tunnel, and reports none when nothing runs:

<!-- golden: tunnel-stop-all.json -->
```json
{
  "results": [],
  "schema": 1
}
```

**Run `start` and `stop` asynchronously.** `start` returns when ssh has established the tunnel or given up, which can take as long as the host's `ConnectTimeout`; `stop` can wait a few seconds for ssh to exit. The document arrives when they finish.

## What is never printed

No key material and no credential: public keys appear only as type, size, fingerprint and comment. No environment variable's value, except the agent socket path. No command line Portico did not build (no `ProxyCommand`, no container command); the one command line printed is the tunnel's own `ssh` invocation. From logs and the output of ssh, agents and docker, only what `portico` prints for people, as one line with terminal control characters removed.

## Versioning

This contract is public: a consumer may be installed separately from Portico and run against another release. Within `schema` 1, a later Portico may:

- add keys to any object (ignore keys you do not know);
- add values to `error.kind`, a warning's `kind` and docker's `problem` (treat an unknown value as `other`);
- give `--json` to more commands.

Anything else raises `schema`: removing or renaming a key, changing a value's type or meaning, or adding a value to `status.state`, `outcome`, `availability`, `agent.state`, `ownership`, `target.kind` or `forward.kind`. Portico prints only its current schema. A consumer that finds a `schema` it does not know should say that this Portico is too new (or too old) for it rather than guess.
