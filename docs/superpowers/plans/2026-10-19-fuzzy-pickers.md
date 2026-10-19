# Fuzzy Pickers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every CLI command that needs a host (or a container) and gets none opens a built-in inline picker, and the TUI filters use the same fuzzy matcher, with a new filter in the `m` checklist.

**Architecture:** `src/fuzzy.rs` holds a pure matcher (folded, multi-term, in-order subsequence with a scored best alignment and matched positions) plus a span helper for highlighting. `src/picker.rs` holds the single- and multi-select picker states (key events in, outcome out), their rendering, a crossterm/ratatui inline terminal loop on stderr, and the `Picker` trait that `cli::run` now takes. The CLI replaces its `fzf` call and numbered menus with the trait; the TUI swaps its substring filters for the matcher and gives `MigrateState` a filter.

**Tech Stack:** Rust 2024, ratatui 0.30 and crossterm 0.29 as already used, clap derive. No new crates.

**Spec:** `docs/superpowers/specs/2026-10-19-fuzzy-pickers-design.md`

## Global Constraints

- No new dependencies. `fzf` is run nowhere after Task 3.
- Comparison goes through `suggest::fold`; matched positions are char indices into the original (unfolded) text.
- The picker draws on stderr, inline (never the alternate screen), at most 12 lines, at least the query line plus one row; it erases itself on close and always leaves raw mode, also on error or panic.
- A cancelled pick exits with status 130 (`CANCELLED`) and runs nothing.
- The TUI never re-sorts: filters keep list order. Only the CLI single-select picker ranks.
- Copy fixed by the spec: `No match.`, `No hosts.`, `No managed hosts.`, `No hosts match "<filter>".`, `choose a host: portico <command> <alias> (no terminal for the picker)`, `choose a host: portico <command> --on <alias> (no terminal for the picker)`.
- Before each commit run `scripts/ci-checks.sh fmt`, `clippy` and `test` as a regular user; run `e2e` in Task 8.
- Conventional Commits, no AI attribution in commits or PRs.

## Review Focus

- **A query typed with accents or uppercase** (`Prod`, `pród`) must match `prod-db` and highlight the right characters, including after a character that folds to two (`æ`). Pinned in Task 1 (`positions_refer_to_original_characters`).
- **A terminal shorter than the list** must not panic or draw past the viewport; the picker shrinks to query plus one row. Pinned in Task 2 (`renders_within_a_tiny_height`).
- **`Esc` in the multi picker with an applied filter** must clear the filter, not cancel the adoption the user was building. Pinned in Task 2 (`multi_esc_clears_filter_then_cancels`) and Task 7 for the TUI.
- **Hosts marked, then hidden by a filter** must still be adopted on `Enter`, and the count must show them. Pinned in Task 2 (`multi_returns_hidden_marks`) and Task 7 (`enter_previews_hidden_marks`).
- **`edit -u root` with no alias** must pick a host and still apply `-u root`, and `remove` must still confirm after the pick. Pinned in Task 4 (`edit_without_alias_applies_flags_to_the_pick`, `remove_without_alias_still_confirms`).

---

### Task 1: The matcher

**Files:**
- Create: `src/fuzzy.rs`
- Modify: `src/lib.rs` (add `pub mod fuzzy;`)

**Interfaces:**
- Consumes: `crate::suggest::fold(&str) -> String` (already `pub(crate)`).
- Produces:
  - `pub struct Match { pub score: i64, pub positions: Vec<Vec<usize>> }` — one sorted, deduplicated list of char indices per field passed in.
  - `pub fn match_fields(query: &str, fields: &[&str]) -> Option<Match>` — `None` when some term matches no field; an empty or whitespace-only query returns `Some` with score 0 and empty positions.
  - `pub fn rank<'a>(query: &str, rows: impl IntoIterator<Item = &'a [&'a str]>) -> Vec<(usize, Match)>` — matching row indices with their match, best score first, ties in input order (stable sort).
  - `pub fn highlight(text: &str, positions: &[usize], base: Style, hit: Style) -> Vec<Span<'static>>` — splits `text` into spans, matched chars styled `hit`.

- [ ] **Step 1: Write the failing tests** in `src/fuzzy.rs` `mod tests`:
  - `subsequence_in_order_matches`: `match_fields("prdb", &["prod-db"])` is `Some`, positions `[[0,1,5,6]]`; `match_fields("bdrp", &["prod-db"])` is `None`.
  - `every_term_must_match`: `"db prod"` matches `["prod-db"]`; `"db web"` does not.
  - `terms_may_match_different_fields`: `match_fields("app nginx", &["web-app", "nginx:1.27"])` is `Some`, positions `[[4,5,6],[0,1,2,3,4]]`.
  - `case_and_accents_are_ignored`: `"PROD"` and `"pröd"` match `["prod"]`; `"e"` matches `["Émile"]` with positions `[[0]]`.
  - `positions_refer_to_original_characters`: `match_fields("aen", &["æn"])` positions `[[0,1]]`.
  - `empty_query_matches_everything`: `""` and `"  "` give `Some(Match { score: 0, positions: vec![vec![]] })` for one field.
  - `field_start_beats_middle`: `rank("db", [["db-main"], ["prod-db"], ["sandbox"]])` order is `0, 1, 2` (start, after separator, middle) — assert `score(0) > score(1) > score(2)`.
  - `consecutive_beats_scattered`: score of `"web"` on `"web-1"` > on `"wxexb"`.
  - `ties_keep_input_order`: `rank("a", [["xa"], ["ya"]])` returns indices `[0, 1]`.
  - `highlight_splits_matched_characters`: `highlight("prod", &[0,2], base, hit)` gives spans `"p"(hit) "r"(base) "o"(hit) "d"(base)`; adjacent hits merge into one span.

- [ ] **Step 2: Run** `cargo test --lib fuzzy` — expected: fails to compile (module missing).

- [ ] **Step 3: Implement `src/fuzzy.rs`.**
  Fold per original character: build `Vec<(char, usize)>` of folded chars, each tagged with its original char index (`fold(&c.to_string())` per char). Split the folded query on whitespace into terms. For each term and each field run this scoring DP and keep the field with the best score; a term matching no field ends in `None`. Row score = sum of term scores; positions = union per field.

  Scoring constants (fixed, so ranking tests are deterministic):
  ```
  MATCH = 16; BOUNDARY = 8; FIELD_START = 8 (added on top of BOUNDARY for index 0);
  CONSECUTIVE = 4; GAP_START = -3; GAP_EXTEND = -1
  boundary(i) = i == 0 || prev original char in "-_./:@ " or whitespace || (prev lowercase && this uppercase)
  ```
  DP over term chars `j` and text chars `i`: `best[j][i]` = best score with term[j] matched at text[i] = `MATCH + bonus(i) + max( best[j-1][k] + gap(i-k-1) over k < i, plus CONSECUTIVE when k == i-1 )`, `gap(0)=0`, `gap(n)=GAP_START + (n-1)*GAP_EXTEND`. No penalty before the first match. Keep back-pointers to recover positions; the best end is the max over `best[last][i]`. O(term × text²) is fine for these sizes; prefer the clearer form.

- [ ] **Step 4: Run** `cargo test --lib fuzzy` — expected: all pass.

- [ ] **Step 5: Commit** `feat: add a fuzzy matcher with ranked, highlighted matches`.

---

### Task 2: Picker states and rendering

**Files:**
- Create: `src/picker.rs`
- Modify: `src/lib.rs` (`pub mod picker;`), `src/tui.rs` (make `TextInput` and its `new`, `as_str`, `is_empty`, `handle_key`, `paste`, `spans` `pub(crate)`)

**Interfaces:**
- Consumes: Task 1 `match_fields`, `rank`, `highlight`; `crate::tui::TextInput`.
- Produces:
  - `pub struct Row { pub fields: Vec<String>, pub extra: Vec<String> }` — `fields` are searched and shown first; `extra` shown dimmed, not searched.
  - `pub(crate) enum Outcome { Continue, Picked(Vec<usize>), Cancelled }` — indices into the rows passed in.
  - `pub(crate) struct Single` with `fn new(prompt: &str, rows: Vec<Row>) -> Self`, `fn handle_key(&mut self, key: KeyEvent) -> Outcome`, `fn paste(&mut self, text: &str)`, `fn render(&self, area: Rect, buf: &mut Buffer)`, `fn height(&self, max: u16) -> u16`.
  - `pub(crate) struct Multi` with the same methods (`new(title: &str, rows: Vec<Row>)`).
  - `const MAX_HEIGHT: u16 = 12;`

- [ ] **Step 1: Write the failing tests** in `src/picker.rs` `mod tests`, driving keys with a `press(&mut state, KeyCode)` helper and rendering into a ratatui `Buffer` read back as lines:
  - `single_ranks_best_first`: rows `prod-db`, `sandbox`, `db-main`; type `db`; `Enter` → `Picked(vec![2])`.
  - `single_cursor_resets_on_query_change`: `Down`, then type `d` → cursor on the first ranked row.
  - `single_moves_with_arrows_ctrl_and_pages`: `Down`/`Up`, `Ctrl+N`/`Ctrl+P`, `PageDown` moves by `MAX_HEIGHT - 1` and clamps.
  - `single_enter_with_no_match_does_nothing`: query `zzz`, `Enter` → `Continue`; render shows `No match.`.
  - `single_cancels_on_esc_and_ctrl_c`: both → `Cancelled`.
  - `single_paste_goes_to_the_query`: `paste("db\n")` → query `db`.
  - `single_prompt_line_shows_counts`: 42 rows, query `pr` matching 3 → first line `host> pr  3/42`.
  - `multi_space_marks_and_a_toggles_visible`: `Space` marks cursor row; `a` marks all visible; `a` again unmarks them.
  - `multi_filter_keys_follow_the_hosts_filter`: `/`, type `pr`, `Enter` applies (rows narrowed, `j`/`k` move again); `/`, type `x`, `Esc` restores `pr`.
  - `multi_esc_clears_filter_then_cancels`: with an applied filter `Esc` → `Continue` and filter empty; `Esc` again → `Cancelled`.
  - `multi_returns_hidden_marks`: mark row 0, filter to exclude it, mark row 2, `Enter` → `Picked(vec![0, 2])`; title line reads `adopt  /<filter>  2 marked`.
  - `multi_enter_without_marks_does_nothing`: → `Continue`.
  - `multi_keeps_input_order`: filtered rows appear in input order, not by score.
  - `renders_highlights_and_dimmed_extra`: the matched cells carry the hit style, `extra` cells the dim modifier, the cursor row is reversed.
  - `renders_within_a_tiny_height`: `height(2) == 2`; rendering 50 rows into a 2-line area shows the prompt and the cursor row, no panic.

- [ ] **Step 2: Run** `cargo test --lib picker` — expected: compile failure.

- [ ] **Step 3: Implement the two states and their `render`.** Single: `TextInput` query, ranked `Vec<(usize, Match)>` recomputed on every query edit, `cursor`, `offset` kept so the cursor is visible. Multi: `filter: TextInput`, `editing: bool`, `before_edit: String`, `marks: BTreeSet<usize>`, `visible: Vec<usize>` in input order, cursor over `visible`. Mark box `[x] `/`[ ] `. Hit style: `Modifier::BOLD | Modifier::UNDERLINED`, exported as `pub(crate) const HIT: Style` for the TUI to reuse; extra: `Modifier::DIM`; cursor row: `Modifier::REVERSED`. `height(max) = min(MAX_HEIGHT, max, 1 + rows.max(1))`, never below 2 when `max >= 2`.

- [ ] **Step 4: Run** `cargo test --lib picker` — expected: pass.

- [ ] **Step 5: Commit** `feat: add single and multi select picker states`.

---

### Task 3: The picker in the CLI, starting with containers

**Files:**
- Modify: `src/picker.rs` (trait and terminal loop), `src/cli.rs` (`run`/`execute` signature, `shell` container pick, remove `pick` and its tests), `src/main.rs`, `src/app/containers.rs` (drop `fzf` from `ContainerPrograms` and its test setup), `tests/cli.rs` if it calls `run` directly.

**Interfaces:**
- Consumes: Task 2 `Row`, `Single`, `Multi`, `Outcome`.
- Produces:
  - ```rust
    pub trait Picker {
        /// The chosen row, or `None` when cancelled.
        fn pick_one(&mut self, prompt: &str, rows: Vec<Row>) -> Result<Option<usize>, String>;
        /// The marked rows in input order, or `None` when cancelled.
        fn pick_many(&mut self, title: &str, rows: Vec<Row>) -> Result<Option<Vec<usize>>, String>;
    }
    pub struct TerminalPicker;
    impl Picker for TerminalPicker { … }
    ```
  - `cli::run(cli, app, picker: &mut dyn Picker, input, output, error, interactive, handle_signals) -> ExitCode` (new third parameter; `execute` likewise).
  - In `cli` tests: `struct Scripted(VecDeque<Option<&'static str>>)` implementing `Picker`: `pick_one` pops the next answer and returns the index of the row whose `fields[0]` equals it (`None` → cancelled, a missing row panics); `pick_many` pops one answer per call, a comma-separated list of `fields[0]` values. It records `(prompt, Vec<fields[0]>)` for assertions.

- [ ] **Step 1: Write the failing tests** in `src/cli.rs` tests, replacing `numbered_picker_*` and `fzf_*`:
  - `shell_picks_a_container_through_the_picker`: two containers, scripted `Some("<second display name>")` → exec call for the second id.
  - `shell_container_rows_search_names_project_and_image`: recorded row `fields` are `[display name, name, project or "", service or "", image]`, `extra` `[project or "Other", status]`.
  - `shell_picker_cancelled_exits_130_and_runs_nothing`: scripted `None` → `ExitCode::from(130)`, no `docker exec` call.
  - `shell_without_terminal_still_needs_a_container`: `interactive = false` → existing error text.

- [ ] **Step 2: Run** `cargo test --lib cli::tests::shell_` — expected: compile failure (`run` arity, `Scripted`).

- [ ] **Step 3: Implement.**
  - `TerminalPicker::pick_*`: `enable_raw_mode`, a `Terminal<CrosstermBackend<Stderr>>` with `Viewport::Inline(state.height(terminal rows))`, loop `event::read()`: `Key` (press only) → `handle_key`, `Paste` → `paste`, `Resize` → `terminal.autoresize()`; redraw after each event. Enable bracketed paste. A guard struct's `Drop` disables bracketed paste and raw mode and clears the viewport, so errors and panics restore the terminal. Map a setup failure to `Err(format!("cannot open the picker: {error}"))`.
  - `shell`: build `Row`s from `group(containers)` as listed above, `picker.pick_one("container", rows)`; map the index back to the container id.
  - `main.rs` passes `&mut TerminalPicker`.
  - Delete `pick`, `ContainerPrograms::fzf`, the `"fzf"` entry in test program setup, and the `fzf`/numbered tests.

- [ ] **Step 4: Run** `cargo test --all-targets` — expected: pass.

- [ ] **Step 5: Commit** `feat: pick containers with the built-in picker instead of fzf`.

---

### Task 4: Host pickers for single-host commands and `--on`

**Files:**
- Modify: `src/cli.rs`

**Interfaces:**
- Consumes: Task 3 `Picker`, `Row`.
- Produces:
  - `alias: Option<String>` on `Show`, `Edit`, `Remove`, `Connect`, `Test`.
  - `on: Option<Option<String>>` on `Containers` and `Shell` with `#[arg(long = "on", value_name = "ALIAS", num_args = 0..=1)]`: `None` this machine, `Some(None)` ask, `Some(Some(alias))` that host.
  - `fn choose_host(app: &App, picker: &mut dyn Picker, interactive: bool, command: &str, managed_only: bool, on: bool) -> Result<Option<String>, String>` — `Ok(None)` when cancelled; errors carry the spec's messages.

- [ ] **Step 1: Write the failing tests:**
  - `parses_optional_aliases_and_bare_on`: `connect` → `alias: None`; `shell --on` → `on: Some(None)`; `shell --on box` → `Some(Some("box"))`; `containers` → `on: None`.
  - `connect_test_show_list_every_host`: managed `dev`, external `legacy`; `show` with no alias, scripted `Some("legacy")` → prints `legacy` details; recorded rows `[["dev"], ["legacy"]]` with `extra` `["managed", <source>]` / `["external", <source>]`, prompt `host`.
  - `edit_and_remove_list_managed_hosts_only`: recorded rows for `edit -u x` only `dev`.
  - `edit_without_alias_applies_flags_to_the_pick`: `edit -u root`, scripted `Some("dev")` → `Updated dev.` and `dev`'s user is `root`.
  - `remove_without_alias_still_confirms`: `remove`, scripted `Some("dev")`, input `n\n` → `Removal cancelled.` and `dev` still present.
  - `cancelled_host_pick_exits_130_and_changes_nothing`: `remove`, scripted `None` → exit 130.
  - `no_hosts_and_no_managed_hosts`: empty inventory → `show` fails with `No hosts.`; only external hosts → `edit` fails with `No managed hosts.`; picker never called.
  - `single_host_still_opens_the_picker`: one host → picker called once.
  - `without_terminal_the_alias_is_required`: `interactive = false`, `connect` → error `choose a host: portico connect <alias> (no terminal for the picker)`; `shell --on` → `choose a host: portico shell --on <alias> (no terminal for the picker)`.
  - `bare_on_picks_the_host_then_the_container`: `shell --on`, scripted `Some("box")` then `Some("<container>")` → exec through `ssh` to `box`; recorded prompts `["host", "container"]`.

- [ ] **Step 2: Run** `cargo test --lib cli::tests` — expected: the new tests fail.

- [ ] **Step 3: Implement** `choose_host` from `app.hosts()` (filter `Ownership::Managed` when `managed_only`), rows `Row { fields: vec![alias], extra: vec![ownership(..).into(), source.display().to_string()] }`. Each command resolves its alias first (`Some` → as before; `None` → `choose_host`, `Ok(None)` → `return Ok(ExitCode::from(CANCELLED))`) then runs its existing body unchanged. `target(on)` takes the resolved `Option<String>`; the held-shell path (`--hold`) keeps requiring its values.

- [ ] **Step 4: Run** `cargo test --all-targets` — expected: pass.

- [ ] **Step 5: Commit** `feat: pick a host when a command is given none`.

---

### Task 5: Multi-select for adopt and export

**Files:**
- Modify: `src/cli.rs` (`run_migration`; remove `parse_selection` and its test if nothing else uses it)

**Interfaces:**
- Consumes: Task 3 `Picker::pick_many`.
- Produces: no new public items.

- [ ] **Step 1: Write the failing tests,** replacing `interactive_adopt_*` and `interactive_export_*`:
  - `interactive_adopt_moves_the_picked_hosts`: two eligible external hosts, scripted `Some("a,b")`, input `y\n` → both adopted; recorded title `adopt`, rows `fields` the aliases, `extra` the `describe_source` text.
  - `ineligible_hosts_are_printed_before_the_picker`: output contains `Not eligible:` and the reason; rows exclude them.
  - `cancelled_adoption_exits_130_and_changes_nothing`: scripted `None`.
  - `declined_preview_changes_nothing`: scripted `Some("a")`, input `n\n` → unchanged.
  - `interactive_export_moves_picked_hosts_back`.
  - Keep `adopt_without_eligible_hosts_says_so_and_succeeds` and `non_interactive_adopt_needs_aliases_or_all_and_yes` passing unchanged.

- [ ] **Step 2: Run** `cargo test --lib cli::tests::` — expected: new tests fail.

- [ ] **Step 3: Implement:** keep the `Not eligible:` printing, drop the `Eligible hosts:` numbered list and the selection prompt; `picker.pick_many(verb, rows)` (verb is `adopt` or `export`); `None` → `ExitCode::from(CANCELLED)`.

- [ ] **Step 4: Run** `cargo test --all-targets` — expected: pass.

- [ ] **Step 5: Commit** `feat: choose hosts to adopt or export with the multi-select picker`.

---

### Task 6: Fuzzy hosts and containers filters in the TUI

**Files:**
- Modify: `src/tui.rs` (`refilter`, hosts list rendering), `src/tui/containers.rs` (`matches`, row rendering)

**Interfaces:**
- Consumes: Task 1 `match_fields`, `highlight`.
- Produces: no new public items.

- [ ] **Step 1: Write the failing tests:**
  - `hosts_filter_is_fuzzy_and_keeps_order` (in `src/tui.rs`): hosts `web-prod`, `db-prod`, `prod-db` in that order; filter `pdb` → visible `["prod-db"]`; filter `prod` → `["web-prod", "db-prod", "prod-db"]` (list order, not score).
  - `hosts_filter_highlights_matches`: render with filter `pd` → the `p` and `d` cells of `prod-db` carry the hit style.
  - `containers_filter_is_fuzzy` (in `src/tui/containers.rs`): filter `ngx` matches a container whose image is `nginx`; order and project headers unchanged.
  - `containers_filter_highlights_the_shown_name`.
  - Existing substring-based filter tests keep passing (a substring is a subsequence).

- [ ] **Step 2: Run** `cargo test --lib tui` — expected: new tests fail.

- [ ] **Step 3: Implement:** `refilter` keeps hosts where `match_fields(filter, &[alias])` is `Some`, storing positions for rendering; containers' `matches` uses `match_fields(filter, &[shown, name, project, service, image])`. Render the alias / shown name with `highlight` (positions of field 0). Keep order; do not call `rank`.

- [ ] **Step 4: Run** `cargo test --lib tui` — expected: pass.

- [ ] **Step 5: Commit** `feat: fuzzy filter the TUI hosts and containers`.

---

### Task 7: Filter in the adopt and export checklist

**Files:**
- Modify: `src/tui/migrate.rs`, `src/tui.rs` (paste routing for `Mode::Migrate` while editing)

**Interfaces:**
- Consumes: Task 1 `match_fields`, `highlight`; `TextInput`.
- Produces: `MigrateState::paste(&mut self, text: &str)`; `title()` and `footer()` return owned values (`Line<'static>` and `String`) instead of `&'static str`.

- [ ] **Step 1: Write the failing tests** in `src/tui/migrate.rs`:
  - `slash_filters_the_checklist`: `/`, type `pr` → only matching candidates shown, ineligible ones filtered like the rest.
  - `filter_edit_keys_match_the_hosts_filter`: `Enter` applies; `/`, type, `Esc` restores the previous filter.
  - `esc_clears_filter_before_closing`: applied filter, `Esc` → `None` and filter cleared; `Esc` → `Close`.
  - `marks_survive_filter_changes`: mark `a`, filter to hide it, clear filter → `a` still marked.
  - `a_marks_only_visible_eligible_rows`.
  - `enter_previews_hidden_marks`: mark `a`, filter to `b`, mark `b`, `Enter`, `y` → `Apply(vec!["a","b"])`.
  - `cursor_stays_or_moves_to_first_match` and `jk_move_through_matches_only`.
  - `no_match_message`: pane shows `No hosts match "zzz".`
  - `title_and_footer_show_the_filter`: title contains `/pr` and `1 marked`; footer contains `/ filter` and, when applied, `Esc clear filter`; while editing the filter-editing footer.

- [ ] **Step 2: Run** `cargo test --lib tui::migrate` — expected: new tests fail.

- [ ] **Step 3: Implement** with `filter: TextInput`, `editing_filter: bool`, `filter_before_edit: String`, `visible: Vec<usize>` recomputed on each filter change, cursor as an index into `visible`. Route paste in `src/tui.rs` to `MigrateState::paste` when `Mode::Migrate`. Highlight matched alias characters.

- [ ] **Step 4: Run** `cargo test --lib tui` — expected: pass.

- [ ] **Step 5: Commit** `feat: filter the adopt and export checklist`.

---

### Task 8: End to end and docs

**Files:**
- Modify: `tests/e2e/terminal.rs` (add `Key::Esc` → `b"\x1b"`, `Key::Down` → `b"\x1b[B"` if absent), `tests/e2e/cli_connect.rs`, `README.md`, `docs/superpowers/specs/2026-10-18-container-shells-design.md`, `docs/superpowers/specs/2026-10-19-fuzzy-pickers-design.md` (status), `ROADMAP.md`, `docs/superpowers/specs/2026-09-13-portico-host-management-design.md`

- [ ] **Step 1: Write the e2e test** `connect_without_alias_picks_a_host` in `tests/e2e/cli_connect.rs`, modelled on `connect_opens_a_real_session`: spawn `["connect"]`, `wait_for("host> ")`, `type_text("bo")`, `key(Key::Enter)`, then the same session assertions; and `cancelled_pick_exits_130`: spawn `["connect"]`, wait, `key(Key::Esc)`, `wait_exit()` code 130, screen no longer shows `host> `.

- [ ] **Step 2: Run** `scripts/ci-checks.sh e2e` — expected: pass (the picker landed in Tasks 2–4).

- [ ] **Step 3: Docs:**
  - README: optional dependencies drop `fzf`; a "Picking a host" section with the single-select and multi-select keys from the spec, optional aliases, bare `--on`; the containers section's picker sentence updated; the checklist keys table gains `/` and `Esc clear filter`.
  - Container shells design: a line under its CLI decision that the picker is replaced by `2026-10-19-fuzzy-pickers-design.md`.
  - Fuzzy pickers design status → `Approved; implemented`.
  - `ROADMAP.md`: remove the fzf pickers item and renumber; v1 design's "Delivered follow-up slices" gains **Fuzzy pickers** with one sentence and the spec name.

- [ ] **Step 4: Run** `scripts/ci-checks.sh fmt`, `clippy`, `test`, `docs`, `e2e` — expected: all pass.

- [ ] **Step 5: Commit** `docs: describe the built-in pickers` (the e2e test may go in a separate `test:` commit).
