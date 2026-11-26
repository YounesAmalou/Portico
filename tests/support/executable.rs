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
//! which runs the script with `sh`. Running a program by name or by its path
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
    fs::write(with_suffix(path, ".sh"), contents).unwrap();
    let launcher = with_suffix(path, ".exe");
    if !launcher.exists() {
        fs::copy(shim(), &launcher).unwrap();
    }
}

/// `path` with `suffix` appended, keeping any dot already in its name.
#[cfg(windows)]
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

/// The compiled launcher, built once per source text into the temporary directory and shared
/// by every test binary.
#[cfg(windows)]
fn shim() -> &'static Path {
    static SHIM: OnceLock<PathBuf> = OnceLock::new();
    SHIM.get_or_init(|| {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/sh_shim.rs");
        let text = fs::read(&source)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", source.display()));
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        std::env::consts::ARCH.hash(&mut hasher);
        let dir = std::env::temp_dir().join("portico-sh-shim");
        let shim = dir.join(format!("sh_shim-{:016x}.exe", hasher.finish()));
        if shim.exists() {
            return shim;
        }
        fs::create_dir_all(&dir).unwrap();
        // Built under a name of its own, then renamed into place whole, as other test
        // binaries may be building it at the same time.
        let partial = dir.join(format!("sh_shim-{}.exe", std::process::id()));
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
        while let Err(error) = fs::rename(&partial, &shim) {
            if shim.exists() {
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
        shim
    })
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
