# Fuzzy Pickers Design

**Status:** Approved; implemented

## Purpose

Most CLI commands need a host alias, and today leaving it out is an error. Only `portico shell` asks, and only for a container: through `fzf` when it is on PATH and a numbered menu otherwise. `portico adopt` and `portico export` ask through a numbered list. The TUI filters its hosts and its containers by substring, and its adopt and export checklist cannot be filtered at all.

This slice gives Portico one built-in search, used everywhere a list is narrowed: a fuzzy matcher, and an inline picker on top of it that every command opens when it needs a host and none is given. The TUI filters use the same matcher, and the checklist gets a filter.

It implements v2 item 1 of `ROADMAP.md`, "fzf pickers across the CLI". `fzf` was the example, not the requirement: the picker is built in, so the behaviour is the same with or without `fzf` installed.

## Settled questions

- **Built in, not `fzf`.** An external `fzf` gives a good picker only to those who installed it, and a numbered menu to everyone else; multi-select would depend on `fzf --multi`. A picker drawn with ratatui, already a dependency, behaves the same for everyone, shares its matching with the TUI, and is tested in-process. `fzf` is no longer run anywhere.
- **No fallback menu.** The picker replaces the `fzf` call and every numbered menu. Without a terminal the commands need their arguments, as today.
- **No new dependency.** The matcher is a small function in Portico, not a crate.
- **The TUI keeps its order.** Ranking reorders rows; the TUI filters without re-sorting so that Compose groups stay together and the list does not jump while typing. Only the CLI single-select picker ranks.
- **Multi-select follows the TUI checklist.** Space marks, as in the `m` checklist, so the multi-select picker uses the checklist's keys and a `/` filter rather than type-to-search.

## Decisions

### Matcher

A function in `src/fuzzy.rs` that matches a query against one or more fields of a row and returns, for a match, a score and the matched character positions in each field.

- **Folding.** Query and text are compared through `suggest::fold`: case and the accents of Latin letters are ignored. Folding is applied per character, so matched positions always refer to characters of the original text, also when one character folds to two (`æ` to `ae`).
- **Terms.** Whitespace splits the query into terms. A row matches when every term matches. An empty query (or one of only whitespace) matches every row, with no positions.
- **Term match.** A term matches a field when its characters appear in the field in order, with gaps allowed (`prdb` matches `prod-db`). A term matches a row when it matches any of the row's fields; the best-scoring field counts.
- **Score.** Higher is better. Each matched character scores, with bonuses for the first character of the field, a character after a separator (`-`, `_`, `.`, `/`, `:`, `@`, space) or at a lower-to-upper case change, and a character directly following the previous matched one; a gap costs a little per skipped character. Among the possible alignments the best one counts. The row's score is the sum of its terms' scores. Ties keep the input order.
- **Purity.** No I/O; the same inputs give the same output.

### Picker

An inline picker in `src/picker.rs`, used by the CLI only.

- **Drawing.** It draws on stderr in an inline ratatui viewport under the prompt, never the alternate screen, so stdout stays clean. The viewport is at most 12 lines, fewer when the list is shorter or the terminal is smaller (at least the query line and one row). It redraws on resize and erases itself when it closes. Raw mode is always left, also on an error or a panic.
- **Rows.** Each row has the fields that are searched and extra columns that are shown dimmed and not searched. Matched characters are highlighted. The selected row is highlighted in full.
- **State and terminal apart.** The picker's state (query or filter, ranking, cursor, scroll, marks) is a plain type that takes key events and returns an outcome; the terminal loop around it only reads events and draws. Tests drive the state directly.
- **Interface.** `src/cli.rs` receives the picker through a small trait (pick one row, pick several rows, each returning the chosen rows or cancelled), so CLI tests pass a scripted picker. The real program passes the terminal picker.
- **Cancelled.** A cancelled pick exits with status 130 and runs nothing, as `portico shell` does today.
- **Terminal failure.** When the terminal cannot be prepared (raw mode or the viewport fails), the command fails with that reason.

#### Single select

Type-to-search, for every command that needs one host or one container.

- The first line reads `<prompt>> <query>` with the number of matches and rows, for example `host> pr  3/42`.
- Rows are ranked by score, best first; the cursor starts on the first row and returns to it when the query changes.
- Keys: characters edit the query; `Backspace` and `Delete` erase, `←` `→` `Home` `End` move the cursor in the query; `↑` `↓`, `Ctrl+P` `Ctrl+N` move the selection; `PgUp` `PgDn` move by a page; `Enter` picks the selected row (nothing when no row matches); `Esc` and `Ctrl+C` cancel. Paste goes into the query.
- No matches shows `No match.` in place of the rows.

#### Multi select

The `m` checklist's keys, for `adopt` and `export`.

- Rows keep their input order. Each shows a mark box (`[x]` or `[ ]`).
- Keys: `↑` `↓`, `j` `k` move; `Space` marks or unmarks the selected row; `a` marks every visible row, or unmarks them when all are marked; `/` edits the filter; `Enter` returns the marked rows (nothing when none is marked); `Esc` clears an applied filter, otherwise cancels; `Ctrl+C` cancels.
- Editing the filter: characters edit it and narrow the rows at once; `Enter` applies it and returns to the keys above; `Esc` restores the filter as it was before the edit. These are the hosts filter's keys.
- Marks survive filter changes. `Enter` returns every marked row, also those the filter hides. The first line shows the filter and the marks, for example `adopt  /pr  2 marked`, so marks hidden by the filter are visible.

### CLI

| Command | Without its alias | Rows |
|---|---|---|
| `connect`, `test`, `show` | Single-select picker, prompt `host` | Every host |
| `edit`, `remove` | Single-select picker, prompt `host` | Managed hosts |
| `containers --on`, `shell --on` | A bare `--on` opens the host picker; no `--on` is still this machine | Every host |
| `shell` without `<container>` | Single-select picker, prompt `container` (after the host picker for a bare `--on`) | Running containers |
| `adopt`, `export` without aliases and `--all` | Multi-select picker | Eligible hosts |

- **Arguments.** The alias of `connect`, `test`, `show`, `edit` and `remove` becomes optional. `--on` takes an optional value: `--on <alias>` as before, a bare `--on` asks. Every other flag keeps its meaning and applies to the chosen host or container (`edit -u root` with no alias picks the host and changes its user).
- **Host rows.** Searched: the alias, as in the TUI hosts filter. Shown dimmed: `managed` or `external`, and the source file, as `portico hosts` prints them.
- **Container rows.** Searched: the name Portico shows, the container name, the Compose project, the service and the image, as in the TUI containers filter. Shown dimmed: the project (or `Other`) and the status.
- **Adopt and export rows.** Searched: the alias. Shown dimmed: where the host is declared, as the numbered list shows it today. Hosts that are not eligible are printed with their reasons before the picker opens, as today.
- **After the pick** every command continues exactly as if the alias had been typed: `remove` still asks to confirm, `connect -n` and `shell -n` open the new terminal after the pick, and `adopt` and `export` still show their preview and ask.
- **Nothing to pick.** With no host (or no managed host for `edit` and `remove`, or no eligible host for `adopt` and `export`) the command says so and exits without opening the picker; `adopt` and `export` keep today's `No host can be <adopted|exported>.` and success status, the others fail with `No hosts.` or `No managed hosts.`, and `shell` keeps `No running containers on <target>.`. A single row still opens the picker: nothing is chosen for the user.
- **No terminal.** When stdin, stdout or stderr is not a terminal (the picker draws on stderr and asks the cursor position through stdout) and the alias is missing, the command fails naming what to pass: `choose a host: portico <command> <alias> (no terminal for the picker)`, `choose a host: portico <command> --on <alias> (no terminal for the picker)`, the existing message for `shell` without a container, and the existing `requires host aliases or --all` message for `adopt` and `export`.
- **Removed.** The `fzf` program in `ContainerPrograms`, its lookup and its tests, and the numbered menus of `shell`, `adopt` and `export`.

### TUI

- **Hosts filter.** Matches the alias with the fuzzy matcher instead of a substring, and highlights the matched characters. Keys, title, selection rules and order are unchanged.
- **Containers filter.** Matches the same fields as today with the fuzzy matcher instead of a substring, and highlights the matched characters in the name shown. Keys, headers, selection rules and order are unchanged.
- **Checklist filter.** The `m` checklist gets a filter with the hosts filter's keys: `/` edits it, typing narrows the rows at once, `Enter` applies, `Esc` while editing restores the previous filter, and `Esc` with an applied filter clears it (otherwise it closes the checklist, as today). Rows that are not eligible are filtered like the others.
  - Marks survive filter changes; `a` marks the visible eligible rows, or unmarks them when all are marked, as in the CLI checklist; `Enter` previews every marked host, also hidden ones.
  - The cursor stays on the same row when it still matches, otherwise it moves to the first match; `j` `k` move through matching rows only.
  - The title shows `/<filter>` and the number of marked hosts. No matches shows `No hosts match "<filter>".`
  - The footer offers `/ filter` and, with an applied filter, `Esc clear filter`; while editing it shows the filter-editing footer.

### Documentation

- The README describes the picker and its keys, the optional aliases, the bare `--on` and the checklist filter, and drops `fzf` from the optional dependencies.
- The container shells design notes that its CLI picker is replaced by this design.
- `ROADMAP.md` drops the item when the slice ships, and the v1 design lists it under its delivered slices.

## Code layout

- **`src/fuzzy.rs`:** the matcher.
- **`src/picker.rs`:** the picker state, its rendering, the terminal loop and the trait the CLI uses.
- **`src/cli.rs`:** optional aliases and `--on`, the rows per command, the no-terminal errors; the `fzf` call and the numbered menus removed.
- **`src/main.rs`:** passes the terminal picker to `cli::run`.
- **`src/app/containers.rs`:** `fzf` removed from `ContainerPrograms`.
- **`src/tui.rs`, `src/tui/containers.rs`, `src/tui/migrate.rs`:** the fuzzy filters, the highlighting and the checklist filter.

## Out of scope

- Ranking in the TUI.
- Searching host fields other than the alias (hostname, user), which would need every host resolved first.
- Remembering queries, a history of picks, or preselecting the last pick.
- Smart case or exact-match syntax (`'term`, `^term`).
- Using `fzf` or reading its configuration.
- A picker for values inside commands, such as the ProxyJump of `add` and `edit`.

## Testing

- **Matcher:** in-order subsequence matching; several terms, each required; matching across fields; folding of case and accents, with positions in the original text; ranking (a field start beats a middle, a separator start beats a middle, consecutive beats scattered, ties keep order); the empty query.
- **Picker state:** single select ranking, cursor movement and paging, query editing, `Enter` with no match, cancelling with `Esc` and `Ctrl+C`; multi select marking, `a`, the filter keys, marks surviving the filter and returned when hidden, `Esc` clearing then cancelling.
- **Picker rendering:** a ratatui test backend shows the prompt line, counts, highlighted matches, dimmed columns and the height limit.
- **CLI, with a scripted picker:** each command without its alias picks and runs as with the alias; a bare `--on` for `containers` and `shell`, then the container picker; managed-only rows for `edit` and `remove`; eligible-only rows for `adopt` and `export`; cancelling exits 130 and changes nothing; nothing to pick; every no-terminal error.
- **TUI:** fuzzy matching and highlighting in the hosts and containers filters with order kept; the checklist filter keys, marks across filters, `a` on visible rows, `Enter` with hidden marks, the title and the footers.
- **End to end:** one picker run through a real terminal when the e2e harness can drive a pty; otherwise the scripted CLI tests are the coverage.
