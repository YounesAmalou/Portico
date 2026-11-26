# Remembered Container User Design

**Status:** Approved; implemented

## Purpose

A container tab in the TUI has one `user:` line, which `u` edits and which every shell opened from that tab then runs as. Nothing keeps it: the next time Portico starts, the line says "image default" again, and someone who always enters `shop/api` as `deploy` types `deploy` again every time. The same user also follows the selection to every other container in the tab, where it is usually wrong.

This slice remembers the user chosen for each container in the settings file (`~/.config/portico/settings.toml`, from the Settings slice), so it becomes that container's default the next time. It implements v4 slice 1 of `ROADMAP.md`, "Remembered container user".

## Goals

- In the TUI, the user line belongs to the selected container: it shows that container's remembered user, or "image default".
- Opening a shell from the TUI (Enter or `n`) with a user remembers that user for the container; opening one with a blank user forgets it.
- A remembered user survives the container being recreated (a Compose redeploy gives it a new ID and sometimes a new name).
- The same Compose project on two machines keeps a user per machine.
- `portico settings` lists what is remembered (also as JSON) and can forget an entry; the file can be edited by hand.

## Non-goals

- **The CLI's `portico shell` ignores the file**, as every CLI command but `portico settings` does since the Settings slice. Without `-u` it uses the image's default user, and `-u` saves nothing. (Decided by the maintainer.) Because of this, the desktop panel's shells are not affected either.
- Proxmox containers: their shells always run as root, so nothing is remembered for them.
- No list of several users per container to choose from; one user, the last one used.
- No live reload of hand edits while the TUI runs (as for tabs).

## Anti-goals

- A user saved that was never used: a typo in the user line that is never opened into a shell is not written.
- A write that loses the user's comments or layout, or overwrites an invalid file (the Settings slice's rules hold).
- A remembered user that silently applies to the wrong container, such as another machine's container with the same name.

## Rules this design keeps

No daemon, no database: the remembered users are a table in the one hand-editable preferences file that the roadmap already allows, written atomically under the same lock. No credential is stored; a user name is not one.

## Settled questions

Decided by the maintainer before the spec was written.

- **What identifies a container:** the machine (this one, or a host alias), the backend, and the Compose `project/service`. A container that is not part of a Compose project is identified by its name. Replicas of a service (`api#1`, `api#2`) share their service's entry.
- **When a user is saved:** when a shell is opened from the TUI with it, by Enter or `n`. Editing the line with `u` alone does not save.
- **The user line follows the selection**: it shows the selected container's user.
- **The CLI ignores the file** (see Non-goals).
- **Clearing:** opening a shell with a blank user forgets the container's entry; `portico settings forget …` forgets one from the CLI; hand edits work too. `portico settings` and its JSON list the remembered users.
- **File layout:** nested tables, approach A below.

## Approaches considered

For the file layout:

1. **Nested tables (chosen):** `[users.local.docker]` and `[users.host.<alias>.<backend>]`, each holding `"<project>/<service>" = "<user>"` or `<name> = "<user>"`. Entries are grouped by machine and backend, easy to scan and edit by hand, in the style of `[tabs]`. `local` and `host.<alias>` are separate levels, so a host whose alias is `local` cannot be mistaken for this machine. `toml_edit` quotes keys that need it (an alias with a dot, a `project/service`).
2. **One flat table with combined keys:** `[users]` with `"prod podman shop/api" = "app"`. One line per entry, but Portico would have to split the keys apart, and a long file is hard to scan.
3. **An array of tables:** `[[users]]` with `target`, `backend`, `container` and `user` fields. The most explicit, but four lines per entry, and changing one entry in place means finding its block, which is clumsy with `toml_edit`.

For the TUI, the maintainer chose saving on shell open over saving when the `u` edit is confirmed (which would store users never used), and a line that follows the selection over keeping one line per tab prefilled from the file (which would not show each container's user).

For the CLI, reading the remembered user in `portico shell` (and so in the panel) was weighed and declined, to keep the Settings slice's rule that only `portico settings` reads the file.

## Decisions

### The file

```toml
# ~/.config/portico/settings.toml
[tabs]
podman = false

[users.local.docker]
"shop/api" = "deploy"
web = "www-data"            # a container outside Compose: its name

[users.host.prod.podman]
"shop/api" = "app"

[users.host."db.example".docker]
"shop/db" = "postgres"
```

- `users` is optional, and so is every table under it. Nothing remembered is the default.
- Under `users`, `local` holds this machine's entries and `host` holds one table per host alias. Under each machine, one table per backend, named as `--backend` names it (`docker`, `podman`). Each entry's key is the container's key (below), and its value is the user, a non-empty string.
- **Invalid:** `users`, `users.local`, `users.host`, a `users.host.<alias>` or a backend table that is not a table, or an entry whose value is not a string or is blank, makes the file invalid, as a wrongly typed tab does. The error gives the path, the line and the reason. As today, an invalid file means every default is used (no tab hidden, nothing remembered) and Portico never writes over it.
- **Ignored and kept:** an unknown backend name, and a `proxmox` table (whose shells run as root), whatever their type or content; any key under `users` other than `local` and `host`. They are kept when writing and give no warning, as unknown keys are. Types are checked only where Portico reads: `users`, `users.local`, `users.host`, each `users.host.<alias>`, and the `docker` and `podman` tables and their entries. (Builder's call, so a file from another version stays valid.)
- A user is stored as typed after trimming spaces at both ends.

### The container key

- A container with Compose labels (`project` and `service`): `<project>/<service>`. Replica numbers are left out.
- Any other container: its name.
- A name cannot contain `/` for Docker or Podman, so the two forms never collide.
- The key is computed from the listing (`backend::listing::Container`) in one function in the backend layer, `remember_key(&Container) -> String`, so the TUI and `portico settings forget` agree.

### Domain (`domain::settings`)

- `Settings` gains `users: BTreeMap<UserKey, String>`, where `UserKey { target: Target, backend: BackendId, container: String }` (`Target` being `backend::Target`: `Local` or `Host(alias)`). `BTreeMap` keeps a stable order for printing and JSON. If `Target` or `BackendId` do not order, the domain adds the ordering it needs (builder's call).
- `Settings::user(&UserKey) -> Option<&str>`.
- `remember_user(path, &UserKey, Option<&str>) -> Result<bool, SettingsError>`: `Some(user)` sets the entry, `None` removes it. It returns whether the file changed. Like `set`, it writes nothing when the entry already says that (and so creates no file for a `None` on a missing file). Removing an entry leaves an empty table in place (the user may have put comments in it); the builder may drop tables that become empty if `toml_edit` keeps that clean.
- `set` and `remember_user` share one read-modify-write path (lock, re-read, edit with `toml_edit`, atomic write, through symlinks, refusing an invalid file). The boolean `Key` enum stays as it is for tabs.
- `remember_user` refuses a Proxmox key (a programming error; the TUI never asks).

### App

- `App::remember_container_user(&UserKey, Option<&str>) -> Result<bool, SettingsError>`, beside `set_setting`, which both the TUI and `portico settings forget` call.

### TUI

- At startup the TUI already loads the settings for the tabs. Each container `Panel` now also takes the remembered users of its backend.
- Each panel keeps a map from container key (on its target) to user for the session, starting from the file. The user line shows the selected container's entry, or "image default". As the selection moves, the line changes.
- `u` edits the selected container's entry. Enter keeps the edit in the session map, and Esc restores the previous value, as today. Nothing is written yet. (Builder's call: an edit is kept for the session even if no shell is opened, so moving away and back does not lose it, but it reaches the file only with a shell.)
- Enter or `n` on a container opens the shell as today, with the line's user. The TUI then remembers the user for that container through `App::remember_container_user`: the user when one is set, or `None` (forget) when the line is blank. The write happens when the shell action is handled, before the terminal is suspended. A write that changes nothing (the user was already remembered) does nothing.
- If the write fails (for example, the file became invalid), the shell still opens. The status line afterwards reports that the user was not remembered, with the settings error. If the file was already invalid at startup, the TUI does not try to write, and the problem shown at startup stands.
- A Proxmox panel has no user line to edit (unchanged) and never writes.
- The details pane's `user:` line shows the same value as the user line.

### CLI

`portico settings` lists the remembered users after the tabs:

```
tabs.tunnels  on
tabs.docker   on
tabs.podman   off  (default on)
tabs.proxmox  on

Remembered container users:
  this machine  docker  shop/api  deploy
  this machine  docker  web       www-data
  prod          podman  shop/api  app

File: ~/.config/portico/settings.toml
```

- The section is left out when nothing is remembered. Columns are aligned; this machine sorts first, then hosts by alias, then backend, then key.
- `portico settings forget [--on <alias>] [--backend docker|podman] <container>` removes one entry. `<container>` is the key as listed (`shop/api` or a name). It is not resolved against a live listing, because the container may not be running. `--backend` defaults to Docker, as for `portico shell`; without `--on` it means this machine. `--on` takes the alias as a plain string: it is not checked against the inventory and opens no picker, so the entries of a host removed from the SSH config can still be forgotten. (Builder's call.) `--backend proxmox` is an argument error. Forgetting an entry that does not exist writes nothing, prints `nothing remembered for …`, and exits 0, as `set` does for a value already in effect.
- `portico shell` is unchanged.

### JSON

`portico settings --json` gains a `users` array, sorted as the CLI lists them:

```json
{
  "exists": true,
  "path": "/home/me/.config/portico/settings.toml",
  "schema": 1,
  "tabs": { "docker": true, "podman": false, "proxmox": true, "tunnels": true },
  "users": [
    {
      "backend": "docker",
      "container": "shop/api",
      "target": { "alias": null, "kind": "local" },
      "user": "deploy"
    }
  ]
}
```

- `target` has the same shape as in the containers document. The array is empty when nothing is remembered. Adding a key is allowed within schema 1.
- `docs/json-output.md`'s Settings section gains `users`, pinned by the existing golden file (updated) so the example stays exact.

## Testing

- `domain::settings`: users load from nested tables; a host alias with a dot; wrong types and blank values are invalid with their line; unknown backends and `proxmox` tables are ignored and kept; `remember_user` creates, replaces and removes an entry, keeps comments, writes through a symlink, writes nothing when unchanged, and refuses an invalid file; tabs and users coexist.
- `remember_key`: a Compose container, a replica, a container without Compose.
- App: the round trip through `App::remember_container_user`.
- TUI state tests: the line follows the selection; the session map starts from the file; `u` edits do not write; Enter and `n` write the user, and a blank line forgets; a failed write still opens the shell and reports in the status line; Proxmox never writes.
- CLI: `portico settings` with and without users (golden output); `settings --json` with users (golden); `settings forget` for an entry that exists, one that does not, `--on`, and `--backend proxmox` refused.
- An end-to-end test through the PTY: in a Docker tab against the fake `docker`, set the user with `u`, open a shell, quit, start the TUI again, and check that the line shows the user and the file holds it.

## Documentation

- README: the remembered user in the container tabs, and `portico settings forget`.
- `docs/json-output.md`: `users` in the Settings document.
- `docs/architecture.md`: `domain::settings` now holds the remembered users; `remember_key` in the backend layer.
- `ROADMAP.md`: remove the slice; v4 then has no items left and its heading goes, with the later slices renumbered. The v1 spec's "Delivered follow-up slices" gains a "Remembered container user" entry.

## Left to the builder

- The exact wording of the status-line message and of `forget`'s output.
- Whether the details pane marks a remembered user (for example "deploy (remembered)") or shows it plainly.
- Whether empty tables are dropped after a forget.
