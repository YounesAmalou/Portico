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
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::discovery::{DiscoveryError, DiscoveryWarning, discover};
use crate::host::Host;
use crate::managed::{
    HostComments, ManagedConfig, ManagedStore, TransactionError, render_host, symlink_target,
    write_unique,
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
    /// The entry config, symlinks resolved.
    entry: PathBuf,
    /// The exact bytes of every file read, keyed by symlink-resolved path: the managed fragment,
    /// the entry config and every source of a candidate.
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
        "{original}; rollback also failed, so the state is uncertain; {recovery}; rollback: {failures}",
        recovery = .backups.iter().map(|(file, backup)| format!("restore {} from {}", file.display(), backup.display())).collect::<Vec<_>>().join("; "),
        failures = .failures.join("; ")
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
            Ok(target) => target,
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
        let entry = self.entry_file()?;
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
        let entry = self.entry_file()?;
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
mod tests {
    use super::*;
    use crate::managed::{ConfigPaths, ManagedStore};
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
}
