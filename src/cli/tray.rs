//! `portico desktop install | uninstall | status` on macOS and Windows: the tray app (the menu
//! bar app on macOS) and starting it at login.

use std::{
    fs,
    io::{self, BufRead},
    path::{Path, PathBuf},
    process::ExitCode,
};

use super::desktop::{DesktopCommand, Out, prompt, w};
use crate::desktop::{
    DesktopEnv,
    status::{self, Status},
    tray::{self, Expected, System, TrayHost},
};

/// `portico desktop install`'s choices on macOS and Windows.
#[derive(Clone, Debug, Default)]
pub struct TrayOptions {
    /// A local archive to install from instead of downloading.
    pub from: Option<PathBuf>,
    /// Set up starting the app at login.
    pub login: bool,
    pub yes: bool,
    /// Update only the app, as its banner asks: no questions, the login entry left as it is.
    pub tray_only: bool,
}

/// The release this `portico` installs the app from.
struct Release<'a> {
    version: &'a str,
    /// The release target, `None` where no app is released (only `--from` works there).
    target: Option<&'a str>,
    expected: Expected,
}

/// The labels are padded to this width, and the lines under them indented by it.
const COLUMN: usize = 19;

/// Runs `portico desktop install | uninstall | status` for the tray app of `system`.
pub fn run(
    command: DesktopCommand,
    env: &DesktopEnv,
    system: System,
    host: &dyn TrayHost,
    input: &mut dyn BufRead,
    output: Out,
    interactive: bool,
) -> Result<ExitCode, String> {
    match command {
        DesktopCommand::Install {
            omarchy,
            quickshell,
            bind,
            no_bind,
            no_panel,
            no_launcher,
            yes,
            no_login,
            from,
            tray_only,
        } => {
            let linux_only = [
                ("--omarchy", omarchy),
                ("--quickshell", quickshell),
                ("--bind", bind.is_some()),
                ("--no-bind", no_bind),
                ("--no-panel", no_panel),
                ("--no-launcher", no_launcher),
            ];
            if let Some((flag, _)) = linux_only.iter().find(|(_, set)| *set) {
                return Err(format!("{flag} is only on Linux"));
            }
            let options = TrayOptions {
                from,
                login: !no_login,
                yes,
                tray_only,
            };
            install(env, system, host, &options, input, output, interactive)
        }
        DesktopCommand::Uninstall { yes } => {
            uninstall(env, system, host, yes, input, output, interactive)
        }
        DesktopCommand::Status => {
            for line in status_lines(env, system, host).lines {
                writeln!(output, "{}", line.render()).map_err(w)?;
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Installs or updates the app and sets up starting it at login, after a preview and a
/// question per piece (none with `--yes`).
pub fn install(
    env: &DesktopEnv,
    system: System,
    host: &dyn TrayHost,
    options: &TrayOptions,
    input: &mut dyn BufRead,
    output: Out,
    interactive: bool,
) -> Result<ExitCode, String> {
    let release = Release {
        version: env!("CARGO_PKG_VERSION"),
        target: tray::target(),
        expected: tray::expected_digest(),
    };
    install_release(
        env,
        system,
        host,
        options,
        &release,
        input,
        output,
        interactive,
    )
}

/// The app, fetched and unpacked into the scratch directory, ready to be moved into place.
struct Fetched {
    /// Where it comes from and how it was checked, for the preview.
    lines: Vec<String>,
    /// The unpacked app (`Paths::placed`'s counterpart).
    app: PathBuf,
    version: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn install_release(
    env: &DesktopEnv,
    system: System,
    host: &dyn TrayHost,
    options: &TrayOptions,
    release: &Release<'_>,
    input: &mut dyn BufRead,
    output: Out,
    interactive: bool,
) -> Result<ExitCode, String> {
    if options.tray_only {
        return update_only(env, system, host, options, release, output);
    }
    // Nothing from a local archive runs (its app is asked its version) before this check.
    if options.from.is_some() && !options.yes && !interactive {
        return Err("desktop install requires --yes in non-interactive mode".into());
    }
    let exe = env
        .exe
        .as_deref()
        .ok_or("cannot find portico's executable")?;
    let portico = tray::stable_portico(exe, env.path.as_deref());
    let paths = tray::paths(system, &env.home, env.local_app_data.as_deref());
    let support = tray::support_dir(system, &env.home, env.local_app_data.as_deref());
    let scratch = Scratch::new(&support)?;

    writeln!(
        output,
        "Portico {}: desktop integration for {}.\n",
        release.version,
        system.name()
    )
    .map_err(w)?;
    let placed = paths.placed(system);
    let present = fs::symlink_metadata(placed).is_ok();
    let installed = if present {
        host.app_version(placed)
    } else {
        None
    };
    // A local archive is read first: its version decides whether there is anything to do.
    let mut fetched = match &options.from {
        Some(_) => Some(fetch(
            env,
            system,
            host,
            release,
            options.from.as_deref(),
            &scratch,
            output,
        )?),
        None => None,
    };
    let wanted = match &fetched {
        Some(fetched) => fetched.version.clone(),
        None => Some(release.version.to_string()),
    };
    let app_pending = !present || installed.is_none() || installed != wanted;
    let entry = host.login_entry();
    let wanted_entry = tray::login_entry(system, &paths.binary, &portico);
    let login_pending = options.login && entry.as_deref() != Some(wanted_entry.as_str());
    let pad = "";

    if !app_pending {
        writeln!(
            output,
            "{:<COLUMN$}installed, {}",
            system.app_name(),
            installed.as_deref().unwrap_or_default()
        )
        .map_err(w)?;
    }
    if options.login && !login_pending {
        writeln!(
            output,
            "{:<COLUMN$}set up (runs {})",
            "Start at login",
            env.show(&portico)
        )
        .map_err(w)?;
    }
    if !app_pending && !login_pending {
        writeln!(output, "\nNothing to install.").map_err(w)?;
        return Ok(ExitCode::SUCCESS);
    }
    if !options.yes && !interactive {
        return Err("desktop install requires --yes in non-interactive mode".into());
    }
    if app_pending && fetched.is_none() {
        fetched = Some(fetch(env, system, host, release, None, &scratch, output)?);
    }

    let ask = |input: &mut dyn BufRead, output: Out, question: &str| -> Result<bool, String> {
        if options.yes {
            return Ok(true);
        }
        let answer = prompt(input, output, &format!("{question} [y/N] "))?;
        Ok(matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"))
    };
    let running = !host.running().is_empty();
    let mut install_app = false;
    if app_pending && let Some(fetched) = &fetched {
        let state = match (present, &installed) {
            (false, _) => "not installed".to_string(),
            (true, Some(version)) => format!("installed, {version}"),
            (true, None) => "installed, version unknown".to_string(),
        };
        writeln!(
            output,
            "{:<COLUMN$}{state} → {} {}",
            system.app_name(),
            tray::PRODUCT,
            fetched.version.as_deref().unwrap_or("of unknown version")
        )
        .map_err(w)?;
        for line in &fetched.lines {
            writeln!(output, "{pad:COLUMN$}{line}").map_err(w)?;
        }
        writeln!(output, "{pad:COLUMN$}install to {}", env.show(placed)).map_err(w)?;
        if running {
            writeln!(output, "{pad:COLUMN$}stops the running app first").map_err(w)?;
        }
        install_app = ask(
            input,
            output,
            if present { "Update it?" } else { "Install it?" },
        )?;
    }
    let mut set_login = false;
    if login_pending {
        if present || install_app {
            let state = match &entry {
                None => "not set up".to_string(),
                Some(text) => match tray::entry_portico(system, text) {
                    Some(old) => format!("set up for {}", env.show(&old)),
                    None => "set up differently".to_string(),
                },
            };
            writeln!(
                output,
                "{:<COLUMN$}{state} → {}",
                "Start at login",
                system.login_name()
            )
            .map_err(w)?;
            writeln!(
                output,
                "{pad:COLUMN$}starts the app with --portico {}",
                env.show(&portico)
            )
            .map_err(w)?;
            if env.is_development_build() {
                writeln!(
                    output,
                    "{pad:COLUMN$}note: {} is a cargo build directory's binary",
                    env.show(exe)
                )
                .map_err(w)?;
            }
            set_login = ask(input, output, "Set it up?")?;
        } else {
            writeln!(
                output,
                "Skipping starting at login: it starts the {}, which is not installed.",
                system.app_noun()
            )
            .map_err(w)?;
        }
    }

    // Every question is answered: now the changes.
    if !options.yes {
        writeln!(output).map_err(w)?;
    }
    let mut failed = false;
    let mut report = |output: Out, result: Result<String, String>| -> Result<(), String> {
        match result {
            Ok(done) => writeln!(output, "{done}").map_err(w),
            Err(error) => {
                failed = true;
                writeln!(output, "error: {error}").map_err(w)
            }
        }
    };
    if install_app || set_login {
        let stopped = host
            .stop(false)
            .map(|()| format!("Stopped the {}.", system.app_noun()));
        if running || stopped.is_err() {
            report(output, stopped)?;
        }
    }
    if let Some(fetched) = fetched.as_ref().filter(|_| install_app) {
        let replaced = replace(placed, &fetched.app, &scratch).map(|()| {
            format!(
                "Installed {} ({} {}).",
                env.show(placed),
                tray::PRODUCT,
                fetched.version.as_deref().unwrap_or("of unknown version")
            )
        });
        let replaced_ok = replaced.is_ok();
        report(output, replaced)?;
        if replaced_ok {
            clear_update_log(env, &support, output)?;
        }
    } else if app_pending {
        writeln!(output, "Skipped the {}.", system.app_noun()).map_err(w)?;
    }
    if set_login {
        let set = host
            .set_login_entry(&paths.binary, &portico)
            .map(|()| format!("Set up the {}.", system.login_name()));
        report(output, set)?;
    } else if login_pending && (present || install_app) {
        writeln!(output, "Skipped starting at login.").map_err(w)?;
    }
    if (install_app || set_login) && paths.binary.is_file() {
        let via_login = host.login_entry().is_some();
        let started = host
            .start(&paths.binary, &portico, via_login)
            .map(|()| format!("Started the {}.", system.app_noun()))
            .map_err(|error| format!("could not start the {}: {error}", system.app_noun()));
        report(output, started)?;
    }

    writeln!(output).map_err(w)?;
    if failed {
        writeln!(
            output,
            "Some steps failed (see above). Fix them and run portico desktop install again."
        )
        .map_err(w)?;
    } else {
        writeln!(output, "Done.").map_err(w)?;
    }
    writeln!(
        output,
        "Run portico desktop status to check it, and portico desktop uninstall to remove it."
    )
    .map_err(w)?;
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// `--tray-only`, the app's own update: no questions, the login entry left as it is. On
/// success the update log is removed; on any failure the error is written, and then whichever
/// app is in place is started: the error is the last line it finds in the log.
fn update_only(
    env: &DesktopEnv,
    system: System,
    host: &dyn TrayHost,
    options: &TrayOptions,
    release: &Release<'_>,
    output: Out,
) -> Result<ExitCode, String> {
    let paths = tray::paths(system, &env.home, env.local_app_data.as_deref());
    let support = tray::support_dir(system, &env.home, env.local_app_data.as_deref());
    let placed = paths.placed(system);
    let entry = host.login_entry();
    let via_login = entry.is_some();
    // The `portico` the app is given: this one, else the one its login entry names.
    let portico = match env.exe.as_deref() {
        Some(exe) => tray::stable_portico(exe, env.path.as_deref()),
        None => entry
            .as_deref()
            .and_then(|entry| tray::entry_portico(system, entry))
            .unwrap_or_else(|| PathBuf::from("portico")),
    };
    // The app is quitting as it hands over: it is waited for first, so that the app started at
    // the end, new or old, is not refused as a second copy.
    let mut updated = || -> Result<Option<String>, String> {
        host.stop(true)?;
        env.exe
            .as_deref()
            .ok_or("cannot find portico's executable")?;
        let scratch = Scratch::new(&support)?;
        let fetched = fetch(
            env,
            system,
            host,
            release,
            options.from.as_deref(),
            &scratch,
            &mut *output,
        )?;
        replace(placed, &fetched.app, &scratch)?;
        Ok(fetched.version)
    };
    let updated = updated();
    let start = || {
        if paths.binary.is_file() {
            host.start(&paths.binary, &portico, via_login)
        } else {
            Err(format!("{} is not installed", env.show(placed)))
        }
        .map_err(|error| format!("could not start the {}: {error}", system.app_noun()))
    };
    match updated {
        Ok(version) => {
            writeln!(
                output,
                "Updated {} to {} {}.",
                env.show(placed),
                tray::PRODUCT,
                version.as_deref().unwrap_or("of unknown version")
            )
            .map_err(w)?;
            start()?;
            writeln!(output, "Started the {}.", system.app_noun()).map_err(w)?;
            clear_update_log(env, &support, output)?;
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            // The error is in the log before the app in place starts and reads it; nothing
            // follows it unless that app cannot be started either.
            if paths.binary.is_file() {
                writeln!(output, "Starting the {} in place.", system.app_noun()).map_err(w)?;
            }
            writeln!(output, "error: {error}").map_err(w)?;
            output.flush().map_err(w)?;
            if let Err(start) = start() {
                writeln!(output, "error: {start}").map_err(w)?;
            }
            Ok(ExitCode::FAILURE)
        }
    }
}

/// Removes the app's update log after a successful replace or removal, so that no app shows
/// an earlier "Update failed". A log that cannot be removed is noted, not an error.
fn clear_update_log(env: &DesktopEnv, support: &Path, output: Out) -> Result<(), String> {
    let log = support.join(tray::UPDATE_LOG);
    match fs::remove_file(&log) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            writeln!(output, "note: cannot remove {}: {error}", env.show(&log)).map_err(w)
        }
    }
}

/// Fetches the app into `scratch`: the release asset, checked against the expected digest,
/// or the archive at `from`, unchecked; then unpacks it and checks its layout.
fn fetch(
    env: &DesktopEnv,
    system: System,
    host: &dyn TrayHost,
    release: &Release<'_>,
    from: Option<&Path>,
    scratch: &Scratch,
    output: Out,
) -> Result<Fetched, String> {
    let (archive, lines) = match from {
        Some(path) => {
            let meta = fs::metadata(path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            if !meta.is_file() {
                return Err(format!(
                    "--from takes an archive in the release layout (scripts/build-tray.sh makes one); {} is not a file",
                    path.display()
                ));
            }
            (
                path.to_path_buf(),
                vec![
                    format!("unpack {} ({}),", env.show(path), size(meta.len())),
                    "its digest is not checked (--from)".to_string(),
                ],
            )
        }
        None => {
            let target = release.target.ok_or(
                "no tray app is released for this system; install one from a local archive with --from <path>",
            )?;
            let name = tray::asset_name(system, release.version, target);
            let archive = scratch.0.join(&name);
            writeln!(output, "Downloading {name}…").map_err(w)?;
            host.download(&tray::asset_url(release.version, &name), &archive)
                .map_err(|error| missing_release(release.version, error))?;
            let (expected, against) = match &release.expected {
                Expected::Baked(digest) => (digest.clone(), "the digest built into this portico"),
                Expected::Sums => {
                    let file = scratch.0.join("SHA256SUMS");
                    host.download(&tray::sums_url(release.version), &file)
                        .map_err(|error| missing_release(release.version, error))?;
                    let sums = fs::read_to_string(&file).map_err(|error| {
                        format!("cannot read the release's SHA256SUMS: {error}")
                    })?;
                    let digest = tray::sums_digest(&sums, &name).ok_or_else(|| {
                        format!("the release's SHA256SUMS has no line for {name}")
                    })?;
                    (digest, "the release's SHA256SUMS")
                }
            };
            let actual = tray::sha256_file(&archive)
                .map_err(|error| format!("cannot read the download: {error}"))?;
            if actual != expected {
                let _ = fs::remove_file(&archive);
                return Err(format!(
                    "{name} does not match {against}: its SHA-256 is {actual}, expected {expected}. The download was deleted."
                ));
            }
            let bytes = fs::metadata(&archive).map_or(0, |meta| meta.len());
            (
                archive,
                vec![
                    format!("download {name} ({}),", size(bytes)),
                    format!("checked against {against}"),
                ],
            )
        }
    };
    let unpacked = scratch.0.join("unpacked");
    fs::create_dir(&unpacked)
        .map_err(|error| format!("cannot create {}: {error}", unpacked.display()))?;
    host.unpack(&archive, &unpacked)?;
    let app = tray::check_layout(&unpacked, system).map_err(|problem| {
        format!(
            "{} is not a {} archive: {problem}",
            archive.display(),
            tray::PRODUCT
        )
    })?;
    // A download is the release of this version; only a local archive's app is asked (which
    // on Windows runs it).
    let version = match from {
        Some(_) => host.app_version(&app),
        None => Some(release.version.to_string()),
    };
    Ok(Fetched {
        lines,
        app,
        version,
    })
}

/// A failed download: a 404 (as curl's last stderr line reports it, never a number elsewhere
/// in the command line) means this version has no release.
fn missing_release(version: &str, error: String) -> String {
    if error.contains("returned error: 404") {
        format!(
            "Portico {version} has no tray app to download ({error}). Install one from a local archive with --from <path>; scripts/build-tray.sh makes one."
        )
    } else {
        format!("cannot download the tray app: {error}")
    }
}

/// `bytes` for a person: `7 bytes`, `12 kB`, `1.4 MB`.
fn size(bytes: u64) -> String {
    if bytes < 1000 {
        format!("{bytes} bytes")
    } else if bytes < 1_000_000 {
        format!("{} kB", bytes.div_ceil(1000))
    } else {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    }
}

/// Moves the unpacked `new` app to `placed`, replacing what is there. The old app is moved
/// into `scratch` first (the same volume), and back if the new one cannot be moved in.
fn replace(placed: &Path, new: &Path, scratch: &Scratch) -> Result<(), String> {
    if let Some(parent) = placed.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let old = scratch.0.join("previous");
    let had_old = match fs::symlink_metadata(placed) {
        Ok(_) => {
            fs::rename(placed, &old)
                .map_err(|error| format!("cannot move {} aside: {error}", placed.display()))?;
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(format!("cannot read {}: {error}", placed.display())),
    };
    if let Err(error) = fs::rename(new, placed) {
        if had_old {
            let _ = fs::rename(&old, placed);
        }
        return Err(format!(
            "cannot move the app to {}: {error}",
            placed.display()
        ));
    }
    Ok(())
}

/// A private directory for the download and the unpacked app, in the app's support directory
/// (on the same volume as the app, so the app moves into place by a rename), removed when
/// dropped, on every path.
struct Scratch(PathBuf);

impl Scratch {
    fn new(support: &Path) -> Result<Self, String> {
        let failed = |error: io::Error| {
            format!(
                "cannot create a directory in {}: {error}",
                support.display()
            )
        };
        fs::create_dir_all(support).map_err(failed)?;
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        for attempt in 0u32..100 {
            let dir = support.join(format!(
                "install-{}-{:x}",
                std::process::id(),
                seed.wrapping_add(u128::from(attempt))
            ));
            match fs::DirBuilder::new().create(&dir) {
                Ok(()) => {
                    let scratch = Scratch(dir);
                    crate::platform::fs::set_mode(&scratch.0, 0o700).map_err(failed)?;
                    return Ok(scratch);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(failed(error)),
            }
        }
        Err(failed(io::ErrorKind::AlreadyExists.into()))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Stops the app, removes its login entry and removes it, after a preview and a question
/// (none with `--yes`).
pub fn uninstall(
    env: &DesktopEnv,
    system: System,
    host: &dyn TrayHost,
    yes: bool,
    input: &mut dyn BufRead,
    output: Out,
    interactive: bool,
) -> Result<ExitCode, String> {
    let paths = tray::paths(system, &env.home, env.local_app_data.as_deref());
    let placed = paths.placed(system);
    let entry = host.login_entry().is_some();
    let running = host.running();
    let present = fs::symlink_metadata(placed).is_ok();
    let noun = system.app_noun();
    if !entry && running.is_empty() && !present {
        writeln!(output, "Portico's {noun} is not installed.").map_err(w)?;
        return Ok(ExitCode::SUCCESS);
    }
    let login = match &paths.launch_agent {
        Some(plist) => format!("{} ({})", env.show(plist), system.login_name()),
        None => system.login_name(),
    };
    writeln!(output, "Portico's {noun}:").map_err(w)?;
    if !running.is_empty() {
        let pids = running
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let process = if running.len() == 1 {
            "process"
        } else {
            "processes"
        };
        writeln!(output, "  stop     the running app ({process} {pids})").map_err(w)?;
    }
    if entry {
        writeln!(output, "  remove   {login}").map_err(w)?;
    }
    if present {
        writeln!(output, "  remove   {}", env.show(placed)).map_err(w)?;
    }
    if !yes {
        if !interactive {
            return Err("desktop uninstall requires --yes in non-interactive mode".into());
        }
        let answer = prompt(input, output, &format!("Remove the {noun}? [y/N] "))?;
        if !matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") {
            writeln!(output, "Nothing was removed.").map_err(w)?;
            return Ok(ExitCode::SUCCESS);
        }
    }
    let mut ok = true;
    let mut report = |output: Out, result: Result<String, String>| -> Result<(), String> {
        match result {
            Ok(done) => writeln!(output, "{done}").map_err(w),
            Err(error) => {
                ok = false;
                writeln!(output, "error: {error}").map_err(w)
            }
        }
    };
    // Stopped first: on macOS this unloads the LaunchAgent, whose file goes next.
    let stopped = host.stop(false).map(|()| format!("Stopped the {noun}."));
    if !running.is_empty() || stopped.is_err() {
        report(output, stopped)?;
    }
    if entry {
        let removed = host
            .remove_login_entry()
            .map(|()| format!("Removed the {login}."));
        report(output, removed)?;
    }
    if present {
        let removed = remove_app(system, &paths).map(|()| format!("Removed {}.", env.show(placed)));
        let removed_ok = removed.is_ok();
        report(output, removed)?;
        if removed_ok {
            let support = tray::support_dir(system, &env.home, env.local_app_data.as_deref());
            clear_update_log(env, &support, output)?;
        }
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Removes the app: the bundle on macOS; on Windows the executable, then its directory when
/// nothing else is left in it.
fn remove_app(system: System, paths: &tray::Paths) -> Result<(), String> {
    let placed = paths.placed(system);
    let failed = |error: io::Error| format!("cannot remove {}: {error}", placed.display());
    match system {
        System::MacOs => fs::remove_dir_all(placed).map_err(failed),
        System::Windows => {
            fs::remove_file(placed).map_err(failed)?;
            let _ = fs::remove_dir(&paths.app);
            Ok(())
        }
    }
}

/// What `portico desktop status` prints on macOS and Windows.
pub fn status_lines(env: &DesktopEnv, system: System, host: &dyn TrayHost) -> Status {
    status::tray_status(env, system, host)
}

#[cfg(test)]
fn status_with(env: &DesktopEnv, system: System, host: &dyn TrayHost, version: &str) -> Status {
    status::tray_status_with(env, system, host, version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::tray::{self, paths, plist_version, sha256_file};
    use std::{
        cell::RefCell,
        collections::HashMap,
        fs,
        io::Write,
        path::{Path, PathBuf},
        rc::Rc,
    };

    const VERSION: &str = "0.8.0";

    fn target(system: System) -> &'static str {
        match system {
            System::MacOs => "aarch64-apple-darwin",
            System::Windows => "x86_64-pc-windows-msvc",
        }
    }

    /// A fake system: downloads are served from `served`, an "archive" is a text file naming
    /// the version its app has, and the app's version is read back from the unpacked files.
    struct FakeHost {
        system: System,
        calls: RefCell<Vec<String>>,
        served: RefCell<HashMap<String, PathBuf>>,
        entry: RefCell<Option<String>>,
        running: RefCell<Vec<u32>>,
        fail_unpack: bool,
        /// What the command has written so far, as the update log would hold it.
        written: Rc<RefCell<Vec<u8>>>,
        /// The last line written when the app was started, as the started app would read it.
        last_line_at_start: RefCell<Option<String>>,
    }

    /// The command's output, shared with the [`FakeHost`].
    struct Shared(Rc<RefCell<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl FakeHost {
        fn new(system: System) -> Self {
            Self {
                system,
                calls: RefCell::new(Vec::new()),
                served: RefCell::new(HashMap::new()),
                entry: RefCell::new(None),
                running: RefCell::new(Vec::new()),
                fail_unpack: false,
                written: Rc::default(),
                last_line_at_start: RefCell::new(None),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn serve(&self, url: String, file: PathBuf) {
            self.served.borrow_mut().insert(url, file);
        }
    }

    impl TrayHost for FakeHost {
        fn download(&self, url: &str, to: &Path) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("download {url}"));
            match self.served.borrow().get(url) {
                Some(file) => fs::copy(file, to).map(drop).map_err(|e| e.to_string()),
                None => Err(format!(
                    "curl -o {} {url} failed (exit status: 22): curl: (22) The requested URL returned error: 404",
                    to.display()
                )),
            }
        }

        fn unpack(&self, archive: &Path, into: &Path) -> Result<(), String> {
            self.calls.borrow_mut().push("unpack".into());
            if self.fail_unpack {
                return Err("tar -xzf failed: truncated archive".into());
            }
            let version = fs::read_to_string(archive).map_err(|e| e.to_string())?;
            make_app(self.system, into, version.trim());
            Ok(())
        }

        fn app_version(&self, app: &Path) -> Option<String> {
            match self.system {
                System::MacOs => {
                    plist_version(&fs::read_to_string(app.join("Contents/Info.plist")).ok()?)
                }
                System::Windows => fs::read_to_string(app)
                    .ok()?
                    .strip_prefix("portico-tray ")
                    .map(str::to_string),
            }
        }

        fn login_entry(&self) -> Option<String> {
            self.entry.borrow().clone()
        }

        fn set_login_entry(&self, binary: &Path, portico: &Path) -> Result<(), String> {
            self.calls.borrow_mut().push("set_login_entry".into());
            *self.entry.borrow_mut() = Some(tray::login_entry(self.system, binary, portico));
            Ok(())
        }

        fn remove_login_entry(&self) -> Result<(), String> {
            self.calls.borrow_mut().push("remove_login_entry".into());
            *self.entry.borrow_mut() = None;
            Ok(())
        }

        fn running(&self) -> Vec<u32> {
            self.running.borrow().clone()
        }

        fn stop(&self, during_update: bool) -> Result<(), String> {
            self.calls
                .borrow_mut()
                .push(format!("stop {during_update}"));
            self.running.borrow_mut().clear();
            Ok(())
        }

        fn start(&self, binary: &Path, portico: &Path, via_login: bool) -> Result<(), String> {
            assert!(binary.is_file(), "started a missing {}", binary.display());
            self.calls.borrow_mut().push(format!(
                "start {} {} {via_login}",
                binary.display(),
                portico.display()
            ));
            *self.running.borrow_mut() = vec![42];
            let written = String::from_utf8(self.written.borrow().clone()).unwrap();
            *self.last_line_at_start.borrow_mut() = written.lines().last().map(str::to_string);
            Ok(())
        }
    }

    /// The app of `version` as its archive holds it, in `dir`.
    fn make_app(system: System, dir: &Path, version: &str) {
        match system {
            System::MacOs => {
                let contents = dir.join("Portico Tray.app/Contents");
                fs::create_dir_all(contents.join("MacOS")).unwrap();
                fs::write(contents.join("MacOS/portico-tray"), "app").unwrap();
                fs::write(
                    contents.join("Info.plist"),
                    format!("<key>CFBundleShortVersionString</key>\n<string>{version}</string>"),
                )
                .unwrap();
            }
            System::Windows => {
                fs::create_dir_all(dir).unwrap();
                fs::write(
                    dir.join("portico-tray.exe"),
                    format!("portico-tray {version}"),
                )
                .unwrap();
            }
        }
    }

    struct Setup {
        dir: tempfile::TempDir,
        env: DesktopEnv,
        host: FakeHost,
        system: System,
    }

    impl Setup {
        fn new(system: System) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            let bin = dir.path().join("bin");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&bin).unwrap();
            fs::write(bin.join("portico"), "portico").unwrap();
            let lad = home.join("AppData/Local");
            let env = DesktopEnv::new(
                home,
                move |name| (name == "LOCALAPPDATA").then(|| lad.clone().into()),
                Some(bin.join("portico")),
            );
            Self {
                dir,
                env,
                host: FakeHost::new(system),
                system,
            }
        }

        fn name(&self) -> String {
            tray::asset_name(self.system, VERSION, target(self.system))
        }

        /// The release's archive, of an app of `version`, served at its URL; its digest.
        fn release(&self, version: &str) -> String {
            let archive = self.dir.path().join(self.name());
            fs::write(&archive, version).unwrap();
            self.host
                .serve(tray::asset_url(VERSION, &self.name()), archive.clone());
            sha256_file(&archive).unwrap()
        }

        fn serve_sums(&self, text: &str) {
            let sums = self.dir.path().join("SHA256SUMS");
            fs::write(&sums, text).unwrap();
            self.host.serve(tray::sums_url(VERSION), sums);
        }

        fn paths(&self) -> tray::Paths {
            paths(
                self.system,
                &self.env.home,
                self.env.local_app_data.as_deref(),
            )
        }

        fn placed(&self) -> PathBuf {
            self.paths().placed(self.system).to_path_buf()
        }

        fn portico(&self) -> PathBuf {
            self.env.exe.clone().unwrap()
        }

        fn support(&self) -> PathBuf {
            tray::support_dir(
                self.system,
                &self.env.home,
                self.env.local_app_data.as_deref(),
            )
        }

        /// Whether the install left no scratch directory behind.
        fn scratch_removed(&self) -> bool {
            fs::read_dir(self.support())
                .map(|entries| entries.count() == 0)
                .unwrap_or(true)
        }

        fn install(
            &self,
            options: TrayOptions,
            expected: Expected,
            answers: &str,
        ) -> (Result<ExitCode, String>, String) {
            self.host.written.borrow_mut().clear();
            let mut output = Shared(Rc::clone(&self.host.written));
            let release = Release {
                version: VERSION,
                target: Some(target(self.system)),
                expected,
            };
            let result = install_release(
                &self.env,
                self.system,
                &self.host,
                &options,
                &release,
                &mut answers.as_bytes(),
                &mut output,
                true,
            );
            (result, self.written())
        }

        fn uninstall(&self, yes: bool, answers: &str) -> (Result<ExitCode, String>, String) {
            self.host.written.borrow_mut().clear();
            let mut output = Shared(Rc::clone(&self.host.written));
            let result = uninstall(
                &self.env,
                self.system,
                &self.host,
                yes,
                &mut answers.as_bytes(),
                &mut output,
                true,
            );
            (result, self.written())
        }

        fn written(&self) -> String {
            String::from_utf8(self.host.written.borrow().clone()).unwrap()
        }

        /// An update log left by an earlier failed update.
        fn stale_log(&self) -> PathBuf {
            let log = self.support().join(tray::UPDATE_LOG);
            fs::create_dir_all(log.parent().unwrap()).unwrap();
            fs::write(&log, "error: an earlier failure\n").unwrap();
            log
        }

        fn status(&self) -> String {
            status_with(&self.env, self.system, &self.host, VERSION)
                .lines
                .iter()
                .map(|line| line.render() + "\n")
                .collect()
        }
    }

    fn login() -> TrayOptions {
        TrayOptions {
            login: true,
            ..TrayOptions::default()
        }
    }

    const SYSTEMS: [System; 2] = [System::MacOs, System::Windows];

    #[test]
    fn install_downloads_checks_and_installs() {
        for system in SYSTEMS {
            let setup = Setup::new(system);
            let digest = setup.release(VERSION);
            let (result, out) = setup.install(login(), Expected::Baked(digest), "y\ny\n");
            assert_eq!(result, Ok(ExitCode::SUCCESS), "{out}");
            let (app, place, entry) = match system {
                System::MacOs => (
                    "Menu bar app",
                    "Applications/Portico Tray.app",
                    "LaunchAgent io.github.younesamalou.portico.tray",
                ),
                System::Windows => (
                    "Tray app",
                    "AppData/Local/Programs/Portico/portico-tray.exe",
                    r"registry value Portico Tray (HKCU\Software\Microsoft\Windows\CurrentVersion\Run)",
                ),
            };
            // Shown the way this system writes paths: `~\…` on Windows.
            let place = setup.env.show(&setup.env.home.join(place));
            let expected = format!(
                "{app:<19}not installed → Portico Tray 0.8.0\n\
                 {:19}download {} (5 bytes),\n\
                 {:19}checked against the digest built into this portico\n\
                 {:19}install to {place}\n\
                 Install it? [y/N] \
                 Start at login     not set up → {entry}\n\
                 {:19}starts the app with --portico {}\n\
                 Set it up? [y/N] ",
                "",
                setup.name(),
                "",
                "",
                "",
                setup.portico().display(),
            );
            assert!(out.contains(&expected), "{out}\n---\n{expected}");
            assert_eq!(
                setup.host.app_version(&setup.placed()).as_deref(),
                Some(VERSION)
            );
            assert_eq!(
                setup.host.login_entry().unwrap(),
                tray::login_entry(system, &setup.paths().binary, &setup.portico())
            );
            assert!(
                setup.host.calls().contains(&format!(
                    "start {} {} true",
                    setup.paths().binary.display(),
                    setup.portico().display()
                )),
                "{:?}",
                setup.host.calls()
            );
            assert!(out.contains("\nDone.\n"), "{out}");
            assert!(
                out.contains(&format!("Downloading {}…\n", setup.name())),
                "{out}"
            );
            assert!(setup.scratch_removed());
        }
    }

    #[test]
    fn install_refuses_digest_mismatch() {
        for system in SYSTEMS {
            let setup = Setup::new(system);
            let digest = setup.release(VERSION);
            let wrong = "0".repeat(64);
            let (result, out) = setup.install(login(), Expected::Baked(wrong.clone()), "y\ny\n");
            let error = result.unwrap_err();
            assert!(error.contains(&digest), "{error}");
            assert!(error.contains(&wrong), "{error}");
            assert!(!setup.placed().exists(), "{out}");
            assert!(setup.host.login_entry().is_none());
            assert!(!setup.host.calls().iter().any(|c| c.starts_with("start")));
            assert!(
                !setup.host.calls().contains(&"unpack".to_string()),
                "a download that does not match is never unpacked"
            );
            assert!(setup.scratch_removed());
        }
    }

    #[test]
    fn install_without_baked_digest_uses_sums() {
        for system in SYSTEMS {
            let setup = Setup::new(system);
            let digest = setup.release(VERSION);
            setup.serve_sums(&format!(
                "{}  other.zip\n{digest}  {}\n",
                "1".repeat(64),
                setup.name()
            ));
            let (result, out) = setup.install(login(), Expected::Sums, "y\nn\n");
            assert_eq!(result, Ok(ExitCode::SUCCESS), "{out}");
            assert!(
                out.contains("checked against the release's SHA256SUMS"),
                "{out}"
            );
            assert!(
                setup
                    .host
                    .calls()
                    .contains(&format!("download {}", tray::sums_url(VERSION)))
            );
            assert!(setup.placed().exists());
            assert!(out.contains("Skipped starting at login."), "{out}");
            assert!(setup.host.login_entry().is_none());
            assert!(
                setup.host.calls().iter().any(|c| c.ends_with(" false")),
                "{:?}",
                setup.host.calls()
            );

            let other = Setup::new(system);
            other.release(VERSION);
            other.serve_sums("");
            let error = other
                .install(login(), Expected::Sums, "y\ny\n")
                .0
                .unwrap_err();
            assert!(error.contains("SHA256SUMS has no line for"), "{error}");
            assert!(!other.placed().exists());
        }
    }

    #[test]
    fn install_reports_missing_release() {
        for system in SYSTEMS {
            let setup = Setup::new(system);
            let (result, out) = setup.install(login(), Expected::Sums, "y\ny\n");
            let error = result.unwrap_err();
            assert!(error.contains("--from"), "{error}");
            assert!(error.contains("404"), "{error}");
            assert!(!setup.placed().exists(), "{out}");
            assert!(setup.scratch_removed());
        }
    }

    #[test]
    fn install_from_archive_skips_download() {
        for system in SYSTEMS {
            let setup = Setup::new(system);
            let archive = setup.dir.path().join("local-archive");
            fs::write(&archive, "0.9.0-dev").unwrap();
            let options = TrayOptions {
                from: Some(archive.clone()),
                yes: true,
                ..TrayOptions::default()
            };
            let (result, out) = setup.install(options, Expected::Sums, "");
            assert_eq!(result, Ok(ExitCode::SUCCESS), "{out}");
            assert!(
                out.contains("not installed → Portico Tray 0.9.0-dev"),
                "{out}"
            );
            assert!(
                out.contains(&format!("unpack {} (9 bytes),", archive.display())),
                "{out}"
            );
            assert!(out.contains("its digest is not checked (--from)"), "{out}");
            assert!(!out.contains("Start at login"), "{out}");
            assert!(!setup.host.calls().iter().any(|c| c.starts_with("download")));
            assert!(!setup.host.calls().contains(&"set_login_entry".to_string()));
            assert_eq!(
                setup.host.app_version(&setup.placed()).as_deref(),
                Some("0.9.0-dev")
            );
            assert!(archive.exists(), "--from's archive is the user's");
            assert!(setup.scratch_removed());
        }
    }

    #[test]
    fn installed_current_is_not_asked() {
        for system in SYSTEMS {
            let setup = Setup::new(system);
            let digest = setup.release(VERSION);
            let (result, _) = setup.install(login(), Expected::Baked(digest.clone()), "y\ny\n");
            assert_eq!(result, Ok(ExitCode::SUCCESS));
            let before = setup.host.calls().len();
            let (result, out) = setup.install(login(), Expected::Baked(digest), "");
            assert_eq!(result, Ok(ExitCode::SUCCESS), "{out}");
            assert!(out.contains("installed, 0.8.0\n"), "{out}");
            assert!(
                out.contains(&format!(
                    "Start at login     set up (runs {})\n",
                    setup.portico().display()
                )),
                "{out}"
            );
            assert!(out.contains("Nothing to install."), "{out}");
            assert!(!out.contains("[y/N]"), "{out}");
            assert_eq!(setup.host.calls().len(), before, "{:?}", setup.host.calls());
        }
    }

    #[test]
    fn older_app_is_offered_as_update() {
        for system in SYSTEMS {
            let setup = Setup::new(system);
            let paths = setup.paths();
            make_app(system, paths.placed(system).parent().unwrap(), "0.7.0");
            *setup.host.running.borrow_mut() = vec![7];
            let digest = setup.release(VERSION);
            let (result, out) =
                setup.install(TrayOptions::default(), Expected::Baked(digest), "y\n");
            assert_eq!(result, Ok(ExitCode::SUCCESS), "{out}");
            assert!(
                out.contains("installed, 0.7.0 → Portico Tray 0.8.0"),
                "{out}"
            );
            assert!(out.contains("stops the running app first"), "{out}");
            assert!(out.contains("Update it? [y/N] "), "{out}");
            assert_eq!(
                setup.host.app_version(&setup.placed()).as_deref(),
                Some(VERSION)
            );
            let calls = setup.host.calls();
            assert!(calls.contains(&"stop false".to_string()), "{calls:?}");
            assert!(
                calls
                    .iter()
                    .any(|c| c.ends_with(" false") && c.starts_with("start"))
            );
            assert!(setup.scratch_removed());
        }
    }

    #[test]
    fn tray_only_keeps_login_entry_and_restarts() {
        for system in SYSTEMS {
            for with_entry in [true, false] {
                let setup = Setup::new(system);
                let paths = setup.paths();
                make_app(system, paths.placed(system).parent().unwrap(), "0.7.0");
                if with_entry {
                    *setup.host.entry.borrow_mut() = Some("theirs".into());
                }
                let log = setup.stale_log();
                let digest = setup.release(VERSION);
                let options = TrayOptions {
                    yes: true,
                    tray_only: true,
                    ..TrayOptions::default()
                };
                let (result, out) = setup.install(options, Expected::Baked(digest), "");
                assert_eq!(result, Ok(ExitCode::SUCCESS), "{out}");
                let calls = setup.host.calls();
                assert!(calls.contains(&"stop true".to_string()), "{calls:?}");
                assert!(!calls.contains(&"stop false".to_string()), "{calls:?}");
                assert!(
                    !calls.iter().any(|c| c.contains("login_entry")),
                    "{calls:?}"
                );
                assert!(
                    calls.contains(&format!(
                        "start {} {} {with_entry}",
                        paths.binary.display(),
                        setup.portico().display()
                    )),
                    "{calls:?}"
                );
                assert_eq!(
                    setup.host.login_entry(),
                    with_entry.then(|| "theirs".to_string())
                );
                assert_eq!(
                    setup.host.app_version(&setup.placed()).as_deref(),
                    Some(VERSION)
                );
                assert!(!log.exists(), "a successful update removes the log");
            }
        }
    }

    #[test]
    fn install_removes_a_stale_update_log() {
        for system in SYSTEMS {
            let setup = Setup::new(system);
            let paths = setup.paths();
            make_app(system, paths.placed(system).parent().unwrap(), "0.7.0");
            let log = setup.stale_log();
            let digest = setup.release(VERSION);
            let (result, out) =
                setup.install(TrayOptions::default(), Expected::Baked(digest), "y\n");
            assert_eq!(result, Ok(ExitCode::SUCCESS), "{out}");
            assert_eq!(
                setup.host.app_version(&setup.placed()).as_deref(),
                Some(VERSION)
            );
            assert!(!log.exists(), "a successful update removes the log: {out}");
            assert!(setup.scratch_removed());

            // A failed install leaves the log alone.
            let mut failing = Setup::new(system);
            failing.host.fail_unpack = true;
            let log = failing.stale_log();
            let digest = failing.release(VERSION);
            let (result, out) =
                failing.install(TrayOptions::default(), Expected::Baked(digest), "y\n");
            assert!(result.is_err(), "{out}");
            assert!(log.exists());
        }
    }

    #[test]
    fn uninstall_removes_a_stale_update_log() {
        for system in SYSTEMS {
            let setup = Setup::new(system);
            let paths = setup.paths();
            make_app(system, paths.placed(system).parent().unwrap(), "0.7.0");
            let log = setup.stale_log();
            let (result, out) = setup.uninstall(true, "");
            assert_eq!(result, Ok(ExitCode::SUCCESS), "{out}");
            assert!(!setup.placed().exists(), "{out}");
            assert!(!log.exists(), "uninstalling removes the log: {out}");
        }
    }

    #[test]
    fn tray_only_failure_restarts_old_app() {
        for system in SYSTEMS {
            let mut setup = Setup::new(system);
            setup.host.fail_unpack = true;
            let paths = setup.paths();
            make_app(system, paths.placed(system).parent().unwrap(), "0.7.0");
            let digest = setup.release(VERSION);
            let options = TrayOptions {
                yes: true,
                tray_only: true,
                ..TrayOptions::default()
            };
            let (result, out) = setup.install(options, Expected::Baked(digest), "");
            assert_eq!(result, Ok(ExitCode::FAILURE), "{out}");
            let error = "error: tar -xzf failed: truncated archive";
            assert_eq!(out.lines().last(), Some(error), "{out}");
            assert_eq!(
                setup.host.last_line_at_start.borrow().as_deref(),
                Some(error),
                "the error is in the log before the app in place starts"
            );
            assert!(
                out.contains(&format!(
                    "Starting the {} in place.\n{error}\n",
                    system.app_noun()
                )),
                "{out}"
            );
            assert_eq!(
                setup.host.app_version(&setup.placed()).as_deref(),
                Some("0.7.0"),
                "{out}"
            );
            assert!(
                setup.host.calls().contains(&format!(
                    "start {} {} false",
                    paths.binary.display(),
                    setup.portico().display()
                )),
                "{:?}",
                setup.host.calls()
            );
            assert!(setup.scratch_removed());
        }
    }

    #[test]
    fn tray_only_restarts_the_app_when_the_support_dir_cannot_be_made() {
        for system in SYSTEMS {
            let setup = Setup::new(system);
            let paths = setup.paths();
            make_app(system, paths.placed(system).parent().unwrap(), "0.7.0");
            *setup.host.entry.borrow_mut() =
                Some(tray::login_entry(system, &paths.binary, &setup.portico()));
            let support = setup.support();
            fs::create_dir_all(support.parent().unwrap()).unwrap();
            fs::write(&support, "a file where the directory goes").unwrap();
            let digest = setup.release(VERSION);
            let options = TrayOptions {
                yes: true,
                tray_only: true,
                ..TrayOptions::default()
            };
            let (result, out) = setup.install(options, Expected::Baked(digest), "");
            assert_eq!(result, Ok(ExitCode::FAILURE), "{out}");
            let last = out.lines().last().unwrap_or_default();
            assert!(
                last.starts_with("error: cannot create a directory"),
                "{out}"
            );
            assert_eq!(
                setup.host.last_line_at_start.borrow().as_deref(),
                Some(last)
            );
            let calls = setup.host.calls();
            assert!(calls.contains(&"stop true".to_string()), "{calls:?}");
            assert!(
                calls.contains(&format!(
                    "start {} {} true",
                    paths.binary.display(),
                    setup.portico().display()
                )),
                "{calls:?}"
            );
            assert!(out.contains("in place.\n"), "{out}");
            assert_eq!(
                setup.host.app_version(&setup.placed()).as_deref(),
                Some("0.7.0")
            );
        }
    }

    #[test]
    fn from_needs_yes_before_reading_the_archive() {
        for system in SYSTEMS {
            let setup = Setup::new(system);
            let archive = setup.dir.path().join("local-archive");
            fs::write(&archive, VERSION).unwrap();
            let options = TrayOptions {
                from: Some(archive),
                ..TrayOptions::default()
            };
            let mut output = Vec::new();
            let release = Release {
                version: VERSION,
                target: Some(target(system)),
                expected: Expected::Sums,
            };
            let result = install_release(
                &setup.env,
                system,
                &setup.host,
                &options,
                &release,
                &mut "".as_bytes(),
                &mut output,
                false,
            );
            assert_eq!(
                result.unwrap_err(),
                "desktop install requires --yes in non-interactive mode"
            );
            assert!(setup.host.calls().is_empty(), "{:?}", setup.host.calls());
        }
    }

    #[test]
    fn missing_release_reads_only_curls_status() {
        let not_found = missing_release(
            "1.0.0",
            "curl -o /tmp/x https://h/v1.0.0/a failed (exit status: 22): curl: (22) The requested URL returned error: 404".into(),
        );
        assert!(not_found.contains("--from"), "{not_found}");
        let other = missing_release(
            "1.0.0",
            "curl -o /tmp/install-404-abc https://h/v404/a failed (exit status: 6): curl: (6) Could not resolve host: h".into(),
        );
        assert!(other.starts_with("cannot download the tray app"), "{other}");
    }

    #[test]
    fn uninstall_removes_login_stops_and_deletes() {
        for system in SYSTEMS {
            let setup = Setup::new(system);
            let digest = setup.release(VERSION);
            let (result, _) = setup.install(login(), Expected::Baked(digest), "y\ny\n");
            assert_eq!(result, Ok(ExitCode::SUCCESS));
            let (result, out) = setup.uninstall(false, "y\n");
            assert_eq!(result, Ok(ExitCode::SUCCESS), "{out}");
            assert!(
                out.contains("stop     the running app (process 42)"),
                "{out}"
            );
            assert!(
                out.contains(&format!("Remove the {}? [y/N] ", system.app_noun())),
                "{out}"
            );
            assert!(!setup.placed().exists());
            assert!(setup.host.login_entry().is_none());
            let calls = setup.host.calls();
            let stop = calls.iter().rposition(|c| c == "stop false").unwrap();
            let remove = calls
                .iter()
                .position(|c| c == "remove_login_entry")
                .unwrap();
            assert!(stop < remove, "{calls:?}");
            if system == System::Windows {
                assert!(
                    !setup.paths().app.exists(),
                    "the empty directory is removed"
                );
            }

            let (result, out) = setup.uninstall(true, "");
            assert_eq!(result, Ok(ExitCode::SUCCESS), "{out}");
            assert!(out.contains("is not installed"), "{out}");
        }
    }

    #[test]
    fn stopping_at_a_question_changes_nothing() {
        for system in SYSTEMS {
            let setup = Setup::new(system);
            let digest = setup.release(VERSION);
            let (result, out) = setup.install(login(), Expected::Baked(digest), "y\n");
            assert_eq!(
                result.unwrap_err(),
                "no answer; nothing was changed",
                "{out}"
            );
            assert!(!setup.placed().exists());
            assert!(setup.host.login_entry().is_none());
            assert!(
                !setup
                    .host
                    .calls()
                    .iter()
                    .any(|c| c.starts_with("start") || c.starts_with("stop")),
                "{:?}",
                setup.host.calls()
            );
            assert!(setup.scratch_removed());

            let (result, _) = setup.uninstall(false, "n\n");
            assert_eq!(result, Ok(ExitCode::SUCCESS));
        }
    }

    #[test]
    fn status_lines_report_app_login_and_running() {
        for system in SYSTEMS {
            let setup = Setup::new(system);
            assert_eq!(
                setup.status(),
                format!(
                    "{:<12} not installed\nAt login     not set up\nRunning      no\n",
                    system.app_name()
                )
            );
            let paths = setup.paths();
            make_app(system, paths.placed(system).parent().unwrap(), "0.7.0");
            let gone = setup.dir.path().join("gone/portico");
            *setup.host.entry.borrow_mut() = Some(tray::login_entry(system, &paths.binary, &gone));
            *setup.host.running.borrow_mut() = vec![42];
            let status = setup.status();
            assert!(
                status.contains(&format!(
                    "installed in {}, 0.7.0; this portico is 0.8.0: out of date, run portico desktop install",
                    setup.env.show(&setup.placed())
                )),
                "{status}"
            );
            assert!(
                status.contains(&format!(
                    "points at {}, which no longer exists",
                    setup.env.show(&gone)
                )),
                "{status}"
            );
            assert!(status.contains("Running      yes (process 42)"), "{status}");
            assert!(status_with(&setup.env, system, &setup.host, VERSION).installed);

            *setup.host.entry.borrow_mut() =
                Some(tray::login_entry(system, &paths.binary, &setup.portico()));
            make_app(system, paths.placed(system).parent().unwrap(), VERSION);
            let status = setup.status();
            assert!(status.contains("0.8.0 (matches this portico)"), "{status}");
            assert!(
                status.contains(&format!("runs {}", setup.portico().display())),
                "{status}"
            );
        }
    }
}
