# Jump Host Suggestions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The ProxyJump field of the TUI add and edit forms suggests known host aliases in a dropdown under the field, and picking one fills in the hop being typed.

**Architecture:** A new pure module, `src/suggest.rs`, finds the hop under the cursor, ranks candidate aliases and applies a pick. `src/tui.rs` keeps the open dropdown in `FormState`, gives it Up/Down/Enter/Tab/Esc while it is open, recomputes it after edits, and draws it as boxed lines under the ProxyJump row.

**Tech Stack:** Rust 2024 (MSRV 1.88), ratatui and crossterm as already used. No new crates.

**Spec:** `docs/superpowers/specs/2026-10-18-jump-host-suggestions-design.md`

## Global Constraints

- No new dependencies.
- The CLI is unchanged; only the TUI forms change. When the dropdown is closed every key behaves exactly as today.
- The dropdown shows at most 5 rows.
- Footer while open: `↑↓ choose  Enter pick  Esc close`. Footer while ProxyJump is focused and the dropdown is closed: the usual form footer with `  Ctrl+Space suggest` inserted before `  Esc cancel`.
- Origins read `managed`, or the external host's source path with `$HOME` shown as `~`.
- Alias comparisons (dedupe, exclusions, exact match) ignore ASCII case, as the TUI already does for aliases (`eq_ignore_ascii_case`); matching the query ignores case with `to_lowercase`.
- Before each commit run `scripts/ci-checks.sh fmt`, `scripts/ci-checks.sh clippy` and `scripts/ci-checks.sh test` as a regular user; all must exit 0.
- Commit messages follow Conventional Commits and carry no AI attribution or co-author trailer.

## Review Focus

1. Non-ASCII text in the field or in aliases (`café`, typed `CAF`): byte ranges must stay on character boundaries and matching must still work. Pinned in Task 1 (`ranks_and_applies_non_ascii_aliases`).
2. A pick with the cursor in the middle of a hop of a multi-hop value: the whole hop is replaced, later hops are kept, and the cursor lands at the end of the new hop. Pinned in Task 1 (`apply_replaces_the_hop_under_a_mid_hop_cursor`).
3. The inventory is replaced while the dropdown is open and has fewer matches: the selection is clamped, and the dropdown closes when nothing matches. Pinned in Task 2 (`replacing_hosts_recomputes_an_open_dropdown`).
4. More than 5 matches: moving Down past the fifth row scrolls the window, and Up at the top or Down at the bottom stays put. Pinned in Task 3 (`dropdown_scrolls_to_keep_the_selection_visible`).
5. Aliases equal except for case (`alpha` external, `ALPHA` managed): one row, the managed one. Pinned in Task 1 (`rank_collapses_duplicates_preferring_managed`).

---

### Task 1: The pure matcher (`src/suggest.rs`)

**Files:**
- Create: `src/suggest.rs`
- Modify: `src/lib.rs` (add `pub mod suggest;` in alphabetical order)

**Interfaces:**
- Produces:
  ```rust
  #[derive(Clone, Debug, Eq, PartialEq)]
  pub enum Origin { Managed, External(PathBuf) }

  #[derive(Clone, Debug, Eq, PartialEq)]
  pub struct Candidate { pub alias: String, pub origin: Origin }

  /// The alias part of the hop under the cursor: `range` is its byte range in the value
  /// (after any `user@`), `query` its text.
  #[derive(Clone, Debug, Eq, PartialEq)]
  pub struct Hop { pub range: Range<usize>, pub query: String }

  pub fn current_hop(value: &str, cursor: usize) -> Option<Hop>;
  pub fn rank(query: &str, candidates: &[Candidate], exclude: &[&str]) -> Vec<Candidate>;
  pub fn apply(value: &str, hop: &Hop, alias: &str) -> (String, usize);
  pub fn origin_label(origin: &Origin, home: Option<&Path>) -> String;
  ```

Rules:
- `current_hop`: the hop is the text between the nearest comma before the cursor (or the start) and the nearest comma at or after the cursor (or the end). If the hop contains `@`, the alias part starts after its last `@`. If the alias part contains `:`, return `None`. An empty hop returns `Some` with an empty range at the cursor's hop position.
- `rank`: drop candidates whose alias equals an `exclude` entry, ignoring ASCII case. Collapse aliases equal ignoring ASCII case into one, keeping a `Managed` one if any, otherwise the first. Keep those whose lowercase alias contains the lowercase query (an empty query matches all). Order: aliases starting with the query first, then the rest, each group sorted by lowercase alias.
- `apply`: replace `range` with `alias`; the cursor is `range.start + alias.len()`.
- `origin_label`: `Managed` gives `managed`; `External(path)` gives the path, with a leading `home` replaced by `~`.

- [ ] **Step 1: Write the failing tests** in `src/suggest.rs` under `#[cfg(test)] mod tests`:

```rust
fn c(alias: &str, managed: bool) -> Candidate {
    Candidate {
        alias: alias.into(),
        origin: if managed { Origin::Managed } else { Origin::External("/home/me/.ssh/config".into()) },
    }
}
fn aliases(list: &[Candidate]) -> Vec<&str> { list.iter().map(|c| c.alias.as_str()).collect() }

#[test]
fn current_hop_finds_the_hop_under_the_cursor() {
    assert_eq!(current_hop("bas", 3), Some(Hop { range: 0..3, query: "bas".into() }));
    assert_eq!(current_hop("bastion,pro", 11), Some(Hop { range: 8..11, query: "pro".into() }));
    assert_eq!(current_hop("a,bb,c", 3), Some(Hop { range: 2..4, query: "bb".into() }));
    assert_eq!(current_hop("", 0), Some(Hop { range: 0..0, query: "".into() }));
    assert_eq!(current_hop("bastion,", 8), Some(Hop { range: 8..8, query: "".into() }));
}

#[test]
fn current_hop_keeps_the_user_and_stops_at_a_port() {
    assert_eq!(current_hop("deploy@bas", 10), Some(Hop { range: 7..10, query: "bas".into() }));
    assert_eq!(current_hop("bastion:22", 10), None);
    assert_eq!(current_hop("bas:22,x", 2), None);
}

#[test]
fn rank_matches_contains_ignoring_case_with_prefixes_first() {
    let all = [c("eu-proxy", false), c("prod-jump", true), c("Proxy", false), c("web", true)];
    assert_eq!(aliases(&rank("pro", &all, &[])), ["prod-jump", "Proxy", "eu-proxy"]);
    assert_eq!(aliases(&rank("", &all, &[])), ["eu-proxy", "prod-jump", "Proxy", "web"]);
}

#[test]
fn rank_collapses_duplicates_preferring_managed() {
    let all = [c("alpha", false), c("ALPHA", true), c("beta", false), c("beta", false)];
    let ranked = rank("", &all, &[]);
    assert_eq!(aliases(&ranked), ["ALPHA", "beta"]);
    assert_eq!(ranked[0].origin, Origin::Managed);
}

#[test]
fn rank_drops_excluded_aliases() {
    let all = [c("bastion", true), c("dev", true), c("prod", true)];
    assert_eq!(aliases(&rank("", &all, &["DEV", "bastion"])), ["prod"]);
}

#[test]
fn apply_replaces_only_the_alias_part_of_the_hop() {
    let hop = current_hop("bastion,pro", 11).unwrap();
    assert_eq!(apply("bastion,pro", &hop, "prod-jump"), ("bastion,prod-jump".into(), 17));
    let hop = current_hop("deploy@bas", 10).unwrap();
    assert_eq!(apply("deploy@bas", &hop, "bastion"), ("deploy@bastion".into(), 14));
}

#[test]
fn apply_replaces_the_hop_under_a_mid_hop_cursor() {
    let hop = current_hop("a,bas,c", 3).unwrap();
    assert_eq!(apply("a,bas,c", &hop, "bastion"), ("a,bastion,c".into(), 9));
}

#[test]
fn ranks_and_applies_non_ascii_aliases() {
    let all = [c("café", true), c("cab", true)];
    assert_eq!(aliases(&rank("CAF", &all, &[])), ["café"]);
    let hop = current_hop("x,ca", 4).unwrap();
    assert_eq!(apply("x,ca", &hop, "café"), ("x,café".into(), 7));
}

#[test]
fn origin_label_names_managed_or_the_file_under_home() {
    let home = Path::new("/home/me");
    assert_eq!(origin_label(&Origin::Managed, Some(home)), "managed");
    assert_eq!(origin_label(&Origin::External("/home/me/.ssh/config".into()), Some(home)), "~/.ssh/config");
    assert_eq!(origin_label(&Origin::External("/etc/ssh/ssh_config".into()), Some(home)), "/etc/ssh/ssh_config");
}
```

- [ ] **Step 2: Run** `cargo test --lib suggest` — expected: FAIL to compile (module missing).
- [ ] **Step 3: Implement** the module per the rules above, with a short module doc comment matching the style of `src/config_error.rs`.
- [ ] **Step 4: Run** `cargo test --lib suggest` — expected: all 9 tests pass.
- [ ] **Step 5: Run the checks and commit**

```bash
git add src/suggest.rs src/lib.rs
git commit -m "feat: rank known aliases for the jump host being typed"
```

---

### Task 2: Dropdown state and keys in the form

**Files:**
- Modify: `src/tui.rs` (`FormState` ~:1124, `FormState::default`/`from_host` ~:1742-1780, `replace_hosts` ~:1249, `handle_paste` ~:1295, `handle_form_key` ~:1402, tests module)

**Interfaces:**
- Consumes: Task 1's `suggest::{Candidate, Origin, Hop, current_hop, rank, apply}`.
- Produces (used by Task 3):
  ```rust
  /// The open jump host dropdown.
  #[derive(Clone, Debug, Eq, PartialEq)]
  struct Suggestions {
      items: Vec<suggest::Candidate>, // never empty while open
      selected: usize,
      offset: usize,                  // first visible row; selected stays in offset..offset + SUGGESTION_ROWS
      forced: bool,                   // opened with Ctrl+Space
  }
  const SUGGESTION_ROWS: usize = 5;
  const PROXY_JUMP: usize = 5;        // index of ProxyJump in FORM_FIELDS
  // FormState gains: suggestions: Option<Suggestions>
  ```

Behaviour (from the spec):
- `UiState::suggest(&mut self, forced: bool)` recomputes the dropdown for the ProxyJump input: candidates are `self.hosts` mapped to `Candidate` (`Ownership::Managed` → `Origin::Managed`, else `Origin::External(source)`); excluded are the Alias field's value and, in an edit form, the original alias, plus every other hop's alias part of the field. It sets `suggestions` to `None` when the active field is not ProxyJump, when `current_hop` is `None`, when nothing ranks, or, unless `forced`, when the query is empty or equals a ranked alias ignoring ASCII case. Otherwise it keeps the previously selected alias selected if still present (else row 0), clamps `offset`, and keeps `forced` from the existing dropdown or the argument.
- In `handle_form_key`, before the existing match:
  - `Ctrl+Space` (`KeyCode::Char(' ')` with `KeyModifiers::CONTROL`) on ProxyJump calls `suggest(true)`.
  - If the dropdown is open: Up/Down move `selected` without wrapping and adjust `offset`; Enter (no modifiers) and Tab apply the pick via `suggest::apply`, set the input's text and cursor, close the dropdown, clear `error` and `test_result`, and return `UiAction::FormEdited`; Esc closes it and returns `UiAction::None`.
- Any field change (Up/Down/Tab/BackTab when closed), Enter save, Ctrl+T and Esc-cancel close it.
- One helper replaces the duplicated "text changed" code of `handle_form_key` and `handle_paste`; it clears `error`, `test_result` and `show_raw`, then: if the text changed, calls `suggest(false)`; if only the cursor moved and a dropdown is open, recomputes it as well. It returns `UiAction::FormEdited` only when the text changed, exactly as today.
- `replace_hosts` recomputes an open dropdown (closing it if nothing matches, clamping the selection).

- [ ] **Step 1: Write the failing tests** in the `tests` module of `src/tui.rs`. Add a helper:

```rust
fn proxy_jump_form(hosts: Vec<HostSummary>) -> UiState {
    let mut state = test_state(hosts);
    press(&mut state, KeyCode::Char('a'));
    for _ in 0..PROXY_JUMP { press(&mut state, KeyCode::Tab); }
    state
}
fn listed(state: &UiState) -> Vec<&str> {
    state.form.suggestions.as_ref().map_or(Vec::new(), |s| s.items.iter().map(|c| c.alias.as_str()).collect())
}
fn ctrl(c: char) -> KeyEvent { KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL) }
```

Tests (names and assertions):
- `jump_suggestions_do_not_open_on_focus`: hosts `bastion`, `dev`; after `proxy_jump_form`, `state.form.suggestions` is `None`.
- `typing_a_jump_host_opens_matching_suggestions`: hosts `bastion` (managed), `bastion-eu` (external), `web`; type `bas` → `listed == ["bastion", "bastion-eu"]`, `selected == 0`.
- `up_and_down_move_through_suggestions_without_wrapping`: same hosts, type `bas`; Down → `selected == 1`; Down → still 1; Up, Up → 0; `state.form.active == PROXY_JUMP` throughout.
- `enter_and_tab_pick_a_suggestion_without_saving`: type `bas`, Down, Enter → returns `UiAction::FormEdited`, value `bastion-eu`, cursor at its end, dropdown `None`, `state.mode == Mode::AddForm`, `active == PROXY_JUMP`. Repeat with a fresh form using Tab → value `bastion`.
- `esc_closes_suggestions_then_cancels_the_form`: type `bas`, Esc → dropdown `None`, mode `AddForm`; Esc → mode `Browse`.
- `enter_saves_when_suggestions_are_closed`: fill Alias `app` and Hostname `app.example` via `fill_form`, focus ProxyJump, type `bastion` (exact match, so closed) → Enter returns `UiAction::Add(_)` whose host has `proxy_jump() == Some("bastion")`.
- `ctrl_space_lists_every_candidate_for_an_empty_hop`: hosts `b`, `a`; `state.handle_key(ctrl(' '))` → `listed == ["a", "b"]`; `forced` is true; Backspace on the empty field keeps it open.
- `exact_alias_closes_suggestions`: hosts `bastion`; type `bastion` → `None`.
- `no_other_hosts_means_no_suggestions`: no hosts; type `b` and press Ctrl+Space → `None` both times.
- `edit_form_does_not_suggest_its_own_host`: managed `dev` and `devbox`; select `dev`, resolve its details, press `e`, focus ProxyJump, clear it, type `dev` → `listed == ["devbox"]`.
- `picking_in_a_chain_replaces_only_the_current_hop`: hosts `bastion`, `prod-jump`, `web`; type `bastion,pro`, Enter → value `bastion,prod-jump`; type `,` → dropdown `None` (empty hop); Ctrl+Space → `listed == ["web"]` (hops already used are excluded).
- `paste_into_proxy_jump_opens_suggestions`: hosts `bastion`; `state.handle_paste("bas")` → `listed == ["bastion"]` and returns `UiAction::FormEdited`.
- `leaving_proxy_jump_closes_suggestions`: type `bas`, BackTab → dropdown `None`, `active == PROXY_JUMP - 1`. In a fresh form, type `bas`, then `state.handle_key(ctrl('t'))` → dropdown `None` (the empty Alias makes the test report a form error, as today).
- `replacing_hosts_recomputes_an_open_dropdown`: hosts `bastion`, `bastion-eu`; type `bas`, Down; `replace_hosts` with only `bastion` → `listed == ["bastion"]`, `selected == 0`; `replace_hosts` with only `web` → `None`.

- [ ] **Step 2: Run** `cargo test --lib tui::tests` — expected: the new tests fail (to compile, then on assertions).
- [ ] **Step 3: Implement** the state, `suggest`, the key handling and the shared text-changed helper as described.
- [ ] **Step 4: Run** `cargo test --lib tui::tests` — expected: all pass, including every existing form test unchanged.
- [ ] **Step 5: Run the checks and commit**

```bash
git add src/tui.rs
git commit -m "feat: open jump host suggestions while typing ProxyJump"
```

---

### Task 3: Drawing the dropdown, the footer and the docs

**Files:**
- Modify: `src/tui.rs` (`form_lines` ~:2051, `footer_text` ~:1996, tests module)
- Modify: `README.md` (form keys paragraph ~:215-216)
- Modify: `ROADMAP.md` (remove v2 slice 1 "Jump host suggestions", renumber the rest starting at 1)

**Interfaces:**
- Consumes: Task 2's `FormState.suggestions`, `Suggestions`, `SUGGESTION_ROWS`, `PROXY_JUMP`; Task 1's `suggest::origin_label`.

Layout: directly after the ProxyJump row, `form_lines` adds a top border, one line per visible row (`offset..offset + SUGGESTION_ROWS`), and a bottom border. Each line starts with spaces equal to the character width of `"> ProxyJump: "`. Rows read `│> alias │  origin` for the selected row (styled `Modifier::REVERSED` on the part inside the box) and `│  alias │  origin` otherwise; the box's inner width is the longest visible alias plus 3, with aliases padded to it. Borders are `┌` `─` `┐` and `└` `─` `┘`. The home for `origin_label` is `std::env::var_os("HOME")`, as `expand_home` already uses.

- [ ] **Step 1: Write the failing tests:**
- `dropdown_renders_aliases_and_origins_under_proxy_jump`: hosts `bastion` (managed) and `bastion-eu` (external, source `/home/test/.ssh/config`); type `bas`; `rendered(&state)` contains `"│> bastion    │  managed"` and `"bastion-eu"` and `"┌"`, and the line containing `"> ProxyJump:"` is immediately followed by the line containing `"┌"`.
- `dropdown_scrolls_to_keep_the_selection_visible`: hosts `h1`..`h7`; Ctrl+Space; press Down 6 times → `selected == 6`, `offset == 2`; `rendered` contains `h7` and not `"│  h1"`; Down again → still 6; Up 6 times → `selected == 0`, `offset == 0`.
- `footer_shows_suggestion_keys_while_open`: type `bas` with host `bastion-eu` → `footer_text(&state) == "↑↓ choose  Enter pick  Esc close"`.
- `footer_advertises_ctrl_space_on_proxy_jump`: closed dropdown on ProxyJump → footer contains `"Ctrl+Space suggest  Esc cancel"`; on the Alias field it does not contain `"Ctrl+Space"`.

- [ ] **Step 2: Run** `cargo test --lib tui::tests` — expected: the four new tests fail.
- [ ] **Step 3: Implement** the drawing in `form_lines` and the two footer variants in `footer_text`.
- [ ] **Step 4: Run** `cargo test --lib tui::tests` — expected: all pass.
- [ ] **Step 5: Docs.** README, after the ProxyJump sentence at ~:216: "While you type a ProxyJump hop, a dropdown lists the known hosts whose alias contains what you typed: `↑` / `↓` choose, `Enter` or `Tab` fills in the hop, and `Esc` closes the list (a second `Esc` cancels the form). `Ctrl+Space` opens the list at any time." ROADMAP: delete the "Jump host suggestions" item and renumber all product slices from 1. Run `cargo test --test docs` — expected: pass.
- [ ] **Step 6: Run the checks and commit**

```bash
git add src/tui.rs README.md ROADMAP.md
git commit -m "feat: suggest known jump hosts in the host forms"
```

Mark the spec's status line `**Status:** Approved; implemented` in the same commit.
