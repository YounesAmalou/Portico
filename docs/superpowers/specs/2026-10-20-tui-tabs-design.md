# TUI Tabs Design

**Status:** Draft; awaiting review

## Purpose

The TUI has grown three surfaces with three ways in and out: the hosts screen, a containers panel squeezed under the hosts list (`c` retargets it, Tab moves the focus into it), and a full-screen tunnels view (`T` opens it, `T` or Esc closes it). Each has its own idea of what Tab, Esc and `T` do, and the containers panel gets only two fifths of the left column. This slice replaces them with three tabs, **Hosts | Tunnels | Containers**, switched with `1` `2` `3` or Tab, each keeping its own state and using the whole screen.

It also gives the tunnel form's Host field the visible suggestion dropdown the host form's ProxyJump field has, which the tunnels design left as a hidden Tab completion. The CLI is unchanged.

## Settled questions

- **Three tabs, in this order: Hosts, Tunnels, Containers.** Hosts is where the TUI opens. Tunnels come second because they are global (no target to choose) and Containers third because it depends on a target, usually a host picked on the first tab.
- **What replaces Tab-to-focus-containers.** Tab and Shift+Tab now cycle the tabs (Hosts → Tunnels → Containers → Hosts, and back). The containers panel leaves the hosts screen entirely and becomes the Containers tab, so there is no panel focus any more: `Focus` and the left-column split are removed, and the hosts list takes the full height of the left column.
- **Where tab keys work.** `1` `2` `3`, Tab and Shift+Tab switch tabs only where the active tab is browsing: the hosts list, the tunnels table, the containers list. Everywhere else they keep their current meaning:
  - text inputs (the hosts, tunnels and containers filters, the containers user field, the host and tunnel forms, the containers target picker) take digits as text;
  - in forms Tab and Shift+Tab keep moving between fields, and in an open suggestion dropdown Tab keeps picking;
  - questions that wait for an answer (remove confirmations, the trust prompt, the adopt and export checklist) ignore digits and Tab, so an open question is never hidden in a tab the user has left. The tunnel form's Kind row is part of a form and is treated the same way.
- **Aliases.** `T` opens the Tunnels tab and `c` the Containers tab, from any tab that is browsing. `c` on the Hosts tab with a host selected also sets the Containers target to that host (below); from the other tabs, and without a selection, it switches without changing the target. `T` and `c` on their own tab do nothing. Esc never switches tabs: it cancels or clears within the tab (the tunnels table's and the containers panel's "Esc back to the hosts" are removed).
- **The Containers target** is shown on the first line of the tab, `Target: prod-db` or `Target: this machine`, with the hint `h change  L this machine`. `h` opens the existing fuzzy picker (`picker::Single`, the one `portico shell` and `connect` use) drawn over the tab, listing `this machine` first and then every host alias with its source dimmed (`managed`, or the external file). Typing ranks with the shared fuzzy matcher, Up and Down choose, Enter picks and Esc closes without changing anything. `L` still goes straight back to this machine, and `c` on the Hosts tab still picks the selected host.
- **Lazy loading is kept, and made lazier.** A target is listed when it becomes the target while the Containers tab is shown (`h`, `L`, or `c`, which also shows the tab), or when the tab is first shown for a target that was never listed. Nothing is listed at start-up any more: today this machine is listed when the TUI starts because the panel is visible; now it is listed the first time the Containers tab is opened. Every target's result is kept for the session as before, a failed listing is retried on the next show, and `r` or F5 lists again. Hosts are never listed up front, and switching tabs never cancels a running listing.
- **Each tab keeps its state.** The tabs are the existing `UiState` (hosts), `TunnelsScreen` and `Panel`, each with its selection, filter, scroll and open form or prompt; switching only changes which one receives keys and is drawn. A listing, a connection test or a tunnel start that finishes on a hidden tab updates that tab, which shows the result when it comes back.
- **Refresh.** `r` and F5 refresh the active tab only: the Hosts tab re-reads the inventory (unchanged), the Tunnels tab re-reads the tunnels and their statuses (unchanged), and the Containers tab lists its target again (F5 is new there). The tunnels statuses are re-read every 2 seconds while the Tunnels tab is shown, and on switching to it, as when the screen opened; nothing is re-read for a hidden tab.
- **Quitting.** `q` quits from any browsing tab, Ctrl+C from anywhere, and running tunnels keep running. The Tunnels tab keeps its bottom border `running tunnels keep running after q` while any tunnel runs.
- **Status line.** Each tab has its own status line (the hosts and containers results today share `UiState::status`; the containers shell and fix results move to the panel). A result arriving for a hidden tab waits there.
- **Prompts on a hidden tab.** A host test whose server presents an unknown key opens the trust prompt only when the Hosts tab is shown and browsing, as it already requires browse mode; otherwise the host is marked untrusted and `t` tests it again, which is today's "you have moved on" rule.
- **Tunnel form Host dropdown.** The Host field gets the ProxyJump dropdown: it opens while a value is typed and lists the known aliases that match, ranked by `suggest::rank`, with their origins (managed, or the external file) as in the host form; Ctrl+Space opens it on demand, even empty; Up and Down choose; Enter or Tab fills the field with the chosen alias and closes it; Esc closes it (a second Esc cancels the form). An exact match closes it unless it was opened with Ctrl+Space. With the dropdown closed, Tab moves to the next field: the hidden "Tab completes the best match" goes away. The field holds one alias, so the whole value is the query (no hops). The footer reads `↑↓ choose  Enter pick  Esc close` while it is open, and offers `Ctrl+Space suggest` on the Host row.
- **Reuse, not a copy.** The dropdown's state (`Suggestions`) and drawing (`suggestion_lines`) move from `src/tui.rs` into a small `src/tui/dropdown.rs` shared by both forms; ranking stays `suggest::rank`. The host form's behaviour does not change.

## Decisions

### The tab bar

One line at the top of every tab: the three tabs with their numbers, the active one reversed and bold, the others plain. Nothing else is on the line, so it never wraps.

```text
 1 Hosts │ 2 Tunnels │ 3 Containers
```

Below it each tab draws its own content, status line and footer, as the screens do today. The footer is the active tab's; the inventory warnings stay on the Hosts tab.

### Hosts tab

The hosts screen as it is, minus the containers panel: the hosts list fills the left column, the details, forms, confirmations, trust prompt and adopt and export checklist the right one.

```text
 [1 Hosts] │ 2 Tunnels │ 3 Containers
┌Hosts /─────────────────────┐┌Details───────────────────────────────────────────────┐
│✓ bastion  managed          ││alias: prod-db                                        │
│> prod-db  managed          ││hostname: 10.0.0.12                                   │
│  staging  ~/.ssh/config    ││user: deploy                                          │
│                            ││port: 22                                              │
│                            ││proxyjump: bastion                                    │
│                            ││                                                      │
│                            ││identities:                                           │
│                            ││  ✓ ~/.ssh/id_ed25519  in agent                       │
└────────────────────────────┘└──────────────────────────────────────────────────────┘
↑↓ move  / filter  a add  m export  e edit  x remove  r refresh  t test  Enter connect
n new terminal  c containers  Tab/1-3 tabs  q quit
```

### Tunnels tab

The tunnels screen as it is, under the tab bar.

```text
 1 Hosts │ [2 Tunnels] │ 3 Containers
┌Tunnels /───────────────────────────────────┐┌Details─────────────────────────────────┐
│> db       prod-db  -L 5432:localhost:5432  ││name: db                                │
│  running                                   ││host: prod-db                           │
│  socks    bastion  -D 1080       stopped   ││forward: -L 5432:localhost:5432         │
│                                            ││status: running (pid 41237, 12m)        │
│                                            ││ssh -f -N -o ExitOnForwardFailure=yes … │
└ running tunnels keep running after q ──────┘└────────────────────────────────────────┘
↑↓ move  / filter  s stop  a add  e edit  x remove  r refresh  Tab/1-3 tabs  q quit
```

The tunnel form, with the Host dropdown open after typing `pr`:

```text
┌Add tunnel──────────────────────────────────────────┐
│  Name: db                                          │
│> Host: pr                                          │
│        ┌──────────┐                                │
│        │> prod-db │  managed                       │
│        │  preview │  ~/.ssh/config.d/work          │
│        └──────────┘                                │
│  Kind: local                                       │
│  Bind address: localhost                           │
│  Bind port: 5432                                   │
│  …                                                 │
└────────────────────────────────────────────────────┘
↑↓ choose  Enter pick  Esc close
```

### Containers tab

The containers panel at full size: the target line, the grouped list with its filter in the title and the user line at the bottom on the left, the selected container's details on the right.

```text
 1 Hosts │ 2 Tunnels │ [3 Containers]
Target: prod-db   h change  L this machine
┌Containers /───────────────────────────────┐┌Container───────────────────────────────┐
│shop                                       ││name: shop-web-1                        │
│> web           nginx:1.27   Up 3 hours    ││image: nginx:1.27                       │
│  db            postgres:16  Up 3 hours    ││status: Up 3 hours                      │
│Other                                      ││ports: 0.0.0.0:80->80/tcp               │
│  watchtower    containrrr/… Up 2 days     ││project: shop                           │
│                                           ││service: web                            │
│user: image default                        ││user: image default                     │
└───────────────────────────────────────────┘└────────────────────────────────────────┘
↑↓ choose  / filter  Enter shell  n new terminal  u user  h target  L this machine
r refresh  Tab/1-3 tabs  q quit
```

The target picker after `h`, drawn over the list:

```text
 1 Hosts │ 2 Tunnels │ [3 Containers]
Target: prod-db   h change  L this machine
┌Containers on─────────────────────────────────────────────────────────────────────────┐
│pro> pro                                                                         3/4  │
│> prod-db  managed                                                                    │
│  preview  ~/.ssh/config.d/work                                                       │
│  this machine                                                                        │
└──────────────────────────────────────────────────────────────────────────────────────┘
type to search  ↑↓ choose  Enter pick  Esc cancel
```

Failures (Docker missing, permission denied with the `g` and `s` fixes, ssh errors) and the "Not listed: r lists containers" and "No containers match" lines are shown in the list as today, wrapped to its now larger width.

### Keys

Global, while the active tab is browsing:

| Key | Effect |
|---|---|
| `1` `2` `3` | Show the Hosts, Tunnels or Containers tab |
| Tab, Shift+Tab | Show the next or previous tab, wrapping around |
| `T` | Show the Tunnels tab |
| `c` | Show the Containers tab; on the Hosts tab with a host selected, target that host |
| `r`, F5 | Refresh the active tab |
| `q` | Quit (running tunnels keep running) |
| Ctrl+C | Quit, from anywhere (unchanged) |

Per tab, the existing keys are kept, except:

- **Hosts:** Tab no longer moves into the containers panel, `T` and `c` are the aliases above.
- **Tunnels:** `T` and Esc no longer return to the hosts; Esc still clears an applied filter and cancels a start that is connecting.
- **Containers:** Tab and Esc no longer return to the hosts; Esc still cancels a listing and clears an applied filter. `h` opens the target picker, `L` targets this machine, F5 joins `r`.

### Footers

- **Hosts:** today's footer with `c list containers  Tab containers  T tunnels` replaced by `c containers  Tab/1-3 tabs` (and the no-selection variant likewise).
- **Tunnels:** `T/Esc hosts` replaced by `Tab/1-3 tabs`.
- **Containers:** `↑↓ choose  / filter[  Esc clear filter]  Enter shell  n new terminal  u user  h target  L this machine  r refresh  Tab/1-3 tabs  q quit`.
- **Target picker:** `type to search  ↑↓ choose  Enter pick  Esc cancel`.
- Footers of text inputs, forms and prompts are unchanged, except the tunnel form's Host row (`Ctrl+Space suggest`, and the dropdown footer while it is open).

## Code layout

- **`src/tui.rs`:** a `Tab` enum (`Hosts`, `Tunnels`, `Containers`) and `UiState::tab`; `Mode::Tunnels` and `Focus` go away. `handle_key` and `handle_paste` route to the active tab, after a single `tab_key` step that handles the global keys only when the active tab reports that it is browsing (`UiState` in `Mode::Browse`, `TunnelsScreen::is_browsing`, `Panel::is_browsing`). `render` draws the tab bar and then the active tab. The event loop reloads the tunnels on switching to their tab, lists the containers target when its tab is shown and the target is unlisted, and stops listing local containers at start-up. `UiAction::OpenTunnels` becomes a tab switch. The `Suggestions` struct and `suggestion_lines` move out.
- **`src/tui/dropdown.rs` (new):** `Suggestions` and `suggestion_lines`, moved unchanged apart from visibility, used by both forms.
- **`src/tui/tunnels.rs`:** `render` draws into a given area and leaves the status and footer to the caller like the other tabs; the `Leave` action goes; the form's Host field uses the dropdown with the inventory's aliases and origins as `Candidate`s; the hidden Tab completion (`host_matches`) goes.
- **`src/tui/containers.rs`:** the panel draws at full size with the target line, owns its status line, holds the target picker (`picker::Single`) while it is open, and gains `h` and F5; `Leave` goes and its doc comment stops saying "under the hosts".
- **`README.md`:** the TUI keys section is reorganised by tab: a short "Tabs" table with the global keys, then the hosts, tunnels and containers tables, with Tab, `T`, Esc and the containers loading paragraph updated, and the tunnel form paragraph describing the Host dropdown.
- **Docs in the same pull request:** this spec becomes "Approved; implemented", and the v1 design's "Delivered follow-up slices" gains a "TUI tabs" bullet.

## Out of scope

- Mouse support for the tab bar.
- More tabs (identities, a log), reordering or hiding tabs, and remembering the active tab between sessions.
- Changing what any tab lists or how it lists it, beyond the start-up listing of this machine.
- `portico tui` arguments to open on a given tab.

## Testing

- **Unit, `src/tui.rs` (as the existing TUI tests):**
  - `1` `2` `3`, Tab and Shift+Tab switch tabs from each browsing tab, wrapping; `T` and `c` switch; `c` with a selected host retargets and asks to list it once, and a second `c` for the same host does not list again.
  - Digits are typed, not switched on, in the hosts filter, a host form, the tunnels filter, a tunnel form, the containers filter and user field, and the target picker; Tab moves between form fields and picks in an open dropdown; a remove confirmation, the trust prompt and the adopt and export checklist ignore digits and Tab.
  - Each tab keeps its selection and filter across a round trip through the others.
  - `r` and F5 produce the active tab's refresh only; `q` quits from each tab; Esc never switches.
  - Opening the Containers tab lists an unlisted target once and a kept listing not at all; nothing is listed before it is opened.
  - A trust prompt does not open while another tab is shown.
  - The tab bar renders with the active tab highlighted, and each tab's footer matches the text above (rendered through `TestBackend`, as the existing render tests).
- **Unit, `src/tui/containers.rs`:** the target line, `h` opening the picker, picking a host or this machine, Esc leaving the target alone, F5 listing again, and the full-size layout.
- **Unit, `src/tui/tunnels.rs`:** the Host dropdown opens on typing, opens empty with Ctrl+Space, ranks managed before external duplicates as `suggest::rank` does, closes on an exact match, Enter and Tab pick, Esc closes and a second Esc cancels, Tab without a dropdown moves on, and the footer; the existing tests adjusted for `Leave` going away.
- **PTY acceptance (`tests/support/tui_pty.py`, a new scenario run from `tests/acceptance.rs`):** on a real terminal, the tab bar is drawn, `2` shows the tunnels table, `3` shows the containers tab and lists this machine (a fake `docker`), Tab wraps back to Hosts, `/` then `2` types a `2` into the hosts filter, and `q` quits.
- The existing tests that use Tab to focus the containers panel or `T`/Esc to leave the tunnels screen are rewritten for the tabs.

## Questions for the maintainer

1. **`c` on the Tunnels tab.** As settled above it only switches to the Containers tab. It could instead target the selected tunnel's host, which is handy for "what is running behind this forward" but makes `c` mean three things. Proposed: keep it a plain switch.
