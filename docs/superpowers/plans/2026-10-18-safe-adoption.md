# Safe Adoption Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Adopt concrete external `Host` blocks into the managed fragment, and export managed hosts back to `~/.ssh/config`, each losslessly, after an exact preview, behind temporary backups with automatic rollback.

**Architecture:** A new `adopt` module scans user files into blocks, decides eligibility, builds a plan holding a byte snapshot of every file it read, renders the preview from the plan, and applies the plan as one multi-file transaction (managed fragment first, user files second) guarded by an `ssh -G` equality check before and after activation. The managed fragment learns to keep comments. CLI and TUI are thin layers over `App::adoption_plan`, `App::export_plan` and `App::migrate`.

**Tech Stack:** Rust (edition and MSRV as in `Cargo.toml`), `thiserror`, `ratatui`/`crossterm`, `clap`, `fs2` locks, `tempfile` in tests. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-10-18-safe-adoption-design.md`

## Global Constraints

- No new crates (cargo deny/audit stay unchanged).
- Never write to a user file except by atomic rename of a fully written temporary file beside it, keeping its mode; a symlinked file has its target replaced (`ManagedStore::entry_target` logic), never the link.
- Every mutation takes the Portico advisory lock (`paths.lock`) and requires a ready setup (`ensure_ready`).
- Byte-for-byte preservation of every line outside the moved range, including `\r\n` endings.
- Backup names: `.<file name>.portico-adopt-backup-<pid>-<nanos>-<attempt>` beside the original (produced by `write_unique` with stem `.<file name>.portico-adopt-backup`), deleted after success, kept only when rollback fails.
- Exact user-facing copy (from the spec): `Adopt which hosts? (all, numbers like 1,3-4, or none)`, `Adopt N hosts? [y/N] ` / `Export N hosts? [y/N] ` (`host` when N = 1), `Adopted: a, b, c`, `Exported: a, b, c`, `the configuration changed since the preview; run the command again to see a fresh preview`.
- Tests run as a regular user; `scripts/ci-checks.sh fmt|clippy|test` must pass.
- Commits: Conventional Commits, no AI attribution lines of any kind.
- Do not edit `ROADMAP.md`.
- Keep `src/tui.rs` changes to: the `Mode::Migrate` variant, the `m` key, routing, the right-pane and footer arms, and the error classification arm.

## Review Focus

1. **A source file edited between preview and confirm** (user saves in an editor while the prompt waits): must refuse with the stale message and change nothing. Test in Task 7.
2. **The same file holding several selected blocks**, adjacent or separated only by a blank line: all ranges removed in one candidate, line numbers in the preview refer to the original file. Tests in Tasks 4 and 7.
3. **A file without a trailing newline whose last block is adopted**, and CRLF files: the remaining bytes must be untouched and no stray blank line created. Tests in Task 4.
4. **Root-owned or read-only include files** (for example under `/etc/ssh/ssh_config.d`): listed ineligible with a reason, never attempted. Test in Task 6.
5. **Exporting when `~/.ssh/config` is a symlink** (dotfile managers): the target is replaced and the link survives. Test in Task 7.

---

### Task 1: Comments in the managed fragment

**Files:**
- Modify: `src/managed.rs` (`ManagedConfig`, `parse`, `render`, `render_host`, `insert`, `replace`, `apply`, `remove`)
- Test: `src/managed.rs` `mod tests`

**Interfaces:**
- Produces:
  - `pub struct HostComments { pub leading: Vec<String>, pub fields: BTreeMap<HostField, Vec<String>> }` with `#[derive(Clone, Debug, Default, Eq, PartialEq)]` and `pub fn is_empty(&self) -> bool`. Each string is one full comment line starting with `#`, without indentation or line ending. Only `Hostname`, `User`, `Port`, `IdentityFile`, `ProxyJump` keys are used (`HostField` gains `PartialOrd, Ord` derives).
  - `ManagedConfig` gains private `comments: BTreeMap<String, HostComments>` keyed by lowercase alias (absent = no comments).
  - `pub fn comments(&self, alias: &str) -> Option<&HostComments>`
  - `pub fn insert_with_comments(&mut self, host: Host, comments: HostComments) -> Result<(), ManagedError>` (`insert` delegates with `HostComments::default()`)
  - `pub fn remove_entry(&mut self, alias: &str) -> Result<(Host, HostComments), ManagedError>` (`remove` delegates and drops the comments)
  - `replace` re-keys the comments to the new alias; `apply` drops `fields[f]` for every field set to `Change::Clear`.

Parsing rules: a run of comment lines is buffered; a blank line clears the buffer; a `Host` line takes the buffer as `leading`; a directive line inside a host takes the buffer as that directive's `fields` entry. A buffer left at the end of the file or before a blank line is discarded. Rendering: leading comments above `Host` at column 0, field comments indented four spaces directly above their directive.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn comments_round_trip_through_parse_and_render() {
    let text = "# Managed by Portico. Do not edit manually.\n\n# work box\nHost dev\n    HostName dev.example\n    # old account\n    User bob\n";
    let config = ManagedConfig::parse(text).unwrap();
    assert_eq!(config.comments("dev").unwrap().leading, vec!["# work box"]);
    assert_eq!(config.comments("DEV").unwrap().fields[&HostField::User], vec!["# old account"]);
    assert_eq!(config.render(), text);
}

#[test]
fn header_and_detached_comments_never_attach() {
    let text = "# Managed by Portico. Do not edit manually.\n\n# detached\n\nHost dev\n    HostName dev.example\n";
    let config = ManagedConfig::parse(text).unwrap();
    assert!(config.comments("dev").is_none_or(HostComments::is_empty));
    assert!(!config.render().contains("# detached"));
}

#[test]
fn rename_keeps_comments_and_clear_drops_field_comments() {
    let mut config = ManagedConfig::empty();
    let comments = HostComments {
        leading: vec!["# lead".into()],
        fields: BTreeMap::from([(HostField::User, vec!["# who".into()]), (HostField::Port, vec!["# p".into()])]),
    };
    config.insert_with_comments(Host::new("dev".into(), "h".into(), Some("bob".into()), Some(2222), None, None).unwrap(), comments).unwrap();
    config.apply("dev", HostChanges { alias: Change::Set("box".into()), user: Change::Clear, ..HostChanges::default() }).unwrap();
    let kept = config.comments("box").unwrap();
    assert_eq!(kept.leading, vec!["# lead"]);
    assert!(!kept.fields.contains_key(&HostField::User));
    assert_eq!(kept.fields[&HostField::Port], vec!["# p"]);
    assert!(config.comments("dev").is_none());
}

#[test]
fn remove_entry_returns_the_comments() {
    // insert_with_comments then remove_entry: returns the same HostComments; comments("dev") is None afterwards.
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib managed::tests::comments_ managed::tests::header_ managed::tests::rename_keeps managed::tests::remove_entry`
Expected: compile errors (`HostComments` not found).

- [ ] **Step 3: Implement as described in Interfaces**

`render_host(host: &Host, comments: Option<&HostComments>) -> String`. Existing tests that compare whole rendered files must keep passing unchanged.

- [ ] **Step 4: Run the managed tests**

Run: `cargo test --lib managed::`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add src/managed.rs src/host.rs
git commit -m "feat: keep comments of managed hosts"
```

---

### Task 2: Discovery records the Host line and conditional includes

**Files:**
- Modify: `src/discovery.rs` (`ExternalHost`, `Scanner`)
- Test: `src/discovery.rs` `mod tests`

**Interfaces:**
- Produces: `ExternalHost { pub alias: String, pub source: PathBuf, pub line: usize, pub conditional: bool }`. `line` is the 1-based line of the `Host` directive in `source`. `conditional` is true when `source` was reached through an `Include` that appears after a `Host` or `Match` line other than `Host *` or `Match all` in the including file, or when the including file was itself conditional.
- Update every `ExternalHost { .. }` constructor in tests.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn records_host_lines() {
    // entry: "Include ~/.ssh/portico/config\n\nHost a\n  HostName x\nHost b c\n"
    // expect a.line == 3, b.line == 5, c.line == 5, all conditional == false
}

#[test]
fn includes_inside_a_block_are_conditional() {
    // entry: "Include ~/.ssh/portico/config\nInclude top.conf\nHost *\n  Include star.conf\nHost gate\n  Include inner.conf\n"
    // top.conf declares t, star.conf declares s, inner.conf declares i and includes deeper.conf declaring d
    // expect t, s unconditional; i, d conditional
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib discovery::tests::records_host_lines discovery::tests::includes_inside`
Expected: compile error (no field `line`).

- [ ] **Step 3: Implement**

Track per scanned file a `conditional` flag passed into `scan_include`, and inside `scan_contents` a `in_block` flag set by each `Host`/`Match` line (cleared by `Host *` and `Match all`; `parse_directive` gains a `ConfigDirective::Match(Vec<String>)` variant so the scanner can see it).

- [ ] **Step 4: Run discovery and app tests**

Run: `cargo test --lib discovery:: app::`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add src/discovery.rs src/app.rs
git commit -m "feat: record where external hosts are declared"
```

---

### Task 3: Raw effective configuration from OpenSSH

**Files:**
- Modify: `src/openssh.rs`
- Test: `src/openssh.rs` `mod tests`

**Interfaces:**
- Produces: `pub(crate) fn effective(&self, entry: &Path, alias: &str, cancelled: Option<&AtomicBool>) -> Result<String, OpenSshError>`: runs `ssh -G -F <entry> -- <alias>` through `checked_output` and returns stdout as UTF-8 (`OpenSshError::InvalidOutput` otherwise).

- [ ] **Step 1: Write the failing test** `effective_returns_raw_output_of_ssh_g_with_the_entry`: with a fake ssh script that prints its arguments, assert the output equals `"-G\n-F\n<entry>\n--\ndev\n"`.
- [ ] **Step 2: Run it** — `cargo test --lib openssh::tests::effective_returns` — expect compile error.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run it** — expect PASS.
- [ ] **Step 5: Commit** — `git commit -m "feat: read the raw effective configuration of a host"`

---

### Task 4: Block scanner and file edits

**Files:**
- Create: `src/adopt/scan.rs` (declared from `src/adopt.rs` as `mod scan;`; create `src/adopt.rs` with just `mod scan;` and `pub(crate) use scan::*;` for now and add `pub mod adopt;` to `src/lib.rs`)
- Test: `src/adopt/scan.rs` `mod tests`

**Interfaces:**
- Produces:
  - `pub(crate) struct Block { pub host_line: usize, pub aliases: Vec<String>, pub removed: Range<usize>, pub parsed: Result<(Host, HostComments), String> }` — `host_line` 1-based; `removed` is a 0-based half-open range of line indices (leading comments through the last directive, plus one directly following blank line); `parsed` is the block converted to a managed host or the first directive-level refusal reason.
  - `pub(crate) fn scan_blocks(text: &str) -> Vec<Block>` — lines split on `\n`; a trailing `\r` is ignored for parsing only.
  - `pub(crate) fn remove_lines(contents: &[u8], ranges: &[Range<usize>]) -> Vec<u8>` — removes whole lines (with their `\n`) for each range; when the removed range reaches the end of a file without a final newline, also removes the `\n` (or `\r\n`) ending the line before it so no empty last line is created.
  - `pub(crate) fn insert_after_first_line(contents: &[u8], text: &[u8]) -> Vec<u8>` — inserts `text` (already ending in `\n`) after the first line; a first line without `\n` gets one.

Refusal reasons in `parsed` (exact copy, first applicable wins): `Host line declares N aliases` (N ≥ 2), `<Directive> is not supported by Portico` (the directive as written), `<Directive> is repeated on line L`, `invalid <field>: <HostError>` (the `HostError` display), `missing value for <Directive> on line L`. A missing `HostName` becomes the alias. Directive names match case-insensitively and accept `Key=value` and `Key = value` (reuse `crate::discovery::parse_directive` tokenizing for the key; the value is everything after the key and separator, trimmed, then parsed with the managed `parse_value` rules, except `ProxyJump` which keeps the raw remainder). Comments: leading (directly above, no blank), inner lines between `Host` and the last directive attach to the next directive, a trailing `# ...` on a directive line (found with `crate::discovery::split_comment`) is appended after that directive's inner comments.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn block_with_leading_inner_and_trailing_comments() {
    let text = "Host other\n  HostName o\n\n# work box\nHost dev\n  # the real name\n  HostName dev.example\n\n  User bob  # old account\n# next section\nHost z\n  HostName z\n";
    let blocks = scan_blocks(text);
    let dev = &blocks[1];
    assert_eq!(dev.host_line, 5);
    assert_eq!(dev.removed, 3..9); // "# work box" .. "  User bob ..." ; next line is a comment, not blank
    let (host, comments) = dev.parsed.clone().unwrap();
    assert_eq!((host.alias(), host.hostname(), host.user()), ("dev", "dev.example", Some("bob")));
    assert_eq!(comments.leading, vec!["# work box"]);
    assert_eq!(comments.fields[&HostField::Hostname], vec!["# the real name"]);
    assert_eq!(comments.fields[&HostField::User], vec!["# old account"]);
}
#[test] fn removed_range_takes_one_following_blank_line() { /* "Host a\n  HostName x\n\n\nHost b\n" -> a.removed == 0..3 */ }
#[test] fn comment_separated_by_blank_line_is_not_leading() { /* "# header\n\nHost a\n  HostName x\n" -> a.removed == 2..4, leading empty */ }
#[test] fn block_ends_at_match_and_at_end_of_file() { /* "Host a\n  User u\nMatch all\n  User v\n" -> a.removed == 0..2, hostname == "a" */ }
#[test] fn key_equals_value_forms_and_case() { /* "host a\n  hostname=x\n  USER = bob\n  Port=2222\n" parses */ }
#[test] fn refusal_reasons() {
    // "Host a b\n  HostName x\n" -> Err("Host line declares 2 aliases")
    // "Host a\n  ForwardAgent yes\n" -> Err("ForwardAgent is not supported by Portico")
    // "Host a\n  User x\n  user y\n" -> Err("user is repeated on line 3")
    // "Host a\n  Port 99999\n" -> Err starting with "invalid port"
}
#[test] fn remove_lines_keeps_every_other_byte_including_crlf() {
    let input = b"A\r\nHost a\r\n  HostName x\r\n\r\nB\r\n";
    assert_eq!(remove_lines(input, &[1..4]), b"A\r\nB\r\n");
}
#[test] fn remove_last_block_without_trailing_newline() {
    assert_eq!(remove_lines(b"A\nHost a\n  HostName x", &[1..3]), b"A\n");
}
#[test] fn remove_two_adjacent_ranges() { /* two blocks back to back in one file, both removed, rest identical */ }
#[test] fn insert_after_first_line_variants() {
    assert_eq!(insert_after_first_line(b"Include x\nrest\n", b"Host a\n\n"), b"Include x\nHost a\n\nrest\n");
    assert_eq!(insert_after_first_line(b"Include x", b"Host a\n"), b"Include x\nHost a\n");
}
```

- [ ] **Step 2: Run them to verify they fail** — `cargo test --lib adopt::scan` — compile errors.
- [ ] **Step 3: Implement `scan_blocks`, `remove_lines`, `insert_after_first_line`.** Make `parse_value` in `managed.rs` `pub(crate)`.
- [ ] **Step 4: Run** — `cargo test --lib adopt::scan` — all pass.
- [ ] **Step 5: Commit** — `git commit -m "feat: scan external host blocks for adoption"`

---

### Task 5: Line diff and preview rendering

**Files:**
- Create: `src/adopt/diff.rs`
- Test: same file

**Interfaces:**
- Produces:
  - `pub struct Preview { pub files: Vec<FilePreview> }`, `pub struct FilePreview { pub path: PathBuf, pub lines: Vec<PreviewLine> }`, `pub enum PreviewLine { Removed { number: usize, text: String }, Added { number: usize, text: String } }` (numbers 1-based: in the old file for `Removed`, in the new file for `Added`; `text` without line ending or trailing `\r`).
  - `pub(crate) fn diff(path: &Path, old: &str, new: &str) -> FilePreview` — strips common prefix and suffix lines, then an LCS over the middle; when the middle exceeds 4 000 000 cells, it reports the whole middle as removed then added.
  - `impl fmt::Display for Preview`: for each file, a line `<path>` (home shown as `~`, via the existing helper used for jump host suggestions if one is public, otherwise `$HOME` prefix replacement), then each line as `  -<number>: <text>` or `  +<number>: <text>`, with a blank line between files.

- [ ] **Step 1: Write the failing tests** — `pure_deletion_reports_old_line_numbers` (old 5 lines, lines 2–3 removed → `Removed{2}`, `Removed{3}`); `pure_insertion_reports_new_line_numbers`; `identical_text_has_no_lines`; `display_groups_by_file_with_signs` (exact string comparison of two files).
- [ ] **Step 2: Run** — `cargo test --lib adopt::diff` — compile errors.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — pass.
- [ ] **Step 5: Commit** — `git commit -m "feat: render exact previews of configuration changes"`

---

### Task 6: Adoption and export plans

**Files:**
- Modify: `src/adopt.rs`, `src/managed.rs` (make `pub(crate)`: `read_managed`, `ensure_ready`, `entry_target` (returning `TransactionError` via a wrapper is fine), `substitute_include`, `write_unique`, `PendingFile::rename_to`, `sync_dir`, `open_lock`, `acquire_lock`, `quote_path`, `line_count`)
- Test: `src/adopt.rs` `mod tests`

**Interfaces:**
- Consumes: Tasks 1–5.
- Produces:
  - `pub enum Direction { Adopt, Export }`
  - `pub struct Candidate { pub alias: String, pub source: PathBuf, pub reason: Option<String> }` (`reason` None = eligible)
  - `pub struct MigrationPlan { pub direction: Direction, pub candidates: Vec<Candidate>, /* private: snapshot: BTreeMap<PathBuf, Vec<u8>> (managed fragment, entry target, every source target), moves: BTreeMap<String /*lowercase alias*/, Move> */ }` where private `Move { file: PathBuf /*symlink target*/, removed: Range<usize>, host: Host, comments: HostComments }` for adoption, and for export `Move { host, comments }` with the entry target as file.
  - `impl MigrationPlan { pub fn eligible(&self) -> impl Iterator<Item = &Candidate>; pub fn preview(&self, selection: &[String]) -> Result<Preview, AdoptError> }`
  - `pub type Effective<'a> = &'a dyn Fn(&Path, &str) -> Result<String, String>;` (entry path, alias → `ssh -G` output or stderr)
  - `impl ManagedStore { pub fn adoption_plan(&self, effective: Effective<'_>) -> Result<MigrationPlan, AdoptError>; pub fn export_plan(&self, effective: Effective<'_>) -> Result<MigrationPlan, AdoptError> }`
  - `pub enum AdoptError` (thiserror): `Transaction(#[from] TransactionError)`, `Discovery(#[from] DiscoveryError)`, `UnknownHost(String)` "unknown host: {0}", `Ineligible { alias, reason }` "cannot adopt {alias}: {reason}" (export: "cannot export ..."; carry the direction or two variants), `Stale` (exact copy above), `Changed { alias, removed: Vec<String>, added: Vec<String> }` "effective configuration of {alias} would change: -… +…", `OpenSsh { alias, stderr }`, `Io { path, source }`, `Rollback { original: Box<AdoptError>, failures: Vec<String>, backups: Vec<(PathBuf, PathBuf)> }` (message lists each `restore <file> from <backup>` and says the state is uncertain).
  - Helper in `managed.rs`: `pub(crate) fn stage_entry(&self, entry: &[u8], managed: &Path) -> Result<PendingFile, TransactionError>` — `substitute_include` toward `managed`, then the `Match all` + system-config append exactly as `preview` does, written with `write_unique` in `ssh_dir` (stem `.config.portico-adopt-entry`). Refactor `preview` to use it.

Eligibility order (spec "What is adoptable"), reasons exact:
1. block `parsed` error for "aliases" (`Host line declares N aliases`)
2. `declared in N files` (from `DiscoveryWarning::DuplicateAlias`), `already managed`, `host inventory is incomplete` (when `aliases_complete` is false)
3. other `parsed` errors (directives and values)
4. `not a regular file`, `not valid UTF-8`, `owned by another user`, `file is read-only`, `directory is read-only` (use `libc::access(W_OK)` on file and parent; ownership via `MetadataExt::uid` vs `libc::geteuid`), `included conditionally (inside a Host or Match block)`
5. losslessness, done in one batch: render one candidate managed fragment containing every host that passed 1–4, stage `before = stage_entry(entry, paths.managed)` and `after = stage_entry(entry, candidate)`, and for each alias compare `effective(before, alias)` with `effective(after, alias)`. A difference gives `effective configuration would change (-<line>, +<line>, …)` listing lines only in before with `-` and only in after with `+`; an error gives `OpenSSH could not resolve it: <first stderr line>`.

Export plan: every managed host is a candidate with `source = paths.managed`; checks 4 apply to the entry target (reason applies to all hosts), then losslessness with `after = stage_entry(insert_after_first_line(entry, blocks), candidate_without_hosts)`.

`preview(selection)`: unknown or ineligible alias → `UnknownHost`/`Ineligible`; empty selection → empty `Preview`. Adoption: `diff(managed, old, new)` where new is the managed config with the selected hosts inserted (comments included) and rendered, then one `diff` per touched source file with its selected ranges removed. Export: `diff(entry)` with the blocks inserted, then `diff(managed)`. Exported blocks are rendered with the managed renderer (alphabetical, comments), joined by one blank line, followed by one blank line.

- [ ] **Step 1: Write the failing tests** (a `fixture()` helper builds a temp home with `ConfigPaths::from_home`, an initialized entry `Include ~/.ssh/portico/config\n` plus content, an empty managed fragment, and `system_config` pointed at a missing path; `same: Effective` returns `Ok(String::new())`):

```rust
#[test] fn eligible_block_in_entry_is_listed_with_its_source() { /* "Host dev\n  HostName d\n" -> one candidate, reason None, source == entry */ }
#[test] fn reasons_follow_the_spec_order() {
    // multi-alias line, ForwardAgent, duplicate across two files, managed collision, conditional include,
    // read-only include file (chmod 0o444), file owned by root is skipped when not running as root? -> use a
    // symlink to /etc/hostname-like root-owned file only if it exists; otherwise assert via a unit helper
    // `source_problem(&Metadata, euid)` given a fake uid.
}
#[test] fn lossless_check_rejects_a_changed_effective_configuration() {
    let effective: Effective = &|entry, _| Ok(if fs::read_to_string(entry).unwrap().contains("adopt-candidate") { "user alice\n".into() } else { "user root\n".into() });
    // reason == "effective configuration would change (-user root, +user alice)"
}
#[test] fn preview_of_adoption_lists_added_and_removed_lines() { /* exact Display string for one host with a leading comment */ }
#[test] fn preview_of_export_inserts_below_the_include() { /* exact Display string */ }
#[test] fn preview_rejects_unknown_and_ineligible_aliases() {}
```

- [ ] **Step 2: Run** — `cargo test --lib adopt::tests` — compile errors.
- [ ] **Step 3: Implement.** Candidate temp files for the plan-time check use stem `.config.portico-adopt-candidate` in `portico_dir` and are dropped before returning.
- [ ] **Step 4: Run** — `cargo test --lib adopt:: managed::` — pass.
- [ ] **Step 5: Commit** — `git commit -m "feat: plan adoption and export with eligibility reasons"`

---

### Task 7: The migration transaction

**Files:**
- Modify: `src/adopt.rs`
- Test: `src/adopt.rs` `mod tests`

**Interfaces:**
- Produces: `impl ManagedStore { pub fn migrate(&self, plan: &MigrationPlan, selection: &[String], effective: Effective<'_>, cancelled: Option<&AtomicBool>) -> Result<Vec<String>, AdoptError> }` returning the migrated aliases as declared, in alphabetical order.
- Test-only fault injection: `#[cfg(test)] thread_local! { static FAIL_AT: Cell<Option<FailPoint>> }` with `enum FailPoint { ManagedRename, FileRename(usize), DirectorySync, Verify, Restore }`, checked at the matching step (`FileRename(n)` = the n-th user file rename, `Restore` = the first restore write).

Steps exactly as spec "The transaction" 1–8. Details the implementer cannot infer:
- Re-check compares each snapshot entry with a fresh `fs::read` of the same path; any difference or read error → `Stale`. It runs once after taking the lock and again for each user file immediately before its rename.
- `preview(selection)` validation runs first (unknown/ineligible errors before the lock).
- Activation of the managed fragment writes the previous managed bytes to `paths.backup` (as `transact` does) before renaming the candidate.
- Export activation order: entry target first, managed second.
- Verify: `stage_entry(fs::read(entry), paths.managed)` after activation, compare `effective` per alias with the step-4 "before" outputs; difference → `Changed`, then rollback.
- Rollback restores in reverse activation order by writing each backup's bytes with `write_unique` + rename (mode kept), then syncs directories; then deletes backups. If any restore fails → `Rollback` with every (file, backup) pair still present.
- Backups are created after validation and before the first rename; on any failure before activation they are deleted.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn adopting_moves_exactly_the_shown_lines() {
    // entry: "Include ~/.ssh/portico/config\n\nHost keep\n  HostName k\n\n# box\nHost dev\n  HostName d\n  User bob\n\nHost after\n  HostName a\n"
    // migrate(["dev"]) -> entry == "Include ~/.ssh/portico/config\n\nHost keep\n  HostName k\n\nHost after\n  HostName a\n"
    // managed contains "# box\nHost dev\n    HostName d\n    User bob\n"; no *.portico-adopt-backup-* files remain
}
#[test] fn adopting_from_an_included_file_and_the_entry_in_one_go() {}
#[test] fn two_blocks_in_one_file_are_removed_together() {}
#[test] fn stale_snapshot_is_refused_and_nothing_changes() { /* append a line to the entry after planning */ }
#[test] fn validation_difference_at_apply_time_changes_nothing() {}
#[test] fn each_activation_failure_rolls_back_byte_for_byte() {
    for point in [FailPoint::ManagedRename, FailPoint::FileRename(0), FailPoint::FileRename(1), FailPoint::DirectorySync, FailPoint::Verify] {
        // fresh fixture with two source files; snapshot all bytes; inject; migrate -> Err; assert all bytes equal and no backups remain
    }
}
#[test] fn failed_restore_keeps_backups_and_names_them() { /* FileRename(1) + Restore -> AdoptError::Rollback whose message contains each backup path */ }
#[test] fn symlinked_source_keeps_the_link() { /* entry is a symlink to dotfiles/config; adopt; link still a symlink, target updated */ }
#[test] fn export_inserts_below_include_and_empties_managed() {}
#[test] fn adopt_then_export_round_trips_comments() { /* resulting entry contains the same comment lines and Host block (Portico formatting) */ }
#[test] fn export_through_a_symlinked_entry_keeps_the_link() {}
#[test] fn concurrent_portico_mutation_waits_for_the_lock() { /* hold paths.lock with fs2 in a thread, start migrate, release, assert it completes */ }
```

- [ ] **Step 2: Run** — `cargo test --lib adopt::tests` — the new tests fail (no `migrate`).
- [ ] **Step 3: Implement `migrate`.**
- [ ] **Step 4: Run** — `cargo test --lib` — all pass.
- [ ] **Step 5: Commit** — `git commit -m "feat: adopt and export hosts in one rollback-safe transaction"`

---

### Task 8: App operations and CLI commands

**Files:**
- Modify: `src/app.rs`, `src/cli.rs`
- Test: `src/cli.rs` `mod tests`, `tests/cli.rs`

**Interfaces:**
- Consumes: Tasks 6–7.
- Produces:
  - `AppError::Adopt(#[from] AdoptError)` (transparent).
  - `App::adoption_plan(&self) -> Result<MigrationPlan, AppError>`, `App::export_plan(&self) -> Result<MigrationPlan, AppError>`, `App::migrate(&self, plan: &MigrationPlan, selection: &[String]) -> Result<Vec<String>, AppError>`, and `pub(crate) fn migrate_cancellable(.., cancelled: &AtomicBool)`; each passes `&|entry, alias| self.openssh.effective(entry, alias, cancelled).map_err(stderr_of)`.
  - `Command::Adopt { aliases: Vec<String>, #[arg(short = 'A', long, conflicts_with = "aliases")] all: bool, #[arg(short = 'y', long)] yes: bool }` and `Command::Export { .. same .. }`.
  - `fn parse_selection(answer: &str, count: usize) -> Result<Vec<usize>, String>` — accepts `all`, `none`, empty, and comma-separated numbers and `a-b` ranges (1-based, whitespace ignored); out of range or malformed → `Err("invalid selection: <answer>")`.

Flow (one function `run_migration(direction, ...)` shared by both commands), copy exact:
- Without aliases and `--all`: non-interactive → error `adopt requires host aliases or --all in non-interactive mode` (`export` likewise). Interactive → print `Eligible hosts:` then `  1. <alias>  <source>` lines, then if any `Not eligible:` and `  <alias>  <source>: <reason>` lines, then the selection prompt. No eligible host → print `No host can be adopted.` (`exported`) plus the ineligible list and exit success.
- `--all`: every eligible host; each ineligible one printed as `Skipped <alias>: <reason>`.
- Empty selection → `Nothing to adopt.` / `Nothing to export.`, success.
- Print the preview (`Preview` Display), then confirmation unless `--yes`; non-interactive without `--yes` → error `adopt requires --yes in non-interactive mode`; declined → `Adoption cancelled.` / `Export cancelled.`, success.

- [ ] **Step 1: Write the failing tests**
  - `src/cli.rs`: `parse_selection_accepts_all_none_numbers_and_ranges` (`"1,3-4"`, count 5 → `[0,2,3]`; `""` and `"none"` → `[]`; `"all"` → `[0..count]`; `"0"`, `"6"`, `"2-1"`, `"x"` → Err).
  - `tests/cli.rs` (TestEnv; init first with `portico init -y`; external includes use absolute paths):
    - `adopt_interactive_selection_moves_the_chosen_hosts` (`run_with_stdin(["adopt"], "2\ny\n")` with three eligible hosts; only the second moves; stdout contains `Adopted: `)
    - `adopt_requires_yes_without_a_terminal` (`run(["adopt", "dev"])` fails with the exact message; files unchanged)
    - `adopt_all_skips_ineligible_hosts_with_reasons` (`adopt --all --yes`; stdout contains `Skipped multi: Host line declares 2 aliases`)
    - `adopt_named_ineligible_host_fails_without_changes`
    - `export_moves_hosts_back_below_the_include`
    - `adopt_refuses_a_block_whose_effective_configuration_would_change`: install a real-ssh wrapper `install_program("ssh", "#!/bin/sh\nexec /usr/bin/ssh \"$@\"\n")`, an entry with `Host *\n  User root\n` above `Host dev\n  HostName d\n  User alice\n`; `adopt dev -y` fails with `effective configuration would change`; files unchanged. Skip (return early with a printed note) when `/usr/bin/ssh` is absent.
- [ ] **Step 2: Run** — `cargo test --lib cli::tests::parse_selection && cargo test --test cli adopt_ export_` — fail.
- [ ] **Step 3: Implement.** Also add the `AppError::Adopt` arm to `may_have_changed_inventory` in `src/tui.rs` (`Io` and `Rollback` → true, everything else false).
- [ ] **Step 4: Run** — `cargo test --all-targets` — pass.
- [ ] **Step 5: Commit** — `git commit -m "feat: add portico adopt and portico export"`

---

### Task 9: TUI migrate screen

**Files:**
- Create: `src/tui/migrate.rs`
- Modify: `src/tui.rs` (only the items listed in Global Constraints)
- Test: `src/tui/migrate.rs` `mod tests`, one key-routing test in `src/tui.rs` tests

**Interfaces:**
- Consumes: `MigrationPlan`, `Preview`, `Direction`.
- Produces in `migrate.rs`:
  - `pub(super) struct MigrateState { pub plan: MigrationPlan, selected: BTreeSet<usize>, cursor: usize, preview: Option<(Preview, usize /*scroll*/)> }` with `pub(super) fn new(plan: MigrationPlan, preselect: Option<&str>) -> Self` (preselects that alias when eligible, cursor on it).
  - `pub(super) enum MigrateAction { None, Close, Apply(Vec<String>) }`
  - `pub(super) fn handle_key(&mut self, key: KeyEvent) -> MigrateAction` — checklist: Up/Down/`k`/`j` move, Space toggles (no-op on ineligible), `a` selects every eligible, Enter with a non-empty selection opens the preview (an empty one sets nothing), Esc → `Close`. Preview: Up/Down scroll one line, PageUp/PageDown ten, `y` → `Apply(selected aliases)`, Esc back to the checklist.
  - `pub(super) fn lines(&self, width: usize) -> Vec<Line<'static>>` — checklist rows `[x] alias  source` / `[ ] alias  source`, ineligible rows `    alias  reason` dimmed (`Modifier::DIM`), cursor row reversed; preview: the `Preview` Display lines from the scroll offset.
  - `pub(super) fn title(&self) -> &'static str` — `Adopt hosts` / `Export hosts`; `pub(super) fn footer(&self) -> &'static str` — checklist `↑↓ move  Space toggle  a all  Enter preview  Esc cancel`, preview `↑↓ scroll  y apply  Esc back`.
- In `tui.rs`: `Mode::Migrate`; `UiState.migrate: Option<MigrateState>`; browse `KeyCode::Char('m')` → `UiAction::OpenMigrate { direction, preselect }` (direction from the selected host's ownership; nothing selected → no-op); the event loop builds the plan with `app.adoption_plan()` / `app.export_plan()` (error → status line) and enters `Mode::Migrate`; `MigrateAction::Apply(aliases)` → `UiAction::Migrate(aliases)` → `app.migrate_cancellable(&plan, &aliases, terminated)` → `finish_mutation(.., status = "Adopted: a, b" / "Exported: a, b")`, which returns to Browse; `Close` → Browse. Browse footers gain `m adopt` (external) / `m export` (managed) after `a add`.

- [ ] **Step 1: Write the failing tests** — `space_toggles_only_eligible_rows`, `a_selects_every_eligible_host`, `enter_without_selection_stays_on_the_checklist`, `y_in_preview_applies_the_selection`, `esc_steps_back_then_closes`, `preselected_host_is_checked`, and in `tui.rs` `m_opens_migration_for_the_selected_host` (asserts the returned `UiAction::OpenMigrate` for an external and a managed selection). Build plans for these tests through a `#[cfg(test)] pub(crate) fn MigrationPlan::for_tests(direction, candidates)` constructor in `adopt.rs`.
- [ ] **Step 2: Run** — `cargo test --lib tui::` — fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — `cargo test --all-targets` — pass.
- [ ] **Step 5: Commit** — `git commit -m "feat: adopt and export hosts from the TUI"`

---

### Task 10: Documentation, real OpenSSH check, full CI

**Files:**
- Modify: `README.md` (commands section: `portico adopt`, `portico export`, the `m` key; one paragraph on what is adoptable and that export is the way back), `tests/real_openssh.rs` (new ignored test), `tests/docs.rs` (assert README documents adoption), the spec status line → `**Status:** Approved; implemented`
- Test: `tests/docs.rs`, `tests/real_openssh.rs`

- [ ] **Step 1: Write the failing doc test** `readme_documents_adoption`: README contains `portico adopt`, `portico export` and `m`.
- [ ] **Step 2: Run** — `cargo test --test docs readme_documents_adoption` — FAIL.
- [ ] **Step 3: Update README and spec status; add `#[ignore]` test `adopt_and_export_keep_ssh_g_identical_with_real_openssh`** that, in a TestEnv with the real-ssh wrapper, adopts and exports a host with a comment and compares `ssh -G -F <entry> dev` before, between and after.
- [ ] **Step 4: Run every CI check as a regular user**

Run: `scripts/ci-checks.sh fmt && scripts/ci-checks.sh clippy && scripts/ci-checks.sh test && cargo test --test real_openssh -- --ignored adopt_and_export`
Expected: all succeed.

- [ ] **Step 5: Commit** — `git commit -m "docs: document host adoption and export"`
