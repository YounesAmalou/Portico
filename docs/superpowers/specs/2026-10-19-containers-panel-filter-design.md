# Containers Panel Filter and Loading Design

**Status:** Approved; implemented

## Purpose

The TUI containers panel added in v0.3.0 follows the hosts selection and lists a host the first time the panel gains focus for it. Moving through the hosts therefore keeps retargeting the panel, and Tab both moves the focus and starts an `ssh`. This slice makes loading explicit and adds a filter inside the panel, so a long listing can be narrowed the way the hosts list already can. The CLI is unchanged.

## Decisions

### Loading

- **Start.** The panel shows this machine. Its containers are listed once when the TUI starts.
- **Tab and Shift+Tab** only move the focus between the hosts panel and the containers panel. They never list anything or change what the panel shows.
- **`c`** in the hosts panel shows the selected host's containers in the panel. The focus stays on the hosts, so the listing runs while the user keeps browsing; Tab then moves into the panel. `c` does nothing without a selected host. It is free in browse mode (`Ctrl+C` quits and is handled before any key routing, so `c` with Ctrl is left alone).
- **`L`** in the containers panel shows this machine again.
- **Moving the host selection** never changes the panel.
- **What `c` and `L` list.** Each target's result is kept for the session, as before. `c` and `L` start a listing when the target has not been listed yet or its last listing failed; a kept listing is shown as it is, and `r` in the panel lists it again. A listing already running for the target is left to finish. Starting a listing for another target still cancels the running one, which is forgotten (or its previous result restored).
- When nothing is listed (a cancelled first listing), the panel says `Not listed: r lists containers`.

### Filter

- **`/`** in the focused panel edits the filter. Typing narrows the rows at once; `Enter` applies and returns to the panel's keys; `Esc` while editing restores the filter as it was before the edit, as in the hosts filter.
- **`Esc`** in the panel clears an applied filter. With no filter it behaves as before: it cancels a running listing, otherwise moves the focus back to the hosts.
- **Matching.** A container matches when the filter is contained in any of: the name the panel shows, the container name, the Compose project, the Compose service, the image. Comparison ignores case and the accents of Latin letters, with the same folding as the jump host suggestions (`suggest::fold`). An empty filter matches everything.
- **Headers.** A Compose project header (or `Other`) is shown only when at least one of its containers matches.
- **Selection.** After the filter changes, the selection stays on the same container when it still matches, otherwise it moves to the first match. Up and Down move through matching rows only, and Enter and `n` open a shell in the selected matching container. Scrolling keeps the selected row visible as before.
- **No matches.** The panel shows `No containers match "<filter>".` and Enter does nothing.
- **Display.** The panel title reads `Containers: <target> /<filter>`, mirroring `Hosts /<filter>`, with a cursor while editing. The footer offers `Esc clear filter` when a filter is applied, and the filter-editing footer while editing.
- **Retargeting** with `c` or `L` to another target clears the filter, resets the selection and the scroll.

### Footers

- Hosts panel, with a selected host: `… c list containers  Tab containers  q quit`.
- Containers panel: `↑↓ choose  / filter[  Esc clear filter]  Enter shell  n new terminal  u user  r refresh  L this machine  Tab hosts`.
- Editing the panel filter: `type to filter  ←→ move  Enter apply  Esc cancel`.

## Out of scope

- Filtering on status, ports or other fields.
- Remembering the filter per target.
- Any CLI change.
