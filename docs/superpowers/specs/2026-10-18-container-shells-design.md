# Container Shells Design

**Status:** Draft, awaiting review

## Purpose

Opening a shell in a container today means connecting to the host, running `docker ps`, copying a name, and typing `docker exec -it … sh`. This slice lets Portico list the running containers on the local machine or on a host it knows, named and grouped by their Compose project and service, and open a shell in one. That works from the CLI, with a picker, and from the TUI, in a containers panel under the hosts.

It implements v2 item 2 of `ROADMAP.md`, "Container shells (Docker and Compose)". Scope stops at listing and a shell: logs, restarts and Compose lifecycle stay out.

## Settled questions

- **Docker only.** Podman becomes a separate roadmap item, and `podman` is never run.
- **Nothing stored.** No list, user choice or preference is written to disk. Remembering the last user per service is a separate roadmap item that needs its own design for a Portico-owned preferences file.
- **Listing over ssh.** The CLI lets ssh prompt (password, unknown host key) as `portico connect` does. The TUI lists in the background with `BatchMode=yes` and explains a failure instead.
- **Docker that needs privileges.** Portico explains what is wrong and offers to fix it. Each fix is one command shown in full first and run in the user's terminal through `sudo`, so sudo asks for the password itself and Portico never sees it. The user can decline, and then Portico only lists the options. No fix needs a logout (see "Docker problems").
- **fzf.** The CLI picker uses `fzf` when it is on PATH. Using fzf for the rest of the CLI (choosing a host for `connect`, `test` and the others) is a separate roadmap item.

## Decisions

### Targets

A target is either **this machine** or **a host in Portico's inventory**, managed or external, looked up by alias the same way `portico connect` does it. Docker commands run directly on this machine. On a host they run as the remote command of `ssh`, with the alias Portico knows, so its configuration (jump hosts included) applies unchanged.

The remote command is one string that Portico builds by quoting every word for a POSIX shell, single quotes with `'\''` for embedded quotes. Every ssh invocation adds `-o RemoteCommand=none`, because ssh refuses a command-line command when the configuration sets one.

### Listing

- **The command** is `docker ps --no-trunc --format <template>`, where the template prints one flat JSON object of strings per container:
  - id, name, image, command, running for, status, ports, networks, mounts
  - the labels `com.docker.compose.project`, `com.docker.compose.service`, `com.docker.compose.container-number` and `com.docker.compose.project.working_dir`

  Each field is written with Go's `json` template function, so any character in a value survives. Portico parses these lines with a small parser for flat string objects, so no new dependency is needed. A line that does not parse is skipped and counted, and the listing says how many lines were skipped.
- **Over ssh** the CLI runs `ssh -o RemoteCommand=none -- <alias> <command>` with stdin inherited, so ssh can prompt on the terminal, and stdout and stderr captured. The TUI adds `-o BatchMode=yes`, uses a null stdin, and runs the listing on a worker thread that Esc or quitting cancels.
- **Grouping.** Containers that have a Compose project label are grouped by project, with projects sorted by name. Inside a project they are sorted by service, then container number. Containers without the label come last, under "Other", sorted by name.
- **Display name.** A Compose container shows its service, with `#<number>` added when the service has more than one container. Any other container shows its name.

### Choosing a container

`portico shell <container>` accepts the following references, tried in this order:

1. an exact container name
2. `project/service`, or `project/service#number`
3. `service`, when exactly one project has that service
4. a container ID prefix of at least 4 hexadecimal characters

A reference that matches nothing is an error naming the target. A reference that matches several containers (replicas, or one service in several projects) is an error that lists the candidates as unambiguous references. The reference is resolved against a fresh listing, so the shell always runs against the full container ID.

### Shell

- **The command:** `docker exec -it [-u <user>] <id> sh -c 'command -v bash >/dev/null 2>&1 && exec bash || exec sh'`. Without `-u`, Docker uses the image's own user. `docker attach` is never used.
- **This machine:** Portico runs it with the terminal inherited and exits with docker's status.
- **A host:** Portico runs `ssh -t -o RemoteCommand=none -- <alias> <quoted command>` through the same path as `portico connect`. ssh's stderr is mirrored and an exit status of 255 is explained exactly as a connection failure is (jump hosts included). Any other status belongs to docker or the shell, and is passed through unchanged.
- **New terminal (`-n`):** opens `xdg-terminal-exec <portico> shell --hold [--on <alias>] [-u <user>] <container id>`. The container is resolved before launching, so the new terminal does not list again. When Portico's own path is unknown, the terminal runs the ssh or docker command directly. The launcher's result is reported as it is for `portico connect -n`.

### Docker problems

Each problem is recognised from docker's exit status and stderr. On a host the same text arrives through ssh.

| Problem | Recognised by | What Portico does |
|---|---|---|
| Docker not installed | This machine: `docker` cannot be spawned (not found). Host: exit status 127 with `docker: command not found`, or `not found`, in stderr | Explains that Docker is not installed on the target, or is not on the PATH of a non-interactive shell there, and suggests installing it from the distribution's packages. Portico does not install packages |
| Daemon not running | `Cannot connect to the Docker daemon` | Explains it and offers to run `sudo systemctl start docker`, but only when `systemctl` exists on the target. Otherwise it suggests starting the daemon |
| No access to the socket | `permission denied while trying to connect to the Docker daemon socket` or `… to the docker API` | The group flow below |

**The group flow.** Portico first runs a short read-only probe on the target. It reports:
- the user name
- the groups of the current process
- the user's groups in the group database
- whether the `docker` group exists
- the socket's group (`/var/run/docker.sock`, or the path in `DOCKER_HOST` when that is a `unix://` path)
- whether `sg` and `newgrp` exist

Then:

1. **The user is in the `docker` group in the database, but not in the current process.** This happens with an older login session, or with an ssh ControlMaster connection opened before the change. Portico reruns the docker command inside the group, with no prompt, using `sg docker -c <command>` when `sg` exists and `newgrp docker -c <command>` otherwise. It notes once that other shells need `newgrp docker` or a new login. If neither tool is available, it explains that a new login is needed.
2. **The user is not in the group.** Portico explains the failure and asks:

   ```
   Add <user> to the docker group on <target>? Members of the docker group have root-equivalent access.
   This runs: sudo sh -c 'groupadd -f docker && usermod -aG docker "$1" && chgrp docker <socket>' sh <user>
   [y/N]
   ```

   - `groupadd -f` creates the group when it is missing.
   - `chgrp` gives the existing socket to the group. A daemon started while the group was missing owns its socket as `root:root`, and this avoids restarting Docker, which would stop running containers. The daemon itself uses the group from its next start.

   On yes, the command runs in the user's terminal (through `ssh -t` on a host), and then the original action is retried inside the group as in case 1. On no, or when the CLI is not interactive, Portico prints the options: that command to run yourself, rootless Docker, or running `sudo docker` yourself in a `portico connect` session.
3. **The socket belongs to another group,** or the probe fails. Portico explains it with docker's own message and makes no offer.

The same offers exist in the TUI, as keys shown in the containers panel's error (see "TUI").

### CLI

```
portico containers [--on <alias>]
portico shell [<container>] [--on <alias>] [-u|--user <user>] [-n|--new-terminal]
```

- Without `--on`, both commands work on this machine.
- **`portico containers`** prints the grouped list: a project header, then one indented row per container with the display name, container name, image and status. Columns are aligned, and "Other" comes last. When nothing is running it prints `No running containers on <target>.`
- **`portico shell` without `<container>`** needs an interactive terminal. It opens a picker over the same rows: `fzf` when it is on PATH (one line per container, prefixed with its project, filtering as you type), otherwise a numbered menu read from stdin. Cancelling exits with status 130 and runs nothing. Non-interactive use without `<container>` is an error. `-u`, `-n` and `--on` apply to the chosen container.
- `--hold` is hidden and used by the new-terminal launch, as for `connect`.
- The exit status is the shell's, docker's or ssh's, mapped the way `portico connect` maps it.

### TUI

- **Layout.** The left column splits: hosts on top, a **Containers** panel below. The panel is always shown, and it keeps at least a few rows for itself in short terminals. Tab and Shift+Tab move the focus between the panels. The details pane on the right describes whatever is focused: the selected host as today, or the selected container (name, ID, image, status, running for, ports, Compose project, service and working directory, networks, mounts and command, all from the listing, with no extra round trip).
- **What the panel lists.** It follows the selected host. Pressing **L** in the panel switches it to this machine, and its title reads `Containers: this machine`. Moving the host selection switches it back.
- **When it lists.** On demand only: when the panel gains focus for a target that has not been listed yet, or on **r**. Until then it reads `Tab to list containers`. Results are kept in memory for the session, per target (no disk). While a listing runs, the panel reads `Listing…`, and Esc cancels it.
- **Keys in the containers panel:**

  | Key | Effect |
  |---|---|
  | Up, Down, j, k | Move the selection; project headers are skipped |
  | Enter | Shell in this terminal (the TUI is suspended as for connect) |
  | n | Shell in a new terminal |
  | u | Edit the user for the next shell, inline in the panel: empty means the image's user. It applies until changed and is never saved |
  | r | List again |
  | L | Switch the panel to this machine |
  | g / s | Shown only when the error offers it: run the group fix or start the daemon. The TUI is suspended so sudo can prompt, then the panel lists again |
  | Tab, Shift+Tab, Esc | Back to the hosts panel |
  | q | Quit |

- **Failures** appear in the panel, as an explanation for ssh failures in the same style as connect, and as the Docker problem texts above otherwise. A shell that fails is reported in the status line, as a failed connection is.
- The footer shows the panel's keys while it has focus.

## Code layout

- **`src/container.rs` (new, pure, unit-tested):**
  - `Container` (the listed fields) and `Listing` (the containers plus the count of skipped lines)
  - the `ps` template, and the parser for flat JSON string objects
  - `group(&[Container])`: projects and "Other", in order
  - `resolve(reference, &[Container]) -> Result<&Container, Resolve>`, with not-found and ambiguous cases
  - `quote(word)` and `remote_command(words)` for the POSIX shell
  - the shell command
  - `classify(status, stderr) -> DockerProblem`
  - the group probe's script and parser, with the decision it leads to
- **`src/app.rs`:** `Target::{Local, Host(alias)}`, plus `containers`, `container_shell`, `docker_fix` and their cancellable TUI variants. These reuse `connectable_alias` and the connection explanation.
- **`src/openssh.rs`:** the smallest possible changes. `connect_mirrored` and `launch_in_new_terminal` gain a variant that takes extra ssh arguments and a remote command (or the portico arguments for the new terminal), and the existing functions call it with none. A cancellable `run_remote` covers the captured listing.
- **`src/cli.rs`:** the `containers` and `shell` commands, the fzf and numbered pickers, and the yes/no offers.
- **`src/tui/containers.rs` (new):** panel state, the listing worker (modelled on `tester.rs`), key handling and rendering. `src/tui.rs` gains only the layout split, focus switching, the details hand-off, and actions to suspend for a shell or a fix.
- **`README.md`:** the commands, the panel's keys, and a note on the Docker requirements and offers. `CHANGELOG.md` comes from the `feat:` commit. `ROADMAP.md` is updated by its owner, not in this slice.

## Out of scope

- Podman, Proxmox (`pct`), and any other backend.
- Logs, restarts, `docker compose up`/`down`, and stopped containers.
- Remembering users or any other preference.
- Installing Docker.
- fzf outside `portico shell`.
- Machine-readable output (`--json` is v3).

## Testing

- **Unit (`container.rs`):**
  - template line parsing, including escaped quotes, backslashes, `\uXXXX` and empty labels; malformed lines are counted
  - grouping and sort order, and display names with replicas
  - every reference form, with not-found and ambiguous errors
  - quoting of spaces, quotes and empty words
  - the shell command with and without a user
  - classification of each Docker problem from real docker messages, including both permission wordings
  - probe parsing and its three outcomes, and `sg` versus `newgrp`
- **CLI (fake executables, as in the existing tests):**
  - `containers` on this machine and on a host: the ssh arguments, the quoting and the output
  - `shell` with each reference form; the exact `docker exec` and `ssh -t` command lines; `-u`; `-n` launching `portico shell --hold` with the resolved ID
  - the picker's numbered fallback; non-interactive use without a container
  - each Docker problem's message
  - the group offer accepted (fake `sudo` records its arguments and the retry runs through `sg`) and declined
  - an ssh exit of 255 explained as for connect
- **TUI (unit, as in `tui.rs`):** Tab focus; listing on focus only; the `L` switch and switching back; Enter and `n` producing shell actions with the chosen user; `u` editing; the error keys appearing only for their problem; the details pane for a container; the footer.
- **Not covered:** real Docker in CI. The existing real-OpenSSH tests stay unchanged.
