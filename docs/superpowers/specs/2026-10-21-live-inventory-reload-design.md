# Live Inventory Reload Design

**Status:** Approved; implemented

## Purpose

The TUI reads the host inventory at startup, after its own changes, and when `r` or F5 is pressed (`2026-10-14-tui-refresh-design.md`). An edit made elsewhere, in `~/.ssh/config`, in an included file or in the managed file by another `portico`, stays invisible until the user thinks of refreshing. This slice makes the hosts list follow the config files by itself. It delivers the "Live inventory reload when the config changes" production-readiness item of `ROADMAP.md`, and changes nothing in the CLI or the files Portico writes.

It also gives the TUI's event loop a clear shape it has lacked: one source of input events and periodic ticks, which background work (the config watch among it) hangs off, independent of the temporary closed-terminal workaround in `src/tui/hangup.rs`.

## Settled questions

- **Polling, not notifications.** The TUI checks the files' metadata on its own loop. No inotify, no `notify` crate, no thread of its own: it fits the rule of no daemon, works the same on every filesystem (network homes, bind mounts, editors that save by renaming), and costs a handful of `stat` calls a second. A future macOS or Windows port needs nothing new.
- **What is watched.** Everything the inventory was read from, as the read itself reports it, so the list can never drift from what discovery follows:
  - the entry config (`~/.ssh/config`);
  - every file the discovery scan read, through any depth of `Include`, by the path it was opened under (so a symlink retargeted by a dotfile manager is noticed too);
  - each `Include` pattern: a literal one by its path (an included file that does not exist yet is noticed when it appears), a glob by the sorted list of paths it matches, with discovery's own matching rules (a wildcard never matches a leading dot). A glob is never judged by its directory's metadata: a directory changes whenever anything in it does, so an editor's swap file beside an included file, or `known_hosts` rewritten in `~/.ssh` under `Include */config`, would reload the list for nothing. Each matched file is a watched file of its own, so its contents are followed too;
  - the managed file (`~/.ssh/portico/config`), so a host added by `portico add` in another terminal appears.
  Discovery records these inputs while it scans (`Discovery::inputs`, each a `File` or a `Glob`), and `App::hosts` passes them on with the managed file (`HostInventory::inputs`). The set is recomputed by every read, so a newly added `Include` is followed and a removed one is dropped.
- **What counts as a change.** For a glob, a different list of matches. For each file: whether it exists, its device and inode, size, and modification and status-change times in nanoseconds. Any difference is a change. The inode catches an editor that renames a new file into place even within one timestamp tick; the status-change time catches a `cp -p` that preserves the modification time. A path that disappears for a moment (an editor deleting and recreating it) is judged by what is there at the next check: a recreated file is a new inode, so it counts as changed. A path that cannot be read (permission denied) counts as absent, so the next read reports the error.
- **When it checks.** Every second while the TUI runs. A check that finds a difference does not reload yet: it looks again 300 ms later, and reloads only once two checks in a row see the same new state. An editor's save (write a temporary file, rename it, fix permissions) or a script touching several files therefore causes one reload, not several, and a file is not read half-written. The worst-case delay from save to screen is about 1.3 seconds plus the read itself.
- **The same reload as `r`.** A change found by the watch goes through the refresh key's path: identities and agents are asked again, the inventory is re-read, cached host details are discarded, host tests in flight are cancelled, the selection stays on the same host (by identity, then by alias) and the filter is kept (all as `2026-10-14-tui-refresh-design.md` specifies). Only the status wording differs: `Config changed: reloaded N hosts` instead of `Refreshed: N hosts`, and `Config changed, reload failed: <error>` instead of `Refresh failed: <error>`, so a message the user did not ask for says why it appeared.
- **Never under an open form.** The reload is applied only where `r` would be accepted: the hosts screen in browse mode. While a host form, the filter, a remove confirmation, the trust prompt or the adopt and export checklist is open, the list is not touched; the status line shows `Config changed: the list reloads when this closes` (unless a message of its own is showing), and the reload is applied on the first tick after it closes. Saving a form re-reads the inventory anyway, which includes the external change, so no second reload follows. The other tabs are in browse mode for the hosts screen; a reload there updates the hidden hosts list, which shows the result when the user comes back.
- **Portico's own writes.** Every read of the inventory, whatever started it (startup, `r`, a watch-triggered reload, the refresh after an add, edit, remove, adopt or export, or after a failed mutation that may have touched the managed file), restarts the watch from the files as they are right after the read. Portico's own changes are complete by then, so they never cause a reload or a message. A read that fails restarts the watch too, from the previous set of paths: the files are not read again until they change again, instead of failing every second.
- **Accepted window.** A change that lands between the end of a read and the moment the watch records the files (microseconds) is taken as already read. It shows on the next change or the next `r`. Closing it would need timestamps compared against the clock, which coarse filesystem clocks make unreliable.
- **What is not watched.** What `ssh -G` resolves can depend on more than these files (environment, `Match exec`, files named by tokens Portico does not expand), and agents and key files are not files of the config. `r` stays for those, and remains the way to force a re-read. Include patterns that discovery cannot expand (an unset variable, `%` tokens) are not watched; discovery already warns about them.
- **The tunnels and containers tabs** are unchanged: tunnels keep their 2-second status refresh while shown, and containers are listed on demand.

## Decisions

### The event loop: input and ticks

`src/tui/events.rs` owns how the TUI waits. `Events::next(every)` returns one `Step`:

- `Step::Input(event)` when the terminal sends something before the next tick is due;
- `Step::Tick(now)` when `every` has passed since the previous tick, even while input keeps arriving, so background work is never starved by a held key;
- `Step::Closed` when the terminal is gone.

The loop polls background work only on ticks: resolved host details, test results, container listings, adoption plans, identity indexes, tunnel starts, the tunnels tab's periodic re-read, and the config watch. Ticks come every 25 ms while a worker is busy, so results still appear promptly, and every 100 ms otherwise. Input is handled as it arrives, as before. The initialization prompt reads the same events and ignores ticks.

`Events` reads the terminal through one function, today `hangup::next_event`, which turns a closed terminal into `Closed`. When crossterm fixes its read loop and `hangup.rs` goes away, that function becomes crossterm's `poll` and `read`; nothing that hangs off ticks changes. `Events` takes its terminal reader and clock as parameters, so its unit tests drive it without a terminal or sleeps.

### The config watch

`src/tui/watch.rs` holds `ConfigWatch`, a small state machine that is told the time and asks an injected `Metadata` for each file's stamp and each glob's matches (the real one calls `fs::metadata`, following symlinks, and discovery's `include_matches`):

- `ConfigWatch::new(inputs, now)` and `rebase(inputs, now)` record what the inputs look like now as read and schedule the next check a second later;
- `tick(now)` does nothing until a check is due. A due check looks at every input: unchanged, it schedules the next check in a second; different from what was read and from the previous check, it notes the new stamps and looks again in 300 ms; different but equal to the previous check, the change has settled and the watch reports it. It returns whether a settled change is waiting;
- once a change has settled the watch stops checking: it has nothing more to learn until the inventory is read again and `rebase` is called. A change that is undone before it settles is forgotten.

The TUI owns one `ConfigWatch` in its loop. On each tick it calls `tick(now)`; when a change is waiting and the hosts screen is browsing, it calls the refresh path with the "config changed" wording; otherwise it sets the state's waiting indicator, which the next tick clears once the reload is done. `UiState` keeps the inventory's `inputs` next to its hosts and warnings, so whichever code replaces the inventory also knows what to watch.

## Testing

- Unit (`watch.rs`, fake metadata and explicit instants, no sleeps): no check before a second has passed; an unchanged file never reports; a change reports only after it is stable for 300 ms; a file that keeps changing does not report until it stops; a change undone before it settles is forgotten; a deleted file, a created one and one replaced by rename (same times, new inode) are changes; an unreadable path counts as absent; `rebase` forgets a pending change and follows the new set of paths; a settled change stays reported until `rebase`.
- Unit (`events.rs`, fake reader and clock): input before the tick is passed on; a quiet wait ends in a tick; a tick due while input keeps arriving is delivered first; a closed terminal is `Closed`; reader errors propagate.
- Unit (`discovery.rs`): `inputs` lists the entry, every file read by its opened path, a literal include that does not exist, and each glob, each once; `include_matches` leaves out dotfiles and files that do not match, also under a wildcard directory. Unit (`watch.rs`): a file appearing beside a glob's matches without matching (a swap file) is no change, while a new match and a match that goes away are. Unit (`app.rs`): the inventory's inputs start with the entry config and name the managed file once.
- Unit (`tui.rs`): the watch-triggered reload uses its own status wording, keeps the selection and filter, and a failure leaves the list; the waiting indicator shows in the status line only when no other message does.
- PTY (`tests/support/tui_pty.py`, scenario `live_reload`): with the TUI running and no key pressed, a host appended to `~/.ssh/config` appears; an `Include` of a new file appears with its host; that file rewritten by rename shows its new host; with the add form open, an external change does not reach the list and the indicator shows, and leaving the form applies it; a host saved from the TUI leaves its `Saved` status in place, with no reload message after it. The existing `refresh` scenario keeps testing `r` for what the watch cannot see (the `ssh -G` answer) and the failure message.
