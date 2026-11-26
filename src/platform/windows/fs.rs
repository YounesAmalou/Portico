//! Windows files: permissions, links and file identity.
//!
//! Windows' OpenSSH checks access control lists, not mode bits. Files Portico creates inherit
//! the access rules of their folder (`~\.ssh`), which it already accepts, so the Unix modes
//! Portico asks for are kept only as far as the read-only attribute goes.

use std::{
    ffi::OsStr,
    fs::{self, Metadata, OpenOptions},
    io::{self, Write},
    os::windows::{
        ffi::OsStrExt,
        fs::{OpenOptionsExt, symlink_dir, symlink_file},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use windows_sys::Win32::{
    Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_WRITE_PROTECT, HANDLE},
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_TEMPORARY,
        FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_DELETE_ON_CLOSE,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileBasicInfo,
        GetFileInformationByHandle, GetFileInformationByHandleEx, MOVEFILE_REPLACE_EXISTING,
        MOVEFILE_WRITE_THROUGH, MoveFileExW,
    },
};

use super::super::common::fs::{PendingFile, Stamp};
#[cfg(test)]
use super::super::common::fs::{take_directory_sync_failure, take_rollback_write_failure};
use super::process::Uid;

/// How long a replacement waits for a file another program holds open to be let go.
const REPLACE_PATIENCE: Duration = Duration::from_secs(1);

/// Follows the symbolic link chain at `path` to the file a write must replace, which may not
/// exist yet. An error names the path it occurred at.
pub fn symlink_target(start: &Path) -> Result<PathBuf, (PathBuf, io::Error)> {
    let mut path = start.to_path_buf();
    for _ in 0..40 {
        // Unlike Unix, reading a path that is not a link fails with an error of its own kind,
        // so the path is asked first.
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => match fs::read_link(&path) {
                Ok(target) => path = path.parent().unwrap_or(Path::new("")).join(target),
                Err(source) => return Err((path, source)),
            },
            Ok(_) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(path),
            Err(source) => return Err((path, source)),
        }
    }
    Err((
        path,
        io::Error::other(format!(
            "too many levels of symbolic links from {}",
            start.display()
        )),
    ))
}

/// Leaves `options` as they are: a new file inherits the access rules of its folder, which
/// keep `~\.ssh` private already. `mode` is accepted for the same call as on Unix.
pub fn private(options: &mut OpenOptions, _mode: u32) -> &mut OpenOptions {
    options
}

/// The permission bits Unix would show for a private file (`0600`) or directory (`0700`),
/// without the write bits when the read-only attribute is set. Permission repairs that compare
/// against those modes therefore leave Windows files alone.
pub fn mode_of(metadata: &Metadata) -> u32 {
    let mode = if metadata.is_dir() { 0o700 } else { 0o600 };
    if metadata.permissions().readonly() {
        mode & !0o222
    } else {
        mode
    }
}

/// Sets or clears the read-only attribute of the file at `path`, as the owner's write bit of
/// `mode` says. The other bits have no meaning on Windows.
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(mode & 0o200 == 0);
    fs::set_permissions(path, permissions)
}

/// Whether the file at `path` can be run, as its extension says: `.exe`, `.cmd` or `.bat`.
pub fn is_executable(path: &Path, _metadata: &Metadata) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            ["exe", "cmd", "bat"]
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        })
}

/// Whether `path` is where an ssh agent can listen: a named pipe (`\\.\pipe\…`), as the
/// OpenSSH Authentication Agent's. The pipe is not opened, since that would take one of the
/// agent's connections, and its owner is not checked, since its access rules admit only the
/// user: an agent that is not running shows when `ssh-add` asks it. Any other file is no
/// agent's.
pub fn is_agent_endpoint(path: &Path) -> io::Result<bool> {
    let pipe = path
        .to_str()
        .and_then(|path| path.get(..9))
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(r"\\.\pipe\"));
    if pipe {
        return Ok(true);
    }
    fs::metadata(path).map(|_| false)
}

/// Whether other names refer to the file at `path`, so replacing it would detach them. A file
/// that cannot be opened to ask counts as having none: writing it fails anyway.
pub fn has_other_links(path: &Path, _metadata: &Metadata) -> bool {
    let Ok(file) = open_attributes(path) else {
        return false;
    };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is open for the call and `information` is the structure it fills.
    let read =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information) };
    read != 0 && information.nNumberOfLinks > 1
}

/// Always true. Access on Windows is governed by the file's access control list, not by who
/// owns it: a file another account owns may still be the user's to write, which `writable`
/// answers.
pub fn owner_is(_metadata: &Metadata, _uid: &Uid) -> bool {
    true
}

/// `path` with every link and `..` resolved, as the filesystem sees it, in the spelling people
/// and other programs use: `C:\Users\me\.ssh\config`, without the `\\?\` prefix that
/// Windows' own resolution adds, whenever the path means the same without it.
pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(path).map(|resolved| without_verbatim_prefix(&resolved))
}

/// `path` without its `\\?\` (or `\\?\UNC\`) prefix, unless the path needs it: longer than
/// Windows' usual limit, or with a part that Windows would read differently without it (a
/// trailing dot or space, a device name such as `NUL`).
fn without_verbatim_prefix(path: &Path) -> PathBuf {
    const MAX_PATH: usize = 260;
    const DEVICES: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let Some(text) = path.to_str() else {
        return path.into();
    };
    let plain = if let Some(share) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{share}")
    } else if let Some(disk) = text.strip_prefix(r"\\?\") {
        let bytes = disk.as_bytes();
        if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || &bytes[1..3] != b":\\" {
            return path.into();
        }
        disk.into()
    } else {
        return path.into();
    };
    let ordinary = |part: &str| {
        let stem = part.split('.').next().unwrap_or(part).trim_end();
        !part.ends_with(['.', ' '])
            && !DEVICES
                .iter()
                .any(|device| stem.eq_ignore_ascii_case(device))
    };
    if plain.len() < MAX_PATH && plain.split('\\').skip(1).all(ordinary) {
        plain.into()
    } else {
        path.into()
    }
}

/// Creates a symbolic link at `link` that points to `target`: a directory link when `target`
/// (relative to the link's folder) is a directory, a file link otherwise. Windows needs
/// Developer Mode or the symbolic link privilege for either. A link Windows stores with `/` in
/// it does not resolve, so the target is stored with `\`.
pub fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    let target = match target.to_str() {
        Some(text) if text.contains('/') => PathBuf::from(text.replace('/', "\\")),
        _ => target.to_path_buf(),
    };
    let resolved = link.parent().unwrap_or(Path::new("")).join(&target);
    if fs::metadata(resolved).is_ok_and(|metadata| metadata.is_dir()) {
        symlink_dir(&target, link)
    } else {
        symlink_file(&target, link)
    }
}

/// Whether the current user may write to `path`: a file is opened for writing (without
/// truncating it), which the access control list and the read-only attribute both answer; in
/// a directory a file is created, and deleted again as it is closed. Opening the directory
/// itself would not do: an administrator may open it whatever its access rules say. An error
/// that is not a refusal (the path is missing, say) is returned.
pub fn writable(path: &Path) -> io::Result<bool> {
    let opened = if fs::metadata(path)?.is_dir() {
        static PROBES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let probe = PROBES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .attributes(FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_TEMPORARY)
            .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
            .open(path.join(format!(".portico-writable-{}-{probe}", std::process::id())))
    } else {
        OpenOptions::new().write(true).open(path)
    };
    match opened {
        Ok(_) => Ok(true),
        Err(error) if is_read_only_error(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Whether `error` says a file cannot be written to: access denied, or a write-protected disk.
pub fn is_read_only_error(error: &io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| {
        u32::try_from(code)
            .is_ok_and(|code| matches!(code, ERROR_ACCESS_DENIED | ERROR_WRITE_PROTECT))
    })
}

/// Creates the directory `path`; it inherits the access rules of its parent.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

/// Writes `contents` to a new file named after `stem` in `dir`, flushed to disk. `mode` is
/// accepted for the same call as on Unix (see `private`).
pub fn write_unique(
    dir: &Path,
    stem: &str,
    contents: &[u8],
    _mode: u32,
) -> io::Result<PendingFile> {
    #[cfg(test)]
    if stem == ".config.portico-rollback" && take_rollback_write_failure() {
        return Err(io::Error::other("injected rollback write failure"));
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    for attempt in 0..1000 {
        let path = dir.join(format!("{stem}-{}-{nonce}-{attempt}", std::process::id()));
        match write_named_new(&path, contents) {
            Ok(()) => return Ok(PendingFile::new(path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary file",
    ))
}

fn write_named_new(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let result = file.write_all(contents).and_then(|()| file.sync_all());
    if result.is_err() {
        drop(file);
        let _ = fs::remove_file(path);
    }
    result
}

/// Moves `from` over `to` in one step, as `rename(2)` does on Unix.
///
/// `MoveFileExW` with write-through comes first, so the move has reached the disk when it
/// returns. It refuses with "access denied" a target that is open (even by a reader sharing
/// its deletion); the standard library's rename then retries with POSIX semantics, which
/// replace such a target the way Unix does. A read-only target is refused by both (the
/// standard library does not ask to ignore the read-only attribute), as is a target held open
/// without sharing its deletion (an antivirus scan, say); either is retried for up to a second
/// before the error is reported.
pub(in crate::platform) fn replace(from: &Path, to: &Path) -> io::Result<()> {
    let from_wide = wide(from)?;
    let to_wide = wide(to)?;
    let deadline = Instant::now() + REPLACE_PATIENCE;
    loop {
        // SAFETY: both names are NUL-terminated wide strings that outlive the call.
        let moved = unsafe {
            MoveFileExW(
                from_wide.as_ptr(),
                to_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if moved != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if !is_held(&error) {
            return Err(error);
        }
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error) if !is_held(&error) || Instant::now() >= deadline => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Whether `error` is the refusal of a file that is in use (or read-only).
fn is_held(error: &io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| {
        u32::try_from(code)
            .is_ok_and(|code| matches!(code, ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION))
    })
}

/// `path` as a NUL-terminated wide string for the Windows API.
fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains a NUL character",
        ));
    }
    wide.push(0);
    Ok(wide)
}

/// Does nothing but honour the test hooks: NTFS commits a rename's directory entry with the
/// write-through `replace` asks for, and a directory cannot be flushed on its own.
pub fn sync_dir(_path: &Path) -> io::Result<()> {
    #[cfg(test)]
    if take_directory_sync_failure(_path) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "injected directory sync failure",
        ));
    }
    Ok(())
}

/// Opens `path` (a file or a directory, following links) only to read its attributes,
/// sharing it fully with other programs.
fn open_attributes(path: &Path) -> io::Result<fs::File> {
    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// The stamp of the file at `path`, following symbolic links as reading the file does: the
/// volume serial number and file index identify the file, and the change time catches a copy
/// that kept the old last-write time.
pub fn stamp(path: &Path) -> Option<Stamp> {
    let file = open_attributes(path).ok()?;
    let handle = file.as_raw_handle() as HANDLE;
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: the handle is open for both calls, and each fills the structure it is given,
    // whose size is passed where the call asks for it.
    let read = unsafe {
        GetFileInformationByHandle(handle, &mut information) != 0
            && GetFileInformationByHandleEx(
                handle,
                FileBasicInfo,
                (&raw mut basic).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            ) != 0
    };
    if !read {
        return None;
    }
    Some(Stamp {
        device: u64::from(information.dwVolumeSerialNumber),
        inode: u64::from(information.nFileIndexHigh) << 32 | u64::from(information.nFileIndexLow),
        size: u64::from(information.nFileSizeHigh) << 32 | u64::from(information.nFileSizeLow),
        modified: unix_time(basic.LastWriteTime),
        changed: unix_time(basic.ChangeTime),
    })
}

/// A Windows file time (100 ns ticks since 1601) as seconds and nanoseconds since 1970.
fn unix_time(ticks: i64) -> (i64, i64) {
    const TICKS_PER_SECOND: i64 = 10_000_000;
    const SECONDS_FROM_1601_TO_1970: i64 = 11_644_473_600;
    (
        ticks.div_euclid(TICKS_PER_SECOND) - SECONDS_FROM_1601_TO_1970,
        ticks.rem_euclid(TICKS_PER_SECOND) * 100,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_paths_keep_the_usual_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config");
        fs::write(&file, "a").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let resolved = canonicalize(&dir.path().join(r"sub\..\config")).unwrap();
        assert!(
            !resolved.to_string_lossy().starts_with(r"\\?\"),
            "{}",
            resolved.display()
        );
        assert!(resolved.ends_with("config"));
        assert_eq!(fs::read_to_string(&resolved).unwrap(), "a");

        let unchanged = [r"\\?\C:\a\nul", r"\\?\C:\a\b.", r"\\?\Volume{x}\a"];
        for path in unchanged {
            assert_eq!(without_verbatim_prefix(Path::new(path)), Path::new(path));
        }
        let long = format!(r"\\?\C:\{}", "a".repeat(300));
        assert_eq!(without_verbatim_prefix(Path::new(&long)), Path::new(&long));
        assert_eq!(
            without_verbatim_prefix(Path::new(r"\\?\UNC\server\share\a")),
            Path::new(r"\\server\share\a")
        );
        assert_eq!(
            without_verbatim_prefix(Path::new(r"\\?\D:\Users\Jane Doe\.ssh")),
            Path::new(r"D:\Users\Jane Doe\.ssh")
        );
    }

    #[test]
    fn a_directory_denied_to_the_user_is_not_writable() {
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        let user = std::env::var("USERNAME").unwrap();
        let icacls = |args: &[&str]| {
            let output = std::process::Command::new("icacls")
                .arg(&locked)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "icacls {args:?}: {output:?}");
        };
        icacls(&["/deny", &format!("{user}:(W)")]);
        let plain = writable(&locked);
        let verbatim = writable(&fs::canonicalize(&locked).unwrap());
        let resolved = writable(&canonicalize(&locked).unwrap());
        icacls(&["/remove:d", &user]);
        assert_eq!(
            (plain.unwrap(), verbatim.unwrap(), resolved.unwrap()),
            (false, false, false)
        );
        assert!(writable(&locked).unwrap());
    }

    #[test]
    fn a_link_written_with_slashes_resolves() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("dotfiles")).unwrap();
        fs::write(dir.path().join("dotfiles").join("config"), "a").unwrap();
        let link = dir.path().join("config");
        symlink(Path::new("dotfiles/config"), &link).unwrap();
        assert_eq!(fs::read_to_string(&link).unwrap(), "a");
    }

    #[test]
    fn stamp_changes_when_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        assert_eq!(stamp(&path), None);
        fs::write(&path, "a").unwrap();
        let first = stamp(&path).unwrap();
        assert_eq!(stamp(&path), Some(first));
        assert_eq!(first.size, 1);

        fs::write(&path, "bb").unwrap();
        let rewritten = stamp(&path).unwrap();
        assert_ne!(rewritten, first);
        assert_eq!(
            (rewritten.device, rewritten.inode),
            (first.device, first.inode)
        );

        write_unique(dir.path(), ".config", b"bb", 0o600)
            .unwrap()
            .rename_to(&path)
            .unwrap();
        let replaced = stamp(&path).unwrap();
        assert_ne!(replaced.inode, rewritten.inode);
    }

    #[test]
    fn write_unique_replaces_an_open_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config");
        fs::write(&target, "old").unwrap();
        let reader = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(&target)
            .unwrap();

        write_unique(dir.path(), ".config", b"new", 0o600)
            .unwrap()
            .rename_to(&target)
            .unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        drop(reader);
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["config"]);
    }

    #[test]
    fn read_only_attribute_is_the_owner_write_bit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        fs::write(&path, "a").unwrap();
        assert_eq!(mode_of(&fs::metadata(&path).unwrap()), 0o600);
        assert_eq!(mode_of(&fs::metadata(dir.path()).unwrap()), 0o700);
        assert!(writable(&path).unwrap());
        assert!(writable(dir.path()).unwrap());

        set_mode(&path, 0o444).unwrap();
        assert_eq!(mode_of(&fs::metadata(&path).unwrap()), 0o400);
        assert!(!writable(&path).unwrap());

        set_mode(&path, 0o600).unwrap();
        assert!(writable(&path).unwrap());
        assert!(writable(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn hard_links_and_executables_are_recognised() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool.CMD");
        fs::write(&path, "@echo off").unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert!(is_executable(&path, &metadata));
        assert!(!is_executable(&dir.path().join("notes.txt"), &metadata));
        assert!(!has_other_links(&path, &metadata));

        fs::hard_link(&path, dir.path().join("alias.cmd")).unwrap();
        assert!(has_other_links(&path, &fs::metadata(&path).unwrap()));
    }

    #[test]
    fn a_symbolic_link_loop_names_the_path_it_started_from() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        symlink(&second, &first).unwrap();
        symlink(&first, &second).unwrap();
        let (_, error) = symlink_target(&first).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("too many levels of symbolic links from {}", first.display())
        );
    }

    #[test]
    fn symlink_target_follows_a_link_to_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let link = dir.path().join("link");
        fs::write(&real, "a").unwrap();
        symlink(Path::new("real"), &link).unwrap();
        assert_eq!(symlink_target(&link).unwrap(), real);
        assert_eq!(symlink_target(&real).unwrap(), real);
        let missing = dir.path().join("missing").join("file");
        assert_eq!(symlink_target(&missing).unwrap(), missing);
    }
}
