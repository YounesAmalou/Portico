mod diff;
use diff::diff;
pub use diff::{FilePreview, Preview, PreviewLine};
mod scan;
pub(crate) use scan::*;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs;
use std::io;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use thiserror::Error;

use crate::discovery::{DiscoveryError, DiscoveryWarning, discover};
use crate::host::Host;
use crate::managed::{
    HostComments, ManagedConfig, ManagedStore, PendingFile, TransactionError, acquire_lock,
    normalize_path, open_lock, render_host, symlink_target, sync_dir, write_unique,
};

/// Whether hosts move into the managed fragment or out of it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Adopt,
    Export,
}

impl Direction {
    fn verb(self) -> &'static str {
        match self {
            Self::Adopt => "adopt",
            Self::Export => "export",
        }
    }
}

/// A host a plan considered, with the first rule it breaks; `reason` is `None` when eligible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub alias: String,
    /// The file that declares it, as discovery reported it (the managed fragment for export).
    pub source: PathBuf,
    pub reason: Option<String>,
}

/// Every candidate of one direction with what moving each eligible one would change, computed
/// from the exact bytes it read.
#[derive(Clone, Debug)]
pub struct MigrationPlan {
    pub direction: Direction,
    pub candidates: Vec<Candidate>,
    /// The managed fragment.
    managed: PathBuf,
    /// The entry config, symlinks resolved, as `file_key` names it.
    entry: PathBuf,
    /// The exact bytes of every file read: the managed fragment, the entry config and every
    /// source of a candidate. User files are keyed by `file_key` of their symlink target.
    snapshot: BTreeMap<PathBuf, Vec<u8>>,
    /// The move of every eligible host, keyed by lowercase alias.
    moves: BTreeMap<String, Move>,
}

/// One eligible host and where it moves.
#[derive(Clone, Debug)]
struct Move {
    /// The user file the block leaves (adoption) or enters (export: the entry config), symlinks
    /// resolved; a key of the snapshot.
    file: PathBuf,
    /// Adoption: the 0-based half-open line range of `file` the block occupies. Export: empty,
    /// the blocks go below the first line.
    removed: Range<usize>,
    host: Host,
    comments: HostComments,
}

/// The `ssh -G` output for an alias when reading an entry config, or OpenSSH's stderr.
pub type Effective<'a> = &'a dyn Fn(&Path, &str) -> Result<String, String>;

#[derive(Debug, Error)]
pub enum AdoptError {
    #[error(transparent)]
    Transaction(#[from] TransactionError),
    #[error(transparent)]
    Discovery(#[from] DiscoveryError),
    #[error("unknown host: {0}")]
    UnknownHost(String),
    #[error("cannot {} {alias}: {reason}", .direction.verb())]
    Ineligible {
        direction: Direction,
        alias: String,
        reason: String,
    },
    #[error(
        "the configuration changed since the preview; run the command again to see a fresh preview"
    )]
    Stale,
    #[error("effective configuration of {alias} would change: {}", signed_lines(.removed, .added))]
    Changed {
        alias: String,
        removed: Vec<String>,
        added: Vec<String>,
    },
    #[error("OpenSSH could not resolve {alias}: {stderr}")]
    OpenSsh { alias: String, stderr: String },
    #[error("filesystem error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "{original}; rollback also failed, so the state is uncertain; {}rollback: {}",
        recovery(.backups),
        .failures.join("; ")
    )]
    Rollback {
        original: Box<AdoptError>,
        failures: Vec<String>,
        /// Each file still to restore, with the backup holding its previous bytes.
        backups: Vec<(PathBuf, PathBuf)>,
    },
}

fn io_error(path: &Path, source: io::Error) -> AdoptError {
    AdoptError::Io {
        path: path.into(),
        source,
    }
}

/// `restore F from B; ` for each file still to restore, or nothing when there is none.
fn recovery(backups: &[(PathBuf, PathBuf)]) -> String {
    backups
        .iter()
        .map(|(file, backup)| format!("restore {} from {}; ", file.display(), backup.display()))
        .collect()
}

/// The path a plan keys `file` by, symlinks already followed: its directory canonicalized, so
/// that two spellings of one file (`a/../x` and `x`, or a path through a symlinked directory)
/// are one key. The directory is resolved by the filesystem rather than lexically because `..`
/// after a symlinked directory leads elsewhere; without a resolvable directory the path is only
/// normalized lexically.
fn file_key(file: PathBuf) -> PathBuf {
    let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else {
        return normalize_path(&file);
    };
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    match fs::canonicalize(dir) {
        Ok(dir) => dir.join(name),
        Err(_) => normalize_path(&file),
    }
}

/// `-line` for each removed and `+line` for each added line, comma separated.
fn signed_lines(removed: &[String], added: &[String]) -> String {
    removed
        .iter()
        .map(|line| format!("-{line}"))
        .chain(added.iter().map(|line| format!("+{line}")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The lines only `before` has and the lines only `after` has, each in its own order.
fn changed_lines(before: &str, after: &str) -> (Vec<String>, Vec<String>) {
    let only = |text: &str, other: &str| {
        let other = other.lines().collect::<BTreeSet<_>>();
        text.lines()
            .filter(|line| !other.contains(line))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    (only(before, after), only(after, before))
}

/// The losslessness reason for one alias from its effective configuration before and after.
fn lossless_reason(
    before: Result<String, String>,
    after: Result<String, String>,
) -> Option<String> {
    let (before, after) = match (before, after) {
        (Ok(before), Ok(after)) => (before, after),
        (Err(stderr), _) | (_, Err(stderr)) => {
            let line = stderr.lines().next().unwrap_or_default().trim();
            return Some(format!("OpenSSH could not resolve it: {line}"));
        }
    };
    if before == after {
        return None;
    }
    let (removed, added) = changed_lines(&before, &after);
    if removed.is_empty() && added.is_empty() {
        return Some("effective configuration would change".into());
    }
    Some(format!(
        "effective configuration would change ({})",
        signed_lines(&removed, &added)
    ))
}

/// The first rule `path` breaks as a file to rewrite: a regular file, valid UTF-8, owned by
/// `euid`, and writable along with its directory.
fn source_problem(path: &Path, contents: &[u8], euid: u32) -> Option<String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => return Some(format!("cannot inspect {}: {error}", path.display())),
    };
    if !metadata.is_file() {
        return Some("not a regular file".into());
    }
    if std::str::from_utf8(contents).is_err() {
        return Some("not valid UTF-8".into());
    }
    if metadata.uid() != euid {
        return Some("owned by another user".into());
    }
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    for (what, path) in [("file", path), ("directory", dir)] {
        if let Err(error) = writable(path) {
            return Some(match error.raw_os_error() {
                Some(libc::EACCES | libc::EPERM | libc::EROFS) => format!("{what} is read-only"),
                _ => format!(
                    "cannot check whether {} is writable: {error}",
                    path.display()
                ),
            });
        }
    }
    None
}

fn writable(path: &Path) -> io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    if unsafe { libc::access(path.as_ptr(), libc::W_OK) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Managed blocks as export writes them below the Portico Include: alphabetical, separated and
/// followed by one blank line.
fn export_blocks<'a>(moves: impl IntoIterator<Item = &'a Move>) -> String {
    let mut moves = moves.into_iter().collect::<Vec<_>>();
    moves.sort_by_key(|item| item.host.alias().to_ascii_lowercase());
    let blocks = moves
        .into_iter()
        .map(|item| render_host(&item.host, Some(&item.comments)))
        .collect::<Vec<_>>();
    format!("{}\n\n", blocks.join("\n\n"))
}

/// A discovered file, read once.
struct Source {
    /// The file, symlinks resolved.
    target: PathBuf,
    /// Its blocks, or why it could not be read.
    blocks: Result<Vec<Block>, String>,
    /// The first rule it breaks as a file to rewrite.
    problem: Option<String>,
}

impl Source {
    fn read(path: &Path, euid: u32, snapshot: &mut BTreeMap<PathBuf, Vec<u8>>) -> Self {
        let target = match symlink_target(path) {
            Ok(target) => file_key(target),
            Err((at, error)) => {
                return Self {
                    target: path.into(),
                    blocks: Err(format!("cannot resolve {}: {error}", at.display())),
                    problem: None,
                };
            }
        };
        let bytes = match snapshot.get(&target) {
            Some(bytes) => bytes.clone(),
            None => match fs::read(&target) {
                Ok(bytes) => bytes,
                Err(error) => {
                    return Self {
                        blocks: Err(format!("cannot read {}: {error}", target.display())),
                        target,
                        problem: None,
                    };
                }
            },
        };
        let blocks = scan_blocks(&String::from_utf8_lossy(&bytes));
        let problem = source_problem(&target, &bytes, euid);
        snapshot.insert(target.clone(), bytes);
        Self {
            target,
            blocks: Ok(blocks),
            problem,
        }
    }
}

impl ManagedStore {
    /// Every discovered external host with the first adoption rule it breaks, and what adopting
    /// each eligible one would change. Nothing is written apart from temporary files for the
    /// losslessness check, which are removed before returning.
    pub fn adoption_plan(&self, effective: Effective<'_>) -> Result<MigrationPlan, AdoptError> {
        let paths = self.paths();
        let (managed_bytes, config) = self.read_managed()?;
        let entry = file_key(self.entry_file()?);
        let entry_bytes = fs::read(&entry).map_err(|source| io_error(&entry, source))?;
        let discovery = discover(&paths.entry, &paths.managed)?;
        let mut snapshot = BTreeMap::from([
            (paths.managed.clone(), managed_bytes),
            (entry.clone(), entry_bytes.clone()),
        ]);

        let duplicates = discovery
            .warnings
            .iter()
            .filter_map(|warning| match warning {
                DiscoveryWarning::DuplicateAlias { alias, sources } => {
                    Some((alias.to_ascii_lowercase(), sources))
                }
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        let euid = effective_uid();
        let mut sources = BTreeMap::new();
        let mut candidates = Vec::new();
        let mut moves = BTreeMap::new();
        for external in &discovery.hosts {
            let source = sources
                .entry(external.source.clone())
                .or_insert_with(|| Source::read(&external.source, euid, &mut snapshot));
            let key = external.alias.to_ascii_lowercase();
            let eligibility = (|| {
                let blocks = source.blocks.as_ref().map_err(Clone::clone)?;
                let block = blocks
                    .iter()
                    .find(|block| block.host_line == external.line)
                    .ok_or_else(|| format!("no Host block on line {}", external.line))?;
                if let (true, Err(reason)) = (block.aliases.len() != 1, &block.parsed) {
                    return Err(reason.clone());
                }
                if let Some(sources) = duplicates.get(&key) {
                    let files = sources.iter().collect::<BTreeSet<_>>().len();
                    return Err(if files > 1 {
                        format!("declared in {files} files")
                    } else {
                        format!("declared {} times in this file", sources.len())
                    });
                }
                if config.find(&external.alias).is_some() {
                    return Err("already managed".to_owned());
                }
                if !discovery.aliases_complete {
                    return Err("host inventory is incomplete".to_owned());
                }
                let (host, comments) = block.parsed.clone()?;
                if let Some(problem) = &source.problem {
                    return Err(problem.clone());
                }
                if external.conditional {
                    return Err("included conditionally (inside a Host or Match block)".to_owned());
                }
                Ok(Move {
                    file: source.target.clone(),
                    removed: block.removed.clone(),
                    host,
                    comments,
                })
            })();
            candidates.push(Candidate {
                alias: external.alias.clone(),
                source: external.source.clone(),
                reason: eligibility.as_ref().err().cloned(),
            });
            if let Ok(item) = eligibility {
                moves.insert(key, item);
            }
        }

        let mut candidate_config = config;
        for item in moves.values() {
            candidate_config
                .insert_with_comments(item.host.clone(), item.comments.clone())
                .map_err(TransactionError::Operation)?;
        }
        let mut plan = MigrationPlan {
            direction: Direction::Adopt,
            candidates,
            managed: paths.managed.clone(),
            entry,
            snapshot,
            moves,
        };
        let failures = self.lossless_failures(&plan, &entry_bytes, &candidate_config, effective);
        plan.reject(failures);
        Ok(plan)
    }

    /// Every managed host with the first export rule it breaks, and what exporting each eligible
    /// one to the entry config would change. Writes only temporary files, as `adoption_plan`.
    pub fn export_plan(&self, effective: Effective<'_>) -> Result<MigrationPlan, AdoptError> {
        let paths = self.paths();
        let (managed_bytes, config) = self.read_managed()?;
        let entry = file_key(self.entry_file()?);
        let entry_bytes = fs::read(&entry).map_err(|source| io_error(&entry, source))?;
        let problem = source_problem(&entry, &entry_bytes, effective_uid());

        let mut candidates = Vec::new();
        let mut moves = BTreeMap::new();
        let mut candidate_config = config.clone();
        for host in config.hosts() {
            candidates.push(Candidate {
                alias: host.alias().to_owned(),
                source: paths.managed.clone(),
                reason: problem.clone(),
            });
            if problem.is_none() {
                let (host, comments) = candidate_config
                    .remove_entry(host.alias())
                    .map_err(TransactionError::Operation)?;
                moves.insert(
                    host.alias().to_ascii_lowercase(),
                    Move {
                        file: entry.clone(),
                        removed: 0..0,
                        host,
                        comments,
                    },
                );
            }
        }
        let after_entry =
            insert_after_first_line(&entry_bytes, export_blocks(moves.values()).as_bytes());
        let mut plan = MigrationPlan {
            direction: Direction::Export,
            candidates,
            managed: paths.managed.clone(),
            snapshot: BTreeMap::from([
                (paths.managed.clone(), managed_bytes),
                (entry.clone(), entry_bytes),
            ]),
            entry,
            moves,
        };
        let failures = self.lossless_failures(&plan, &after_entry, &candidate_config, effective);
        plan.reject(failures);
        Ok(plan)
    }

    /// The losslessness rule for every move of `plan` in one batch: each alias must resolve
    /// identically against the current entry config and against `after_entry` including
    /// `candidate`. Returns the reason of each lowercase alias that fails.
    fn lossless_failures(
        &self,
        plan: &MigrationPlan,
        after_entry: &[u8],
        candidate: &ManagedConfig,
        effective: Effective<'_>,
    ) -> BTreeMap<String, String> {
        if plan.moves.is_empty() {
            return BTreeMap::new();
        }
        let paths = self.paths();
        let check = || -> Result<BTreeMap<String, String>, AdoptError> {
            let managed = write_unique(
                &paths.portico_dir,
                ".config.portico-adopt-candidate",
                candidate.render().as_bytes(),
                0o600,
            )
            .map_err(|source| io_error(&paths.portico_dir, source))?;
            let before = self.stage_entry(&plan.snapshot[&plan.entry], &paths.managed)?;
            let after = self.stage_entry(after_entry, managed.path())?;
            Ok(plan
                .moves
                .iter()
                .filter_map(|(key, item)| {
                    let alias = item.host.alias();
                    lossless_reason(
                        effective(before.path(), alias),
                        effective(after.path(), alias),
                    )
                    .map(|reason| (key.clone(), reason))
                })
                .collect())
        };
        check().unwrap_or_else(|error| {
            plan.moves
                .keys()
                .map(|key| {
                    let reason = format!("could not check the effective configuration: {error}");
                    (key.clone(), reason)
                })
                .collect()
        })
    }
}

/// A step of the transaction at which a test makes the next attempt fail.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FailPoint {
    ManagedRename,
    /// The n-th rename of a user file, from 0.
    FileRename(usize),
    DirectorySync,
    Verify,
    /// The first write of a restored file.
    Restore,
}

#[cfg(test)]
thread_local! {
    /// The armed fail points; each fails once.
    static FAIL_AT: std::cell::RefCell<Vec<FailPoint>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Fails with an injected error when the test armed `point`.
#[cfg(test)]
fn fail_point(point: FailPoint) -> io::Result<()> {
    let armed = FAIL_AT.with(|armed| {
        let mut armed = armed.borrow_mut();
        let index = armed.iter().position(|armed| *armed == point);
        index.map(|index| armed.remove(index)).is_some()
    });
    if armed {
        return Err(io::Error::other(format!("injected failure at {point:?}")));
    }
    Ok(())
}

/// One file the transaction replaces, in activation order.
struct Replacement {
    /// The file, symlinks resolved; a key of the plan's snapshot.
    path: PathBuf,
    dir: PathBuf,
    mode: u32,
    /// Whether it is the managed fragment rather than a user file.
    managed: bool,
    /// The new contents, staged beside the file until activated.
    candidate: Option<PendingFile>,
    /// The previous contents, beside the file, removed when dropped.
    backup: Option<PendingFile>,
}

/// The temporary name stem of `kind` for `path`: `.<file name>.portico-adopt-<kind>`.
fn temporary_stem(path: &Path, kind: &str) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    format!(".{name}.portico-adopt-{kind}")
}

fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.into(),
        _ => PathBuf::from("."),
    }
}

/// The `ssh -G` output of `alias` against `entry`, or the OpenSSH error naming the alias.
fn resolve(effective: Effective<'_>, entry: &Path, alias: &str) -> Result<String, AdoptError> {
    effective(entry, alias).map_err(|stderr| AdoptError::OpenSsh {
        alias: alias.into(),
        stderr: stderr.trim().into(),
    })
}

/// `Changed` when the effective configuration of `alias` differs between `before` and `after`.
fn require_same(alias: &str, before: &str, after: &str) -> Result<(), AdoptError> {
    if before == after {
        return Ok(());
    }
    let (removed, added) = changed_lines(before, after);
    Err(AdoptError::Changed {
        alias: alias.into(),
        removed,
        added,
    })
}

impl ManagedStore {
    /// Moves the selected hosts of `plan` as one all-or-nothing transaction and returns their
    /// aliases as declared, in alphabetical order.
    ///
    /// Under Portico's lock, every file the plan read must still hold the bytes it read, or
    /// nothing is written. The candidates of every file are staged and validated with `ssh -G`
    /// before anything is activated, every replaced file is backed up beside itself, and a
    /// failure while activating or verifying restores every replaced file from its backup.
    /// `cancelled` is honoured only while waiting for the lock.
    pub fn migrate(
        &self,
        plan: &MigrationPlan,
        selection: &[String],
        effective: Effective<'_>,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Vec<String>, AdoptError> {
        let moves = plan.selected(selection)?;
        let changes = plan.changes(&moves)?;
        if moves.is_empty() {
            return Ok(Vec::new());
        }
        let aliases = moves
            .iter()
            .map(|item| item.host.alias().to_owned())
            .collect::<Vec<_>>();
        let paths = self.paths();

        // 1. Lock.
        self.ensure_ready()?;
        let lock = open_lock(&paths.lock).map_err(|source| io_error(&paths.lock, source))?;
        acquire_lock(&lock, cancelled).map_err(|source| io_error(&paths.lock, source))?;

        // 2. Re-check.
        self.recheck(plan, &moves)?;

        // 3. Stage.
        let mut replacements = Vec::with_capacity(changes.len());
        for (path, contents) in &changes {
            let managed = *path == plan.managed;
            let mode = if managed {
                0o600
            } else {
                fs::metadata(path)
                    .map_err(|source| io_error(path, source))?
                    .permissions()
                    .mode()
                    & 0o7777
            };
            let dir = parent_dir(path);
            let candidate = write_unique(&dir, &temporary_stem(path, "candidate"), contents, mode)
                .map_err(|source| io_error(&dir, source))?;
            replacements.push(Replacement {
                path: path.clone(),
                dir,
                mode,
                managed,
                candidate: Some(candidate),
                backup: None,
            });
        }

        // 4. Validate before activation.
        let candidate_of = |path: &Path| {
            replacements
                .iter()
                .find(|replacement| replacement.path == path)
                .and_then(|replacement| replacement.candidate.as_ref())
                .map(PendingFile::path)
        };
        let managed_candidate = candidate_of(&plan.managed).expect("the managed fragment changes");
        let entry = &plan.snapshot[&plan.entry];
        let after_entry = match plan.direction {
            Direction::Adopt => entry,
            Direction::Export => {
                &changes
                    .iter()
                    .find(|(path, _)| *path == plan.entry)
                    .expect("export changes the entry config")
                    .1
            }
        };
        let before_file = self.stage_entry(entry, &paths.managed)?;
        let after_file = self.stage_entry(after_entry, managed_candidate)?;
        let mut before = Vec::with_capacity(aliases.len());
        for alias in &aliases {
            let old = resolve(effective, before_file.path(), alias)?;
            require_same(alias, &old, &resolve(effective, after_file.path(), alias)?)?;
            before.push(old);
        }
        drop((before_file, after_file));

        // 5. Back up.
        let mut dirs = Vec::new();
        for replacement in &mut replacements {
            let backup = write_unique(
                &replacement.dir,
                &temporary_stem(&replacement.path, "backup"),
                &plan.snapshot[&replacement.path],
                replacement.mode,
            )
            .map_err(|source| io_error(&replacement.dir, source))?;
            replacement.backup = Some(backup);
            if !dirs.contains(&replacement.dir) {
                dirs.push(replacement.dir.clone());
            }
        }
        for dir in &dirs {
            sync_dir(dir).map_err(|source| io_error(dir, source))?;
        }

        // 6. Activate, then 7. verify.
        let mut activated = 0;
        let result = (|| {
            for replacement in &mut replacements {
                let candidate = replacement.candidate.take().expect("staged once");
                if replacement.managed {
                    write_unique(
                        &paths.portico_dir,
                        ".config.portico-backup",
                        &plan.snapshot[&plan.managed],
                        0o600,
                    )
                    .and_then(|backup| backup.rename_to(&paths.backup))
                    .map_err(|source| io_error(&paths.backup, source))?;
                    #[cfg(test)]
                    fail_point(FailPoint::ManagedRename)
                        .map_err(|source| io_error(&replacement.path, source))?;
                    candidate
                        .rename_to(&replacement.path)
                        .map_err(|source| io_error(&replacement.path, source))?;
                } else {
                    // ponytail: editors do not share the lock; this narrows their window to
                    // the instant between this read and the rename, as `portico init` accepts.
                    match fs::read(&replacement.path) {
                        Ok(bytes) if bytes == plan.snapshot[&replacement.path] => {}
                        _ => return Err(AdoptError::Stale),
                    }
                    #[cfg(test)]
                    fail_point(FailPoint::FileRename(match plan.direction {
                        // The managed fragment was activated first.
                        Direction::Adopt => activated - 1,
                        Direction::Export => activated,
                    }))
                    .map_err(|source| io_error(&replacement.path, source))?;
                    candidate
                        .rename_to(&replacement.path)
                        .map_err(|source| io_error(&replacement.path, source))?;
                }
                activated += 1;
            }
            for dir in &dirs {
                #[cfg(test)]
                fail_point(FailPoint::DirectorySync).map_err(|source| io_error(dir, source))?;
                sync_dir(dir).map_err(|source| io_error(dir, source))?;
            }

            #[cfg(test)]
            fail_point(FailPoint::Verify).map_err(|source| io_error(&plan.entry, source))?;
            let installed =
                fs::read(&plan.entry).map_err(|source| io_error(&plan.entry, source))?;
            let installed = self.stage_entry(&installed, &paths.managed)?;
            for (alias, old) in aliases.iter().zip(&before) {
                require_same(alias, old, &resolve(effective, installed.path(), alias)?)?;
            }
            Ok(())
        })();

        // 8. Finish: dropping the replacements removes the backups and any unused candidate.
        let Err(original) = result else {
            return Ok(aliases);
        };
        let replaced = &mut replacements[..activated];
        let mut failures = Vec::new();
        for replacement in replaced.iter().rev() {
            // The backup holds these same bytes; restoring from memory needs no extra read.
            let restored = (|| {
                #[cfg(test)]
                fail_point(FailPoint::Restore)?;
                write_unique(
                    &replacement.dir,
                    &temporary_stem(&replacement.path, "restore"),
                    &plan.snapshot[&replacement.path],
                    replacement.mode,
                )?
                .rename_to(&replacement.path)
            })();
            if let Err(source) = restored {
                failures.push(io_error(&replacement.path, source).to_string());
            }
        }
        let mut synced = Vec::new();
        for replacement in replaced.iter() {
            if !synced.contains(&replacement.dir) {
                if let Err(source) = sync_dir(&replacement.dir) {
                    failures.push(io_error(&replacement.dir, source).to_string());
                }
                synced.push(replacement.dir.clone());
            }
        }
        if failures.is_empty() {
            return Err(original);
        }
        let backups = replaced
            .iter_mut()
            .map(|replacement| {
                let backup = replacement
                    .backup
                    .take()
                    .expect("backed up before activation");
                (replacement.path.clone(), backup.keep())
            })
            .collect();
        Err(AdoptError::Rollback {
            original: Box::new(original),
            failures,
            backups,
        })
    }

    /// `Stale` unless every file `plan` read still holds the bytes it read, the entry config
    /// still resolves to the same file and, for adoption, discovery still finds each selected
    /// alias declared once, in the file it moves from.
    fn recheck(&self, plan: &MigrationPlan, moves: &[&Move]) -> Result<(), AdoptError> {
        for (path, bytes) in &plan.snapshot {
            match fs::read(path) {
                Ok(current) if current == *bytes => {}
                _ => return Err(AdoptError::Stale),
            }
        }
        if self.entry_file().ok().map(file_key).as_ref() != Some(&plan.entry) {
            return Err(AdoptError::Stale);
        }
        if plan.direction == Direction::Adopt {
            let paths = self.paths();
            let discovery = discover(&paths.entry, &paths.managed)?;
            if !discovery.aliases_complete {
                return Err(AdoptError::Stale);
            }
            for item in moves {
                let mut declarations = discovery
                    .hosts
                    .iter()
                    .filter(|external| external.alias.eq_ignore_ascii_case(item.host.alias()));
                let unchanged = match (declarations.next(), declarations.next()) {
                    (Some(external), None) => symlink_target(&external.source)
                        .is_ok_and(|target| file_key(target) == item.file),
                    _ => false,
                };
                if !unchanged {
                    return Err(AdoptError::Stale);
                }
            }
        }
        Ok(())
    }
}

impl MigrationPlan {
    /// The candidates that may be selected.
    pub fn eligible(&self) -> impl Iterator<Item = &Candidate> {
        self.candidates
            .iter()
            .filter(|candidate| candidate.reason.is_none())
    }

    /// The exact line changes of moving the selected hosts, grouped by file in activation
    /// order. An empty selection changes nothing.
    pub fn preview(&self, selection: &[String]) -> Result<Preview, AdoptError> {
        let files = self
            .changes(&self.selected(selection)?)?
            .into_iter()
            .map(|(path, new)| {
                let old = String::from_utf8_lossy(&self.snapshot[&path]);
                diff(&path, &old, &String::from_utf8_lossy(&new))
            })
            .collect();
        Ok(Preview { files })
    }

    /// Marks the candidates of the failed lowercase aliases ineligible and drops their moves.
    fn reject(&mut self, failures: BTreeMap<String, String>) {
        for (key, reason) in failures {
            self.moves.remove(&key);
            for candidate in &mut self.candidates {
                if candidate.reason.is_none() && candidate.alias.to_ascii_lowercase() == key {
                    candidate.reason = Some(reason.clone());
                }
            }
        }
    }

    /// The moves of the selected aliases in alphabetical order, or why one cannot be selected.
    fn selected(&self, selection: &[String]) -> Result<Vec<&Move>, AdoptError> {
        let mut selected = BTreeMap::new();
        for alias in selection {
            let key = alias.to_ascii_lowercase();
            if let Some(item) = self.moves.get(&key) {
                selected.insert(key, item);
                continue;
            }
            let candidate = self
                .candidates
                .iter()
                .find(|candidate| candidate.alias.eq_ignore_ascii_case(alias))
                .ok_or_else(|| AdoptError::UnknownHost(alias.clone()))?;
            return Err(AdoptError::Ineligible {
                direction: self.direction,
                alias: candidate.alias.clone(),
                reason: candidate.reason.clone().unwrap_or_default(),
            });
        }
        Ok(selected.into_values().collect())
    }

    /// The new contents of every file `moves` touch, in activation order: the managed fragment
    /// then each source file for adoption, the entry config then the managed fragment for
    /// export. Each file's previous contents are its snapshot.
    fn changes(&self, moves: &[&Move]) -> Result<Vec<(PathBuf, Vec<u8>)>, AdoptError> {
        if moves.is_empty() {
            return Ok(Vec::new());
        }
        let text = String::from_utf8_lossy(&self.snapshot[&self.managed]);
        let mut config = ManagedConfig::parse(&text).map_err(TransactionError::Operation)?;
        match self.direction {
            Direction::Adopt => {
                let mut ranges = BTreeMap::<&Path, Vec<Range<usize>>>::new();
                for item in moves {
                    config
                        .insert_with_comments(item.host.clone(), item.comments.clone())
                        .map_err(TransactionError::Operation)?;
                    ranges
                        .entry(&item.file)
                        .or_default()
                        .push(item.removed.clone());
                }
                let mut changes = vec![(self.managed.clone(), config.render().into_bytes())];
                changes.extend(ranges.into_iter().map(|(file, ranges)| {
                    (
                        file.to_path_buf(),
                        remove_lines(&self.snapshot[file], &ranges),
                    )
                }));
                Ok(changes)
            }
            Direction::Export => {
                for item in moves {
                    config
                        .remove_entry(item.host.alias())
                        .map_err(TransactionError::Operation)?;
                }
                let entry = insert_after_first_line(
                    &self.snapshot[&self.entry],
                    export_blocks(moves.iter().copied()).as_bytes(),
                );
                Ok(vec![
                    (self.entry.clone(), entry),
                    (self.managed.clone(), config.render().into_bytes()),
                ])
            }
        }
    }
}

#[cfg(test)]
impl MigrationPlan {
    /// A plan over made-up files: each eligible candidate is a `Host <alias>` block with a
    /// `HostName <alias>.example` line, appended to its source file for adoption and kept in the
    /// managed fragment for export, below an entry config holding only the Portico Include.
    pub(crate) fn for_tests(direction: Direction, candidates: Vec<Candidate>) -> Self {
        let managed = PathBuf::from("/home/test/.ssh/portico/config");
        let entry = PathBuf::from("/home/test/.ssh/config");
        let mut config = ManagedConfig::empty();
        let mut snapshot = BTreeMap::<PathBuf, Vec<u8>>::new();
        let mut moves = BTreeMap::new();
        for candidate in candidates
            .iter()
            .filter(|candidate| candidate.reason.is_none())
        {
            let alias = &candidate.alias;
            let host = Host::new(alias, format!("{alias}.example"), None, None, None, None)
                .expect("a valid test host");
            let (file, removed) = match direction {
                Direction::Adopt => {
                    let contents = snapshot.entry(candidate.source.clone()).or_default();
                    let start = contents.iter().filter(|&&byte| byte == b'\n').count();
                    contents
                        .extend(format!("Host {alias}\n  HostName {alias}.example\n\n").bytes());
                    (candidate.source.clone(), start..start + 3)
                }
                Direction::Export => {
                    config.insert(host.clone()).expect("distinct test aliases");
                    (entry.clone(), 0..0)
                }
            };
            let item = Move {
                file,
                removed,
                host,
                comments: HostComments::default(),
            };
            moves.insert(alias.to_ascii_lowercase(), item);
        }
        snapshot.insert(managed.clone(), config.render().into_bytes());
        snapshot
            .entry(entry.clone())
            .or_insert_with(|| b"Include ~/.ssh/portico/config\n".to_vec());
        Self {
            direction,
            candidates,
            managed,
            entry,
            snapshot,
            moves,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed::{ConfigPaths, ManagedStore};
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    const HEADER: &str = "# Managed by Portico. Do not edit manually.\n";

    struct Fixture {
        dir: TempDir,
        paths: ConfigPaths,
        store: ManagedStore,
    }

    impl Fixture {
        fn home(&self) -> &Path {
            self.dir.path()
        }

        fn write(&self, relative: &str, contents: &[u8]) -> PathBuf {
            let path = self.paths.ssh_dir.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            path
        }
    }

    /// A ready temporary home whose entry is the Portico Include followed by `entry` and whose
    /// managed fragment is `managed` (the header alone when empty).
    fn fixture_with(entry: &str, managed: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = ConfigPaths::from_home(dir.path());
        paths.system_config = dir.path().join("missing_ssh_config");
        fs::create_dir_all(&paths.portico_dir).unwrap();
        fs::write(
            &paths.entry,
            format!("Include ~/.ssh/portico/config\n{entry}"),
        )
        .unwrap();
        fs::write(&paths.managed, format!("{HEADER}{managed}")).unwrap();
        let store = ManagedStore::new(paths.clone());
        Fixture { dir, paths, store }
    }

    fn fixture(entry: &str) -> Fixture {
        fixture_with(entry, "")
    }

    fn same() -> Effective<'static> {
        &|_, _| Ok(String::new())
    }

    fn reasons(plan: &MigrationPlan) -> Vec<(String, Option<String>)> {
        let mut reasons = plan
            .candidates
            .iter()
            .map(|candidate| (candidate.alias.clone(), candidate.reason.clone()))
            .collect::<Vec<_>>();
        reasons.sort();
        reasons
    }

    fn assert_no_staged_files(paths: &ConfigPaths) {
        for dir in [&paths.ssh_dir, &paths.portico_dir] {
            for entry in fs::read_dir(dir).unwrap() {
                let name = entry.unwrap().file_name();
                assert!(
                    !name.to_string_lossy().starts_with(".config.portico-"),
                    "left behind {name:?} in {}",
                    dir.display()
                );
            }
        }
    }

    #[test]
    fn eligible_block_in_entry_is_listed_with_its_source() {
        let fixture = fixture("Host dev\n  HostName d\n");
        let plan = fixture.store.adoption_plan(same()).unwrap();
        assert_eq!(plan.direction, Direction::Adopt);
        assert_eq!(
            plan.candidates,
            vec![Candidate {
                alias: "dev".into(),
                source: fixture.paths.entry.clone(),
                reason: None,
            }]
        );
        assert_eq!(plan.eligible().count(), 1);
        assert_no_staged_files(&fixture.paths);
    }

    #[test]
    fn reasons_follow_the_spec_order() {
        let fixture = fixture_with(
            "Include extra.conf readonly.conf latin.conf locked/hosts.conf\n\
             Host ok\n  HostName o\n\
             Host multi other\n  HostName m\n\
             Host fwd\n  ForwardAgent yes\n\
             Host dup\n  HostName a\n\
             Host twice\n  HostName x\n\
             Host twice\n  HostName y\n\
             Host taken\n  HostName t\n\
             Match host x\n  Include cond.conf\n",
            "\nHost taken\n    HostName t\n",
        );
        fixture.write("extra.conf", b"Host dup\n  ForwardAgent yes\n");
        let readonly = fixture.write(
            "readonly.conf",
            b"Host ro\n  HostName r\nHost rofwd\n  ForwardAgent yes\n",
        );
        fs::set_permissions(&readonly, fs::Permissions::from_mode(0o444)).unwrap();
        fixture.write("latin.conf", b"Host latin\n  HostName l\n# caf\xe9\n");
        let locked = fixture.write("locked/hosts.conf", b"Host locked\n  HostName k\n");
        let locked_dir = locked.parent().unwrap().to_path_buf();
        fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o555)).unwrap();
        fixture.write("cond.conf", b"Host cond\n  HostName c\n");

        let plan = fixture.store.adoption_plan(same());
        fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let plan = plan.unwrap();

        let reason = |text: &str| Some(text.to_owned());
        let mut expected = vec![
            (
                "cond".into(),
                reason("included conditionally (inside a Host or Match block)"),
            ),
            ("dup".into(), reason("declared in 2 files")),
            ("dup".into(), reason("declared in 2 files")),
            (
                "fwd".into(),
                reason("ForwardAgent is not supported by Portico"),
            ),
            ("latin".into(), reason("not valid UTF-8")),
            ("locked".into(), reason("directory is read-only")),
            ("multi".into(), reason("Host line declares 2 aliases")),
            ("ok".into(), None),
            ("other".into(), reason("Host line declares 2 aliases")),
            ("ro".into(), reason("file is read-only")),
            (
                "rofwd".into(),
                reason("ForwardAgent is not supported by Portico"),
            ),
            ("taken".into(), reason("already managed")),
            ("twice".into(), reason("declared 2 times in this file")),
            ("twice".into(), reason("declared 2 times in this file")),
        ];
        expected.sort();
        assert_eq!(reasons(&plan), expected);
        assert_eq!(
            plan.eligible()
                .map(|c| c.alias.as_str())
                .collect::<Vec<_>>(),
            ["ok"]
        );
    }

    #[test]
    fn incomplete_inventory_makes_every_host_ineligible() {
        let fixture = fixture("Include ~nobody/hosts\nHost dev\n  HostName d\n");
        let plan = fixture.store.adoption_plan(same()).unwrap();
        assert_eq!(
            reasons(&plan),
            [("dev".into(), Some("host inventory is incomplete".into()))]
        );
    }

    #[test]
    fn a_file_owned_by_another_user_is_refused() {
        let fixture = fixture("");
        let path = fixture.write("hosts.conf", b"Host a\n  HostName a\n");
        let uid = std::os::unix::fs::MetadataExt::uid(&fs::metadata(&path).unwrap());
        assert_eq!(source_problem(&path, b"Host a\n", uid), None);
        assert_eq!(
            source_problem(&path, b"Host a\n", uid.wrapping_add(1)).as_deref(),
            Some("owned by another user")
        );
        assert_eq!(
            source_problem(&fixture.paths.ssh_dir, b"", uid).as_deref(),
            Some("not a regular file")
        );
    }

    #[test]
    fn lossless_check_rejects_a_changed_effective_configuration() {
        let fixture = fixture("Host dev\n  HostName d\n");
        let effective: Effective = &|entry, _| {
            Ok(
                if fs::read_to_string(entry)
                    .unwrap()
                    .contains("adopt-candidate")
                {
                    "user alice\n".into()
                } else {
                    "user root\n".into()
                },
            )
        };
        let plan = fixture.store.adoption_plan(effective).unwrap();
        assert_eq!(
            reasons(&plan),
            [(
                "dev".into(),
                Some("effective configuration would change (-user root, +user alice)".into())
            )]
        );
        assert_no_staged_files(&fixture.paths);

        let failing: Effective = &|_, _| Err("ssh: bad option\nsecond line\n".into());
        let plan = fixture.store.adoption_plan(failing).unwrap();
        assert_eq!(
            reasons(&plan),
            [(
                "dev".into(),
                Some("OpenSSH could not resolve it: ssh: bad option".into())
            )]
        );
        assert_no_staged_files(&fixture.paths);
    }

    #[test]
    fn preview_of_adoption_lists_added_and_removed_lines() {
        let fixture =
            fixture("\n# box\nHost dev\n  HostName d\n  User bob\n\nHost after\n  HostName a\n");
        let plan = fixture.store.adoption_plan(same()).unwrap();
        let preview = plan.preview(&["dev".into()]).unwrap();
        assert_eq!(
            preview.render(Some(fixture.home())),
            "~/.ssh/portico/config\n  +2: \n  +3: # box\n  +4: Host dev\n  +5:     HostName d\n  +6:     User bob\n\n\
             ~/.ssh/config\n  -3: # box\n  -4: Host dev\n  -5:   HostName d\n  -6:   User bob\n  -7: \n"
        );
        assert_eq!(
            fs::read_to_string(&fixture.paths.managed).unwrap(),
            HEADER,
            "planning changes nothing"
        );
    }

    #[test]
    fn preview_of_adoption_removes_blocks_of_several_files() {
        let fixture = fixture("Include extra.conf\nHost a\n  HostName a\n");
        fixture.write(
            "extra.conf",
            b"Host b\n  HostName b\n\nHost c\n  HostName c\n",
        );
        let plan = fixture.store.adoption_plan(same()).unwrap();
        let preview = plan.preview(&["C".into(), "a".into(), "b".into()]).unwrap();
        assert_eq!(
            preview.render(Some(fixture.home())),
            "~/.ssh/portico/config\n  +2: \n  +3: Host a\n  +4:     HostName a\n  +5: \n  +6: Host b\n  +7:     HostName b\n  +8: \n  +9: Host c\n  +10:     HostName c\n\n\
             ~/.ssh/config\n  -3: Host a\n  -4:   HostName a\n\n\
             ~/.ssh/extra.conf\n  -1: Host b\n  -2:   HostName b\n  -3: \n  -4: Host c\n  -5:   HostName c\n"
        );
    }

    #[test]
    fn symlinked_source_is_previewed_at_its_target() {
        let fixture = fixture("Include linked.conf\n");
        let target = fixture.write("dotfiles/hosts.conf", b"Host dev\n  HostName d\n");
        let link = fixture.paths.ssh_dir.join("linked.conf");
        symlink("dotfiles/hosts.conf", &link).unwrap();
        let plan = fixture.store.adoption_plan(same()).unwrap();
        assert_eq!(plan.candidates[0].source, link);
        let preview = plan.preview(&["dev".into()]).unwrap();
        assert_eq!(preview.files[1].path, target);
    }

    #[test]
    fn preview_of_export_inserts_below_the_include() {
        let fixture = fixture_with(
            "Host other\n  HostName o\n",
            "\n# box\nHost dev\n    HostName d\n    User bob\n",
        );
        let plan = fixture.store.export_plan(same()).unwrap();
        assert_eq!(plan.direction, Direction::Export);
        assert_eq!(
            plan.candidates,
            vec![Candidate {
                alias: "dev".into(),
                source: fixture.paths.managed.clone(),
                reason: None,
            }]
        );
        let preview = plan.preview(&["dev".into()]).unwrap();
        assert_eq!(
            preview.render(Some(fixture.home())),
            "~/.ssh/config\n  +2: # box\n  +3: Host dev\n  +4:     HostName d\n  +5:     User bob\n  +6: \n\n\
             ~/.ssh/portico/config\n  -2: \n  -3: # box\n  -4: Host dev\n  -5:     HostName d\n  -6:     User bob\n"
        );
        assert_no_staged_files(&fixture.paths);
    }

    #[test]
    fn export_lossless_check_stages_the_inserted_blocks() {
        let fixture = fixture_with("", "\nHost dev\n    HostName d\n");
        let effective: Effective = &|entry, _| {
            let text = fs::read_to_string(entry).unwrap();
            Ok(
                if text.contains("adopt-candidate") && text.contains("Host dev") {
                    "same\n".into()
                } else if text.contains("adopt-candidate") {
                    "lost\n".into()
                } else {
                    "same\n".into()
                },
            )
        };
        let plan = fixture.store.export_plan(effective).unwrap();
        assert_eq!(reasons(&plan), [("dev".into(), None)]);
    }

    #[test]
    fn preview_rejects_unknown_and_ineligible_aliases() {
        let fixture = fixture("Host dev\n  HostName d\nHost fwd\n  ForwardAgent yes\n");
        let plan = fixture.store.adoption_plan(same()).unwrap();
        assert!(matches!(
            plan.preview(&["nope".into()]),
            Err(AdoptError::UnknownHost(alias)) if alias == "nope"
        ));
        assert_eq!(
            plan.preview(&["dev".into(), "fwd".into()])
                .unwrap_err()
                .to_string(),
            "cannot adopt fwd: ForwardAgent is not supported by Portico"
        );
        assert!(plan.preview(&[]).unwrap().files.is_empty());

        let fixture = fixture_with("", "\nHost dev\n    HostName d\n");
        fs::set_permissions(&fixture.paths.entry, fs::Permissions::from_mode(0o444)).unwrap();
        let plan = fixture.store.export_plan(same()).unwrap();
        assert_eq!(
            plan.preview(&["dev".into()]).unwrap_err().to_string(),
            "cannot export dev: file is read-only"
        );
    }

    #[test]
    fn plans_require_a_ready_setup() {
        let fixture = fixture("");
        fs::write(&fixture.paths.entry, "Host dev\n  HostName d\n").unwrap();
        assert!(matches!(
            fixture.store.adoption_plan(same()),
            Err(AdoptError::Transaction(TransactionError::NotInitialized))
        ));
        assert!(matches!(
            fixture.store.export_plan(same()),
            Err(AdoptError::Transaction(TransactionError::NotInitialized))
        ));
    }
    #[test]
    fn snapshot_keys_name_each_file_once() {
        let fixture = fixture("Include linked.conf\n");
        fs::create_dir_all(fixture.paths.ssh_dir.join("sub")).unwrap();
        let target = fixture.write("hosts.conf", b"Host dev\n  HostName d\n");
        symlink(
            "sub/../hosts.conf",
            fixture.paths.ssh_dir.join("linked.conf"),
        )
        .unwrap();
        let plan = fixture.store.adoption_plan(same()).unwrap();
        let preview = plan.preview(&["dev".into()]).unwrap();
        assert_eq!(preview.files[1].path, target);
        assert!(plan.snapshot.contains_key(&target));
        assert_eq!(plan.snapshot.len(), 3, "{:?}", plan.snapshot.keys());

        let fixture = fixture_with("", "\nHost dev\n    HostName d\n");
        let real = fixture.paths.ssh_dir.join("config.real");
        fs::rename(&fixture.paths.entry, &real).unwrap();
        symlink("sub/../config.real", &fixture.paths.entry).unwrap();
        fs::create_dir_all(fixture.paths.ssh_dir.join("sub")).unwrap();
        let plan = fixture.store.export_plan(same()).unwrap();
        assert_eq!(plan.entry, real);
        assert_eq!(plan.preview(&["dev".into()]).unwrap().files[0].path, real);
    }

    #[test]
    fn rollback_message_reads_well_with_any_number_of_backups() {
        let error = |backups: Vec<(PathBuf, PathBuf)>| {
            AdoptError::Rollback {
                original: Box::new(AdoptError::Stale),
                failures: vec!["disk full".into()],
                backups,
            }
            .to_string()
        };
        let original = AdoptError::Stale.to_string();
        assert_eq!(
            error(Vec::new()),
            format!(
                "{original}; rollback also failed, so the state is uncertain; rollback: disk full"
            )
        );
        assert_eq!(
            error(vec![
                ("/a".into(), "/.a.bak".into()),
                ("/b".into(), "/.b.bak".into()),
            ]),
            format!(
                "{original}; rollback also failed, so the state is uncertain; restore /a from /.a.bak; restore /b from /.b.bak; rollback: disk full"
            )
        );
    }
    /// Every file under the temporary home with its bytes, apart from Portico's lock and the
    /// managed fragment's `config.bak`; comparing two of these also proves no temporary file
    /// was left behind.
    fn tree(paths: &ConfigPaths) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let kind = fs::symlink_metadata(&path).unwrap().file_type();
                if kind.is_dir() {
                    walk(&path, files);
                } else if kind.is_symlink() {
                    let target = fs::read_link(&path).unwrap();
                    files.insert(path, target.into_os_string().into_encoded_bytes());
                } else {
                    files.insert(path.clone(), fs::read(&path).unwrap());
                }
            }
        }
        let mut files = BTreeMap::new();
        walk(&paths.ssh_dir, &mut files);
        files.remove(&paths.lock);
        files.remove(&paths.backup);
        files
    }

    /// Asserts that no temporary file of a transaction remains anywhere under `.ssh`.
    fn assert_no_temporary_files(paths: &ConfigPaths) {
        for path in tree(paths).keys() {
            let name = path.file_name().unwrap().to_string_lossy();
            assert!(
                !name.starts_with(".config.portico-") && !name.contains(".portico-adopt-"),
                "left behind {}",
                path.display()
            );
        }
    }

    fn arm(points: &[FailPoint]) {
        FAIL_AT.with(|armed| *armed.borrow_mut() = points.to_vec());
    }

    fn migrate(
        fixture: &Fixture,
        plan: &MigrationPlan,
        aliases: &[&str],
    ) -> Result<Vec<String>, AdoptError> {
        let selection = aliases
            .iter()
            .map(|alias| alias.to_string())
            .collect::<Vec<_>>();
        fixture.store.migrate(plan, &selection, same(), None)
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    #[test]
    fn adopting_moves_exactly_the_shown_lines() {
        let fixture = fixture(
            "\nHost keep\n  HostName k\n\n# box\nHost dev\n  HostName d\n  User bob\n\nHost after\n  HostName a\n",
        );
        let plan = fixture.store.adoption_plan(same()).unwrap();
        assert_eq!(migrate(&fixture, &plan, &["DEV"]).unwrap(), ["dev"]);
        assert_eq!(
            read(&fixture.paths.entry),
            "Include ~/.ssh/portico/config\n\nHost keep\n  HostName k\n\nHost after\n  HostName a\n"
        );
        assert_eq!(
            read(&fixture.paths.managed),
            format!("{HEADER}\n# box\nHost dev\n    HostName d\n    User bob\n")
        );
        assert_eq!(
            read(&fixture.paths.backup),
            HEADER,
            "config.bak holds the previous fragment"
        );
        assert_no_temporary_files(&fixture.paths);
    }

    #[test]
    fn adopting_from_an_included_file_and_the_entry_in_one_go() {
        let fixture = fixture("Include extra.conf\nHost a\n  HostName a\n");
        let extra = fixture.write(
            "extra.conf",
            b"Host b\n  HostName b\n\nHost c\n  HostName c\n# tail\n",
        );
        fs::set_permissions(&extra, fs::Permissions::from_mode(0o640)).unwrap();
        let plan = fixture.store.adoption_plan(same()).unwrap();
        assert_eq!(migrate(&fixture, &plan, &["b", "a"]).unwrap(), ["a", "b"]);
        assert_eq!(
            read(&fixture.paths.entry),
            "Include ~/.ssh/portico/config\nInclude extra.conf\n"
        );
        assert_eq!(read(&extra), "Host c\n  HostName c\n# tail\n");
        assert_eq!(
            fs::metadata(&extra).unwrap().permissions().mode() & 0o7777,
            0o640
        );
        assert_eq!(
            read(&fixture.paths.managed),
            format!("{HEADER}\nHost a\n    HostName a\n\nHost b\n    HostName b\n")
        );
        assert_no_temporary_files(&fixture.paths);
    }

    #[test]
    fn two_blocks_in_one_file_are_removed_together() {
        let fixture = fixture("Include extra.conf\n");
        let extra = fixture.write(
            "extra.conf",
            b"Host b\n  HostName b\n\nHost c\r\n  HostName c\r\n\r\nHost d\n  HostName d\n",
        );
        let plan = fixture.store.adoption_plan(same()).unwrap();
        let preview = plan.preview(&["b".into(), "d".into()]).unwrap();
        assert_eq!(
            preview.files.len(),
            2,
            "one change for the managed fragment, one for the file"
        );
        assert_eq!(migrate(&fixture, &plan, &["d", "b"]).unwrap(), ["b", "d"]);
        assert_eq!(read(&extra), "Host c\r\n  HostName c\r\n\r\n");
        assert_no_temporary_files(&fixture.paths);
    }

    #[test]
    fn stale_snapshot_is_refused_and_nothing_changes() {
        let fixture = fixture("Host dev\n  HostName d\n");
        let plan = fixture.store.adoption_plan(same()).unwrap();
        let mut entry = fs::OpenOptions::new()
            .append(true)
            .open(&fixture.paths.entry)
            .unwrap();
        std::io::Write::write_all(&mut entry, b"# edited\n").unwrap();
        let before = tree(&fixture.paths);
        let error = migrate(&fixture, &plan, &["dev"]).unwrap_err();
        assert!(matches!(error, AdoptError::Stale), "{error}");
        assert_eq!(
            error.to_string(),
            "the configuration changed since the preview; run the command again to see a fresh preview"
        );
        assert_eq!(tree(&fixture.paths), before);
        assert!(!fixture.paths.backup.exists());
    }

    #[test]
    fn a_retargeted_entry_link_is_stale() {
        let fixture = fixture("Host dev\n  HostName d\n");
        let plan = fixture.store.adoption_plan(same()).unwrap();
        let moved = fixture.paths.ssh_dir.join("moved");
        fs::copy(&fixture.paths.entry, &moved).unwrap();
        fs::remove_file(&fixture.paths.entry).unwrap();
        symlink("moved", &fixture.paths.entry).unwrap();
        let before = tree(&fixture.paths);
        assert!(matches!(
            migrate(&fixture, &plan, &["dev"]),
            Err(AdoptError::Stale)
        ));
        assert_eq!(tree(&fixture.paths), before);
    }

    #[test]
    fn a_new_declaration_found_by_discovery_is_stale() {
        let fixture = fixture("Include conf.d/*\n");
        fixture.write("conf.d/a.conf", b"Host dev\n  HostName d\n");
        let plan = fixture.store.adoption_plan(same()).unwrap();
        fixture.write("conf.d/b.conf", b"Host dev\n  HostName other\n");
        let before = tree(&fixture.paths);
        assert!(matches!(
            migrate(&fixture, &plan, &["dev"]),
            Err(AdoptError::Stale)
        ));
        assert_eq!(tree(&fixture.paths), before);
    }

    #[test]
    fn unknown_ineligible_and_empty_selections_take_no_lock() {
        let fixture = fixture("Host dev\n  HostName d\nHost fwd\n  ForwardAgent yes\n");
        let plan = fixture.store.adoption_plan(same()).unwrap();
        let before = tree(&fixture.paths);
        assert!(matches!(
            migrate(&fixture, &plan, &["nope"]),
            Err(AdoptError::UnknownHost(alias)) if alias == "nope"
        ));
        assert_eq!(
            migrate(&fixture, &plan, &["dev", "fwd"])
                .unwrap_err()
                .to_string(),
            "cannot adopt fwd: ForwardAgent is not supported by Portico"
        );
        assert!(migrate(&fixture, &plan, &[]).unwrap().is_empty());
        assert!(!fixture.paths.lock.exists());
        assert_eq!(tree(&fixture.paths), before);
    }

    #[test]
    fn validation_difference_at_apply_time_changes_nothing() {
        let fixture = fixture("Host dev\n  HostName d\n");
        let plan = fixture.store.adoption_plan(same()).unwrap();
        let before = tree(&fixture.paths);
        let effective: Effective = &|entry, _| {
            Ok(if read(entry).contains("adopt-candidate") {
                "user alice\n".into()
            } else {
                "user root\n".into()
            })
        };
        let error = fixture
            .store
            .migrate(&plan, &["dev".into()], effective, None)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "effective configuration of dev would change: -user root, +user alice"
        );
        let failing: Effective = &|_, _| Err("ssh: bad option\n".into());
        let error = fixture
            .store
            .migrate(&plan, &["dev".into()], failing, None)
            .unwrap_err();
        assert!(
            matches!(error, AdoptError::OpenSsh { ref alias, .. } if alias == "dev"),
            "{error}"
        );
        assert_eq!(tree(&fixture.paths), before);
        assert!(!fixture.paths.backup.exists());
    }

    /// A ready home with hosts to adopt from the entry and from an included file.
    fn two_source_fixture() -> Fixture {
        let fixture = fixture(
            "Include extra.conf\n\n# entry host\nHost a\n  HostName a\r\n\nHost z\n  HostName z\n",
        );
        fixture.write(
            "extra.conf",
            b"Host b\n  HostName b\n  User bob # old\n\nHost y\n  HostName y\n",
        );
        fixture
    }

    #[test]
    fn each_activation_failure_rolls_back_byte_for_byte() {
        for point in [
            FailPoint::ManagedRename,
            FailPoint::FileRename(0),
            FailPoint::FileRename(1),
            FailPoint::DirectorySync,
            FailPoint::Verify,
        ] {
            let fixture = two_source_fixture();
            let plan = fixture.store.adoption_plan(same()).unwrap();
            let before = tree(&fixture.paths);
            arm(&[point]);
            let error = migrate(&fixture, &plan, &["a", "b"]).unwrap_err();
            arm(&[]);
            assert!(matches!(error, AdoptError::Io { .. }), "{point:?}: {error}");
            assert!(error.to_string().contains("injected"), "{point:?}: {error}");
            assert_eq!(tree(&fixture.paths), before, "{point:?}");
        }
    }

    #[test]
    fn each_export_activation_failure_rolls_back_byte_for_byte() {
        for point in [
            FailPoint::FileRename(0),
            FailPoint::ManagedRename,
            FailPoint::DirectorySync,
            FailPoint::Verify,
        ] {
            let fixture = fixture_with(
                "Host other\n  HostName o\n",
                "\n# box\nHost dev\n    HostName d\n",
            );
            let plan = fixture.store.export_plan(same()).unwrap();
            let before = tree(&fixture.paths);
            arm(&[point]);
            let error = migrate(&fixture, &plan, &["dev"]).unwrap_err();
            arm(&[]);
            assert!(error.to_string().contains("injected"), "{point:?}: {error}");
            assert_eq!(tree(&fixture.paths), before, "{point:?}");
        }
    }

    #[test]
    fn a_verification_difference_rolls_back() {
        let fixture = two_source_fixture();
        let plan = fixture.store.adoption_plan(same()).unwrap();
        let before = tree(&fixture.paths);
        let calls = std::cell::Cell::new(0);
        // Validation runs two probes per alias; the installed configuration then differs.
        let effective: Effective = &|_, alias| {
            calls.set(calls.get() + 1);
            Ok(if calls.get() > 4 {
                format!("{alias} changed\n")
            } else {
                format!("{alias}\n")
            })
        };
        let error = fixture
            .store
            .migrate(&plan, &["a".into(), "b".into()], effective, None)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "effective configuration of a would change: -a, +a changed"
        );
        assert_eq!(tree(&fixture.paths), before);
    }

    #[test]
    fn an_editor_change_during_the_transaction_is_caught_before_its_rename() {
        let fixture = two_source_fixture();
        let extra = fixture.paths.ssh_dir.join("extra.conf");
        let plan = fixture.store.adoption_plan(same()).unwrap();
        let edited = b"Host b\n  HostName b\n  User bob # old\n\nHost y\n  HostName y\n# editor\n";
        let effective: Effective = &|_, _| {
            fs::write(&extra, edited).unwrap();
            Ok(String::new())
        };
        let mut expected = tree(&fixture.paths);
        expected.insert(extra.clone(), edited.to_vec());
        let error = fixture
            .store
            .migrate(&plan, &["a".into(), "b".into()], effective, None)
            .unwrap_err();
        assert!(matches!(error, AdoptError::Stale), "{error}");
        assert_eq!(
            tree(&fixture.paths),
            expected,
            "the editor's change is kept, the rest restored"
        );
    }

    #[test]
    fn failed_restore_keeps_backups_and_names_them() {
        let fixture = two_source_fixture();
        let plan = fixture.store.adoption_plan(same()).unwrap();
        let before = tree(&fixture.paths);
        arm(&[FailPoint::FileRename(1), FailPoint::Restore]);
        let error = migrate(&fixture, &plan, &["a", "b"]).unwrap_err();
        arm(&[]);
        let AdoptError::Rollback {
            original,
            failures,
            backups,
        } = &error
        else {
            panic!("expected a rollback error, got {error}");
        };
        assert!(original.to_string().contains("injected"), "{original}");
        assert_eq!(failures.len(), 1, "{failures:?}");
        let files = backups
            .iter()
            .map(|(file, _)| file.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            files,
            [fixture.paths.managed.clone(), fixture.paths.entry.clone()],
            "every replaced file, in activation order"
        );
        let message = error.to_string();
        for (file, backup) in backups {
            assert!(
                message.contains(&format!(
                    "restore {} from {}",
                    file.display(),
                    backup.display()
                )),
                "{message}"
            );
            assert_eq!(
                fs::read(backup).unwrap(),
                before[file],
                "{}",
                backup.display()
            );
            let name = backup.file_name().unwrap().to_string_lossy().into_owned();
            let stem = format!(
                ".{}.portico-adopt-backup-",
                file.file_name().unwrap().to_string_lossy()
            );
            assert!(name.starts_with(&stem), "{name}");
            assert_eq!(backup.parent(), file.parent());
        }
        // The entry restore was the one that failed; the managed fragment was restored.
        assert_eq!(
            fs::read(&fixture.paths.managed).unwrap(),
            before[&fixture.paths.managed]
        );
        let mut after = tree(&fixture.paths);
        for (_, backup) in backups {
            after.remove(backup);
        }
        after.remove(&fixture.paths.entry);
        let mut expected = before.clone();
        expected.remove(&fixture.paths.entry);
        assert_eq!(
            after, expected,
            "only the kept backups remain besides the files"
        );
    }

    #[test]
    fn symlinked_source_keeps_the_link() {
        let fixture = fixture("");
        let target = fixture.write(
            "dotfiles/config",
            b"Include ~/.ssh/portico/config\nHost dev\n  HostName d\n",
        );
        fs::remove_file(&fixture.paths.entry).unwrap();
        symlink("dotfiles/config", &fixture.paths.entry).unwrap();
        let plan = fixture.store.adoption_plan(same()).unwrap();
        assert_eq!(migrate(&fixture, &plan, &["dev"]).unwrap(), ["dev"]);
        assert!(
            fs::symlink_metadata(&fixture.paths.entry)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(read(&target), "Include ~/.ssh/portico/config\n");
        assert_no_temporary_files(&fixture.paths);
    }

    #[test]
    fn export_inserts_below_include_and_empties_managed() {
        let fixture = fixture_with(
            "Host other\n  HostName o\n",
            "\n# box\nHost dev\n    HostName d\n    User bob\n\nHost abc\n    HostName c\n",
        );
        let plan = fixture.store.export_plan(same()).unwrap();
        assert_eq!(
            migrate(&fixture, &plan, &["dev", "abc"]).unwrap(),
            ["abc", "dev"]
        );
        assert_eq!(
            read(&fixture.paths.entry),
            "Include ~/.ssh/portico/config\nHost abc\n    HostName c\n\n# box\nHost dev\n    HostName d\n    User bob\n\nHost other\n  HostName o\n"
        );
        assert_eq!(read(&fixture.paths.managed), HEADER);
        assert_no_temporary_files(&fixture.paths);
    }

    #[test]
    fn adopt_then_export_round_trips_comments() {
        let fixture = fixture(
            "\n# box\n# second\nHost dev\n  # the name\n  HostName d\n  User bob  # old account\n\nHost other\n  HostName o\n",
        );
        let plan = fixture.store.adoption_plan(same()).unwrap();
        migrate(&fixture, &plan, &["dev"]).unwrap();
        let plan = fixture.store.export_plan(same()).unwrap();
        migrate(&fixture, &plan, &["dev"]).unwrap();
        assert_eq!(
            read(&fixture.paths.entry),
            "Include ~/.ssh/portico/config\n# box\n# second\nHost dev\n    # the name\n    HostName d\n    # old account\n    User bob\n\n\nHost other\n  HostName o\n"
        );
        assert_eq!(read(&fixture.paths.managed), HEADER);
        assert_no_temporary_files(&fixture.paths);
    }

    #[test]
    fn export_through_a_symlinked_entry_keeps_the_link() {
        let fixture = fixture_with("", "\nHost dev\n    HostName d\n");
        let target = fixture.paths.ssh_dir.join("dotfiles/config");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::rename(&fixture.paths.entry, &target).unwrap();
        symlink("dotfiles/config", &fixture.paths.entry).unwrap();
        let plan = fixture.store.export_plan(same()).unwrap();
        assert_eq!(migrate(&fixture, &plan, &["dev"]).unwrap(), ["dev"]);
        assert!(
            fs::symlink_metadata(&fixture.paths.entry)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            read(&target),
            "Include ~/.ssh/portico/config\nHost dev\n    HostName d\n\n"
        );
        assert_no_temporary_files(&fixture.paths);
    }

    #[test]
    fn concurrent_portico_mutation_waits_for_the_lock() {
        use fs2::FileExt;
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let fixture = fixture("Host dev\n  HostName d\n");
        let plan = fixture.store.adoption_plan(same()).unwrap();
        let lock = fs::File::create(&fixture.paths.lock).unwrap();
        lock.lock_exclusive().unwrap();

        let before = tree(&fixture.paths);
        let cancelled = AtomicBool::new(true);
        let error = fixture
            .store
            .migrate(&plan, &["dev".into()], same(), Some(&cancelled))
            .unwrap_err();
        assert!(error.to_string().contains("operation cancelled"), "{error}");
        assert_eq!(tree(&fixture.paths), before);

        let (locked, released) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            // Recorded before unlocking: migrate may finish before this thread runs again.
            locked.send(Instant::now()).unwrap();
            FileExt::unlock(&lock).unwrap();
        });
        let not_cancelled = AtomicBool::new(false);
        let result = fixture
            .store
            .migrate(&plan, &["dev".into()], same(), Some(&not_cancelled));
        let finished = Instant::now();
        holder.join().unwrap();
        assert_eq!(result.unwrap(), ["dev"]);
        assert!(
            finished >= released.recv().unwrap(),
            "migrate waited for the lock"
        );
        assert_eq!(
            read(&fixture.paths.entry),
            "Include ~/.ssh/portico/config\n"
        );
    }
}
