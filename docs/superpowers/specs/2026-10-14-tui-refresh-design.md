# TUI Refresh Design

## Purpose

The TUI reads the host inventory once at startup and afterwards only when Portico itself changes it. Host details resolved with `ssh -G` are cached until the inventory changes. Edits made elsewhere, in `~/.ssh/config`, in an included file, or in the managed file by another tool, therefore stay invisible until Portico restarts. This slice adds a refresh key that re-reads everything. It implements the "TUI refresh" follow-up slice of `2026-09-13-portico-host-management-design.md` and changes nothing else in that design.

## Decisions

- **Keys:** `r` and `F5`, in browse mode only. `r` is unused there; in the filter and the forms it stays an ordinary character. `F5` is the conventional refresh key for users who expect it. The browse footer advertises `r refresh` in every state, including when no host is shown, since an empty list is exactly when an externally added host should be pulled in. The footer names only `r`; the README lists both.
- **What refresh does:** it calls `App::hosts()`, the same read the TUI uses at startup and after a mutation, so discovery (including `Include`d files and warnings) and the managed file are read again. On success it replaces the host list and warnings, discards every cached host detail, cancels any `ssh -G` in flight, and requests the selected host again. Details are discarded even when the host list is unchanged, because included configs can change what `ssh -G` resolves without changing any alias.
- **Selection:** stays on the same host when it still exists. A row is matched first by its full identity (alias, ownership and source file), then by alias alone, ignoring ASCII case, so a host whose definition moved to another file stays selected. If the alias is gone the selection falls to the first visible host, as for any inventory change. The applied filter is kept.
- **Status:** a successful refresh shows `Refreshed: N hosts`, using the singular for one. The count is the number of hosts in the inventory, not the number matching the filter.
- **Errors:** if the inventory cannot be read (unreadable or unparsable file, incomplete discovery), the UI state is untouched: the old list, selection, filter and cached details stay, and the status line shows `Refresh failed: <error>`. Nothing is discarded unless the new inventory was read.
- **Scope:** the CLI reads files on every invocation and has no cache, so it gains nothing. Refresh never writes any file and does not re-run initialization.

## Testing

- Unit: the keys produce a refresh action only in browse mode and are plain text in the filter and forms; applying a refreshed inventory keeps the selection by identity, falls back to alias, and falls to the first host when the host vanished; the filter survives; a failed refresh changes nothing but the status; the footer advertises the key.
- PTY: on a real terminal, a host added to an included external config and a changed hostname are absent until `r` is pressed and visible afterwards; a broken managed file leaves the screen intact with a failure status.
