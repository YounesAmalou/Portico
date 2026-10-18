# Container Shells Design

**Status:** Approved design; spec under review

## Purpose

Opening a shell in a Docker container today means connecting to the host, running `docker ps`, finding the container that belongs to a Compose service, and typing a `docker exec` line. This slice lists the running containers on the local machine or on a host Portico knows, names them by their Compose project and service, and opens a shell in the one chosen, from the CLI and the TUI.

It implements v2 item 2 of `ROADMAP.md`, "Container shells (Docker and Compose)". Scope stops at listing and a shell: logs, restarts and Compose lifecycle are not part of it.

## Decisions

### Settled open questions

- **Docker only.** Portico runs the `docker` binary. Podman support is a separate roadmap item.
- **Nothing is stored.** Every listing runs `docker` again, and no user choice is remembered. Remembering preferences, such as the last user chosen per Compose service, is a separate roadmap item that needs its own design for a Portico-owned state file.
- **Docker that needs a password or sudo.** Portico never runs `docker` through `sudo`. When `docker` is denied the daemon socket, Portico explains why and offers to add the user to the `docker` group (see "Docker group access"). The user can decline; Portico then lists the options and stops.
- **Listing over ssh.** From the CLI, ssh may prompt (password, unknown host key) exactly as `portico connect` does. The TUI lists in the background with `BatchMode=yes`, so a host that needs a prompt fails there with an explanation.

### Targets

A target is either the local machine or a host alias from the inventory (managed or external, matched as `portico connect` matches it). On the local machine Portico runs `docker` and the other commands below directly. On a host it runs them as a remote command of `ssh`:

- Listing and the group checks: `ssh -T -o RemoteCommand=none -- <alias> <command>`, with stdout and stderr captured. The CLI leaves stdin attached so ssh can prompt; the TUI adds `-o BatchMode=yes` and gives ssh no stdin.
- The shell and the group change: `ssh -t -o RemoteCommand=none -- <alias> <command>`, run as `portico connect` runs ssh: in this terminal with stderr mirrored, so an ssh failure (exit 255) gets the same explanation as a connection, jump hosts included.

`RemoteCommand=none` keeps a host whose configuration sets `RemoteCommand` usable, since ssh refuses both a configured and a command-line remote command.

The remote command is one string that the remote login shell parses. Every word is single-quoted (a `'` inside a word becomes `'\''`), which POSIX shells, fish and csh all read the same way. Words that need no quoting (letters, digits, `-_./=:@,%+`) are written bare so the command stays readable in `ps`.

### Listing

```
docker ps --no-trunc --format '{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Label "com.docker.compose.project"}}\t{{.Label "com.docker.compose.service"}}\t{{.Label "com.docker.compose.container-number"}}'
```

Each output line is one running container: full ID, name, image, status, then the Compose project, service and container number (empty when the container has no such label). A line without seven fields is skipped. Containers are grouped by project: projects sorted by name, inside a project by service then number; containers with no project form a last group, "Other", sorted by name.

### Naming a container

`portico shell` takes a reference resolved against a fresh listing, in this order; the first rule that matches exactly one container wins, and a rule matching several is an error naming them:

1. The exact container name.
2. The exact full ID.
3. `project/service`, which matches every replica of that service.
4. `service`, across all projects.
5. A prefix of the ID, like `docker` accepts.

No match is an error that suggests `portico containers`. Several matches list each candidate with its name, project and service, so the user can repeat the command with a name.

### The shell

```
docker exec -it [-u <user>] <full id> sh -c 'command -v bash >/dev/null 2>&1 && exec bash || exec sh'
```

Never `docker attach`. Without `-u` docker uses the image's own user. `bash` runs when the container has it, `sh` otherwise. The exit code is the command's own (`portico connect`'s rule: the code, or 128 plus a signal). Docker's own errors, such as a container without `sh`, appear as docker prints them.

A new terminal (`-n`) runs `xdg-terminal-exec <portico> shell --hold [--on <alias>] [-u <user>] -- <full id>` when Portico's path is known, so a failure is explained and held on screen as for `portico connect -n`. Otherwise, it runs the plain command (`docker exec …` or `ssh -t … <alias> docker exec …`). The held run lists again to resolve the ID, so it also works out `sg` by itself.

### Recognised docker failures

A listing that fails is classified from its exit status and stderr:

| Cause | Recognised by | Explanation |
|---|---|---|
| ssh failed | exit 255 on a host | As `portico connect` explains it |
| docker missing | exit 127, or "docker: command not found" / "docker: not found" | Docker is not installed on the target, or not on the `PATH` of a non-interactive shell there |
| daemon down | "Cannot connect to the Docker daemon" | The daemon is not running; start it (for example `sudo systemctl start docker`) |
| socket denied | "permission denied while trying to connect to the docker" (any case) | See "Docker group access" |
| anything else | | docker's own message, unchanged |

Explanations use the existing `Explanation` type and its rendering, so they read like the others.

### Docker group access

When docker is denied the socket, Portico runs one check on the target:

```
sh -c 'id -un; id -Gn; id -Gn "$(id -un)"; getent group docker >/dev/null && echo docker-group'
```

It prints the user, the groups of this session, the groups the group database gives that user, and whether a `docker` group exists.

- **Member, but this session predates it** (in the database's groups, not the session's; this happens for a desktop session that has not logged out, or for a reused ssh ControlMaster connection): Portico runs the docker command again through `sg docker -c '<docker command>'`, and does the same for the shell. No logout is needed. The CLI notes `Using the docker group through sg until you log in again.` on stderr.
- **Not a member, and the group exists:** Portico explains the denial and, when interactive, asks:
  `Add <user> to the docker group on <target>? Members of the docker group have root-equivalent access to <target>. [y/N]`
  On yes it runs `sudo usermod -aG docker <user>` in the terminal (through `ssh -t` on a host), so sudo asks for the password itself and Portico never sees it. When that succeeds it lists again (now through `sg`, since this session predates the change). On no, or without a terminal, it prints the options instead: the `sudo usermod` command to run yourself, rootless Docker, or running `sudo docker` yourself after `portico connect`.
- **No docker group:** adding a member would not help, so Portico explains that the socket is not shared through a `docker` group on that target and lists the same options except the group command.
- **Member in this session too:** the socket's group or mode is not what Docker sets up; Portico shows docker's message and the explanation without an offer.

### CLI

```
portico containers [--on <alias>]
portico shell <container> [--on <alias>] [-u|--user <user>] [-n|--new-terminal]
```

Without `--on`, the target is the local machine. `portico containers` prints one block per group:

```
shop
  web     shop-web-1     nginx:1.27    Up 2 hours
  db      shop-db-1      postgres:16   Up 2 hours
Other
  pgadmin dpage/pgadmin4:latest        Up 3 hours
```

A Compose row shows service, name, image and status in aligned columns; an "Other" row shows name, image and status. No running containers prints `No running containers on <target>.` and exits 0. A failure prints the explanation on stderr and exits with the failed command's code (1 when Portico itself fails). `portico shell` hides a `--hold` flag that works as `portico connect --hold`.

### TUI

- `c` on the selected host opens the container picker for that host; `C` opens it for the local machine.
- The picker replaces the host list in the left pane, titled `Containers on <target>`. It shows `Listing containers…` while the background listing runs; `Esc` cancels it and returns to the hosts. Group headers (project names, then `Other`) are not selectable; rows show `service  name` (or `name` under Other). The details pane shows the selected container: name, image, status, project, service, ID, and the user the shell will run as (`image default` when none is set).
- Keys in the picker: `↑`/`↓` move, `Enter` opens the shell in this terminal (the TUI suspends as for a connection), `n` (and `Ctrl+Enter` where detected) in a new terminal, `u` edits the user in a one-line prompt (`Enter` keeps it, `Esc` discards the edit; an empty value means the image's user), `r` lists again, `Esc` returns to the hosts. The user applies to the next shell opened from this picker and is forgotten when the picker closes.
- A failed listing shows its explanation in the pane. When the offer above applies, the footer adds `g add to docker group`: `g` suspends the TUI, runs the `sudo usermod` command in the terminal, then lists again. The `sg` retry happens without asking, as in the CLI.
- After a shell ends the status line reports it as after a connection, and the picker stays open.
- Footer in the picker: `↑↓ move  Enter shell  n new terminal  u user  r refresh  Esc back`.

## Code layout

- `src/container.rs` (new, pure, no process spawning): `Container`, `Group`, `parse_ps`, `group`, `resolve(reference) -> Result<&Container, Resolution>`, `ps_args`, `shell_args(user, id)`, `remote_command(words) -> String` (quoting), `GroupCheck::parse`, and `classify(status, stderr) -> DockerFailure`.
- `src/docker.rs` (new): runs commands on a target (`Target::{Local, Host(alias)}`): the listing, the group check, the `sg` retry, the shell and the group change, using `OpenSsh` for the ssh side.
- `src/openssh.rs`: `connect_mirrored` and `launch_in_new_terminal` gain variants that take a remote command and a launcher command; the existing functions call them unchanged, so their behaviour and tests stay as they are.
- `src/app.rs`: `containers(target)`, `container_shell(target, reference, user, new_terminal)` and `join_docker_group(target)`, reusing `connectable_alias` and `explain_connection`.
- `src/cli.rs`: the `containers` and `shell` commands and the group prompt.
- `src/tui/containers.rs` (new): the picker's state, keys, background lister and rendering. `src/tui.rs` gains only a `Mode::Containers`, its `UiAction`s, the `c`/`C` bindings and dispatch into the new module, to keep merges with other work small.
- `README.md`: the two commands, the picker keys, the docker group behaviour, and Docker as a requirement for this feature only. `ROADMAP.md` is updated separately. `CHANGELOG.md` comes from the `feat:` commit.

## Out of scope

- Podman, remembered preferences (separate roadmap items).
- Stopped containers, logs, restarts, Compose `up` and `down`.
- Running `docker` through `sudo`, or storing anything about a host's Docker.
- A container column in `portico hosts`, or machine-readable output (v3).

## Testing

- **Pure (unit, `container.rs`):** parsing well-formed, short and empty lines; grouping and ordering with several projects, replicas and standalone containers; each resolution rule, its precedence and ambiguity errors; quoting of plain words, spaces, `'`, `$` and an empty word; `shell_args` with and without a user; group-check parsing for each case; failure classification for each table row.
- **Runner and App (unit, fake executables as the existing tests use):** a fake `docker` for local listing and exec; a fake `ssh` that records its arguments and runs the remote command through `sh -c`, so the quoting is exercised for real; `sg` retry when the check reports a stale session; the explanation for ssh exit 255; new-terminal arguments with and without Portico's path.
- **CLI (`tests/cli.rs`):** `containers` output, empty output and failure exit codes; `shell` resolution errors; the group prompt answered yes (fake `sudo` records its arguments, then the listing succeeds) and no (options printed, nothing run); no prompt when non-interactive.
- **TUI (unit, `tui.rs` and `tui/containers.rs`):** `c` and `C` open the picker for the right target; loading, cancel with `Esc`, rows and headers, selection skipping headers, the user prompt, `Enter`/`n` produce the shell action with the user, `g` appears only when the offer applies, and the footer.
- Tests never need a real Docker daemon and run as a regular user.
