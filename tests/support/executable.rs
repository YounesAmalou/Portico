//! Writes executable scripts that tests run immediately afterwards.
//!
//! Executing a file fails with `ETXTBSY` while any process holds it open for
//! writing. Closing our own handle is not enough: a concurrent `fork` in
//! another test thread duplicates it, and the copy lives until that child
//! reaches `exec`. The only reliable signal is the kernel's own: a read lease
//! cannot be taken while the file has a writer, so the script is handed out
//! once the lease succeeds. macOS refuses to execute a file only while it is
//! being written as swap, so there the script is handed out at once.
//!
//! Windows runs only `.exe` files: the script goes to `<path>.sh` and a small
//! launcher (`sh_shim.rs`, compiled with `rustc` on first use) to `<path>.exe`,
//! which runs the script with `sh`, the arguments passed through a prelude at its top. Running a program by name or by its path
//! without the extension finds the `.exe`.

#[cfg(windows)]
use std::{
    ffi::OsString,
    hash::{DefaultHasher, Hash, Hasher},
    path::PathBuf,
    process::Command,
    sync::OnceLock,
};
use std::{fs, path::Path};
#[cfg(target_os = "linux")]
use std::{
    io,
    os::fd::AsRawFd,
    thread,
    time::{Duration, Instant},
};
#[cfg(unix)]
use std::{
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
};

/// The POSIX `sh` that runs the fakes: `/bin/sh`, or on Windows the one `sh_shim` uses (the
/// `SH` variable, else Git for Windows' own).
#[allow(dead_code, reason = "only some tests run a script themselves")]
pub fn posix_sh() -> std::path::PathBuf {
    if cfg!(windows) {
        std::env::var_os("SH").map_or_else(|| r"C:\Program Files\Git\bin\sh.exe".into(), Into::into)
    } else {
        "/bin/sh".into()
    }
}

/// Removes the executable script `write_executable` made at `path`.
#[allow(dead_code, reason = "only some tests remove a fake")]
pub fn remove_executable(path: impl AsRef<Path>) {
    let path = path.as_ref();
    #[cfg(unix)]
    fs::remove_file(path).unwrap();
    #[cfg(windows)]
    for suffix in [".exe", ".sh"] {
        fs::remove_file(with_suffix(path, suffix)).unwrap();
    }
}

/// Creates or replaces the executable script at `path`.
#[cfg(unix)]
pub fn write_executable(path: impl AsRef<Path>, contents: &str) {
    let path = path.as_ref();
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o700)
        .open(path)
        .unwrap();
    file.write_all(contents.as_bytes()).unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o700))
        .unwrap();
    drop(file);
    wait_until_no_writers(path);
}

/// Creates or replaces the executable script at `path`: the script at `<path>.sh`, run by the
/// launcher at `<path>.exe`. A launcher already there is kept, as it may be running.
#[cfg(windows)]
pub fn write_executable(path: impl AsRef<Path>, contents: &str) {
    let path = path.as_ref();
    // The prelude goes after the `#!` line, before the script's first command.
    let (shebang, body) = match contents.strip_prefix("#!") {
        Some(_) => contents.split_at(contents.find('\n').map_or(contents.len(), |end| end + 1)),
        None => ("", contents),
    };
    fs::write(
        with_suffix(path, ".sh"),
        format!("{shebang}{SHIM_PRELUDE}{body}"),
    )
    .unwrap();
    let launcher = with_suffix(path, ".exe");
    if !launcher.exists() {
        fs::copy(shim(), &launcher).unwrap();
    }
}

/// Rebuilds `"$@"` from the arguments `sh_shim` passes in the environment, and removes them
/// from it, so that a fake the script starts gets only its own; restores the `HOME` the shim
/// was given.
#[cfg(windows)]
const SHIM_PRELUDE: &str = r#"if [ -n "${PORTICO_SHIM_HOME+set}" ]; then HOME=$PORTICO_SHIM_HOME; fi
unset PORTICO_SHIM_HOME
portico_shim_i=1
while [ "$portico_shim_i" -le "${PORTICO_SHIM_ARGC:-0}" ]; do
  eval "set -- \"\$@\" \"\$PORTICO_SHIM_ARG_$portico_shim_i\""
  unset "PORTICO_SHIM_ARG_$portico_shim_i"
  portico_shim_i=$((portico_shim_i + 1))
done
unset portico_shim_i PORTICO_SHIM_ARGC
"#;

/// `path` with `suffix` appended, keeping any dot already in its name.
#[cfg(windows)]
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

/// The compiled launcher, shared by every test binary.
#[cfg(windows)]
fn shim() -> &'static Path {
    static SHIM: OnceLock<PathBuf> = OnceLock::new();
    SHIM.get_or_init(|| compile_program("tests/support/sh_shim.rs"))
}

/// Compiles the test program at `source` (relative to the crate) with `rustc`, once per source
/// text into the temporary directory, and returns its path. Every test program may include
/// `tests/support/job.rs`, so that file's text counts as part of each one's source.
#[cfg(windows)]
#[allow(
    dead_code,
    reason = "only some test binaries compile programs of their own"
)]
pub fn compile_program(source: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = root.join(source);
    let mut hasher = DefaultHasher::new();
    for file in [source.as_path(), &root.join("tests/support/job.rs")] {
        let text = fs::read(file)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", file.display()));
        text.hash(&mut hasher);
    }
    std::env::consts::ARCH.hash(&mut hasher);
    let dir = std::env::temp_dir().join("portico-test-programs");
    let stem = source.file_stem().unwrap().to_string_lossy();
    let program = dir.join(format!("{stem}-{:016x}.exe", hasher.finish()));
    if program.exists() {
        return program;
    }
    fs::create_dir_all(&dir).unwrap();
    // Built under a name of its own, then renamed into place whole, as other test binaries
    // may be building it at the same time.
    let partial = dir.join(format!("{stem}-{}.exe", std::process::id()));
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(&rustc)
        .args(["--edition", "2021", "-C", "debuginfo=0", "-o"])
        .arg(&partial)
        .arg(&source)
        .output()
        .unwrap_or_else(|error| panic!("cannot run {rustc:?}: {error}"));
    if !output.status.success() {
        let _ = fs::remove_file(&partial);
        panic!(
            "cannot compile {}:\n{}",
            source.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // A virus scanner may hold the new file for a moment, so a failed rename is retried.
    let mut attempts = 0;
    while let Err(error) = fs::rename(&partial, &program) {
        if program.exists() {
            // Another test binary put its own copy there first.
            let _ = fs::remove_file(&partial);
            break;
        }
        attempts += 1;
        assert!(
            attempts < 50,
            "cannot rename {}: {error}",
            partial.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    program
}

#[cfg(target_os = "macos")]
fn wait_until_no_writers(_path: &Path) {}

#[cfg(target_os = "linux")]
fn wait_until_no_writers(path: &Path) {
    let file = fs::File::open(path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match set_lease(&file, libc::F_RDLCK) {
            Ok(()) => {
                set_lease(&file, libc::F_UNLCK).unwrap();
                return;
            }
            Err(error) if error.raw_os_error() == Some(libc::EAGAIN) => {
                assert!(
                    Instant::now() < deadline,
                    "{} is still open for writing",
                    path.display()
                );
                thread::yield_now();
            }
            Err(error) => panic!("could not check {} for writers: {error}", path.display()),
        }
    }
}

#[cfg(target_os = "linux")]
fn set_lease(file: &fs::File, lease: libc::c_int) -> io::Result<()> {
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, lease) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
