# Jump Host Suggestions Design

**Status:** Draft

## Purpose

Typing a ProxyJump value today means remembering the exact alias of the jump host. This slice makes the ProxyJump field of the TUI add and edit forms suggest the aliases Portico already knows, managed and external, as the user types, in a small dropdown under the field. Picking one fills in the hop. Free text stays allowed, and the value is still validated by OpenSSH on save exactly as typed text is.

It implements the "Jump host suggestions" slice of `ROADMAP.md` (v2). The CLI is unchanged.

## Decisions

### What is matched

- **The current hop.** ProxyJump takes several hops separated by commas. Suggestions match the hop the cursor is in: the text between the commas around the cursor. When that hop has a `user@` part, only the text after the `@` is matched, and the `user@` is kept on a pick. When a `:` follows the host part (a port is being typed), the hop gets no suggestions.
- **Candidates.** Every alias in the TUI's host inventory, managed and external. Wildcard patterns are already skipped by discovery. An alias listed by several files appears once; the managed entry is preferred, otherwise the first one in inventory order.
- **Exclusions.** The host being edited (in an add form, the value of the Alias field) and every alias already used by another hop of the same field.
- **Ranking.** A candidate matches when it contains the query, ignoring case. Candidates that start with the query come first, then the rest; each group is sorted alphabetically.
- **Each row** shows the alias and its origin: `managed`, or the source file of an external host with `$HOME` shown as `~`.

### When the dropdown opens and closes

- It **opens** after the user types, pastes or deletes text in the ProxyJump field, when the current hop's query is not empty and at least one candidate matches. It never opens just because ProxyJump gains focus.
- **Ctrl+Space** opens it on demand, also with an empty hop, listing every candidate. An empty hop then keeps it open until it is closed by one of the rules below.
- It **closes** when no candidate matches, when the hop becomes empty (unless it was opened with Ctrl+Space), when the hop is exactly equal to a candidate's alias, after a pick, on Esc, when focus leaves ProxyJump, and when the form is saved or tested.
- If the host inventory is replaced while it is open, it is recomputed against the new inventory.

### Keys while it is open

| Key | Effect |
|---|---|
| Up, Down | Move the selection (no wrap) |
| Enter, Tab | Replace the current hop with the selected alias, put the cursor at the end of that hop, close the dropdown. Neither saves nor changes field |
| Esc | Close the dropdown only; a second Esc cancels the form as today |
| Anything else | As today (editing, Left/Right, Ctrl+T, Ctrl+O); after an edit the dropdown is recomputed |

When it is closed, every key behaves exactly as today.

### Layout

- The dropdown is drawn as boxed lines directly under the ProxyJump row, aligned with the value column, with the selected row marked `>`; the origin is shown beside each alias. ProxyJump is the last field, so only the hint, error and test-result lines below it move down.
- At most 5 rows are visible; the list scrolls to keep the selection visible and starts on the first match.
- While it is open the form footer reads `↑↓ choose  Enter pick  Esc close` instead of the usual form keys. The usual footer advertises `Ctrl+Space suggest` when ProxyJump is focused.

## Code layout

- `src/suggest.rs` (new, pure):
  - `Candidate { alias: String, origin: Origin }` with `Origin::{Managed, External(PathBuf)}`.
  - `current_hop(value: &str, cursor: usize) -> Option<Hop>`: the byte range of the hop's alias part, or `None` when a port follows it.
  - `rank(query: &str, candidates: &[Candidate], exclude: &[&str]) -> Vec<Candidate>`: dedupe, exclusions, case-insensitive contains, prefix matches first, then alphabetical.
  - `apply(value: &str, hop: &Hop, alias: &str) -> (String, usize)`: the new value and cursor.
- `src/tui.rs`:
  - `FormState` gains `suggestions: Option<Suggestions>` (items, selected index, scroll offset, whether it was opened with Ctrl+Space).
  - `handle_form_key` handles an open dropdown's keys before its existing match, and opens it on Ctrl+Space.
  - The "text changed" handling shared by key input and paste recomputes the dropdown (and removes the paste path's duplicate of that logic).
  - Focus changes, save, test and Esc close it; `replace_hosts` recomputes an open one.
  - `form_lines` draws it after the ProxyJump row; `footer_text` shows its keys.
- `README.md`: Ctrl+Space and a sentence on the dropdown in the form keys. `ROADMAP.md`: the slice removed and the rest renumbered. `CHANGELOG.md` comes from the `feat:` commit.

## Out of scope

- Suggestions in any other field, or in the CLI.
- Checking that a typed jump host exists; free text stays valid.
- Mouse support.
- A general completion widget: `suggest.rs` can be reused if a second field needs one.

## Testing

- **Matcher (unit, `suggest.rs`):** the hop under the cursor with one or several commas and the cursor mid-chain; `user@` kept and excluded from the query; no hop once `:port` follows; case-insensitive contains with prefix matches first and alphabetical order; duplicates collapsed with managed preferred; the edited host and already-used hops excluded; `apply` on the first, middle and last hop.
- **TUI (unit, `tui.rs`):** nothing opens on focus; typing opens it and Up/Down move the selection; Enter and Tab pick without saving or changing field; Esc closes it and a second Esc cancels the form; Enter saves when it is closed; Ctrl+Space (`KeyCode::Char(' ')` with CONTROL) on an empty hop lists every candidate; an exact match closes it; with no other hosts it never opens; an edit form does not suggest its own host; a pick in a multi-hop value replaces only the current hop; focus change and paste behave as specified; the footer and the rendered box with aliases and origins.
- No integration or real-OpenSSH tests: the CLI and the saved value's validation are unchanged.
