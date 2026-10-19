//! Saved tunnels as the CLI and the TUI use them: their reports, their changes, and starting and
//! stopping them.

use std::{
    cell::OnceCell,
    fs::{File, OpenOptions},
    io,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

use fs2::FileExt;
use thiserror::Error;

use super::{App, AppError};
use crate::{
    explain::{Explanation, Operation},
    managed::{acquire_lock, open_lock},
    openssh::{OpenSshError, kill_process_tree},
    probe::{Failure, diagnose},
    tunnel::{
        Forward, Tunnel, TunnelReport, TunnelStatus, TunnelWarning, TunnelsFile, addresses_overlap,
        check_args, collide, collisions, listeners_in_effective_config,
        runtime::{
            Bind, Identity, Logged, Record, RunningRecord, current_uid, find_in, log_reason,
            log_tail, now, stop_process, try_bind,
        },
        start_args,
    },
};

/// Why a tunnel operation was refused before anything changed.
#[derive(Debug, Error)]
pub enum TunnelRefusal {
    #[error("no tunnel named {0}")]
    Unknown(String),
    #[error("a tunnel named {0} already exists")]
    Exists(String),
    #[error("tunnel {0} is running; stop it first")]
    Running(String),
    #[error("tunnel {name}: host {host} is not in the inventory; edit the tunnel")]
    UnknownHost { name: String, host: String },
    #[error("OpenSSH rejected tunnel {name}: {stderr}")]
    Rejected { name: String, stderr: String },
    #[error("port {port} on {address} is in use{}", by.as_ref().map_or(String::new(), |by| format!(" by tunnel {by}")))]
    PortInUse {
        address: String,
        port: u16,
        by: Option<String>,
    },
    #[error("listening on port {port} on {address} needs privileges")]
    Denied { address: String, port: u16 },
    #[error("tunnel {0} is running on the same port; stop it first")]
    Collides(String),
    #[error("cannot keep tunnel state in {path}: {source}", path = .path.display())]
    Runtime {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// What an edit changes; `None` keeps the field.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TunnelChanges {
    pub name: Option<String>,
    pub host: Option<String>,
    pub forward: Option<Forward>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartOutcome {
    Started {
        pid: u32,
    },
    AlreadyRunning {
        pid: u32,
    },
    /// ssh did not establish the tunnel. `reason` is one line; `explanation`, when Portico
    /// recognises the failure, says what to do.
    Failed {
        reason: String,
        explanation: Option<Explanation>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopOutcome {
    Stopped,
    /// Nothing was running; a failure record, if any, was cleared.
    NotRunning,
}

fn runtime_error(path: PathBuf) -> impl FnOnce(io::Error) -> TunnelRefusal {
    move |source| TunnelRefusal::Runtime { path, source }
}

/// `address:port`, with an IPv6 address in brackets.
fn socket_text(address: &str, port: u16) -> String {
    if address.contains(':') {
        format!("[{address}]:{port}")
    } else {
        format!("{address}:{port}")
    }
}

impl App {
    /// Every saved tunnel with its status and warnings, in name order.
    pub fn tunnels(&self) -> Result<Vec<TunnelReport>, AppError> {
        let file = self.tunnels.load()?;
        let known = self.hosts().ok().map(|inventory| {
            inventory
                .hosts
                .into_iter()
                .map(|host| host.alias)
                .collect::<Vec<_>>()
        });
        let scan = OnceCell::new();
        let running: Vec<(Tunnel, TunnelStatus)> = file
            .tunnels()
            .iter()
            .map(|tunnel| {
                (
                    tunnel.clone(),
                    self.run_state_in(&tunnel.name, &scan, false),
                )
            })
            .collect();
        Ok(running
            .iter()
            .map(|(tunnel, state)| {
                let status = if state.is_running() {
                    state.clone()
                } else {
                    self.port_status(tunnel, &running)
                        .unwrap_or_else(|| state.clone())
                };
                let mut warnings = Vec::new();
                if let Some(known) = &known
                    && !known
                        .iter()
                        .any(|alias| alias.eq_ignore_ascii_case(&tunnel.host))
                {
                    warnings.push(TunnelWarning::UnknownHost {
                        host: tunnel.host.clone(),
                    });
                }
                warnings.extend(
                    collisions(file.tunnels(), tunnel)
                        .into_iter()
                        .map(|other| TunnelWarning::CollidesWith { tunnel: other }),
                );
                TunnelReport {
                    tunnel: tunnel.clone(),
                    status,
                    warnings,
                }
            })
            .collect())
    }

    /// One saved tunnel's report.
    pub fn tunnel(&self, name: &str) -> Result<TunnelReport, AppError> {
        self.tunnels()?
            .into_iter()
            .find(|report| report.tunnel.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| TunnelRefusal::Unknown(name.into()).into())
    }

    /// The last lines of a tunnel's log, for showing a failure in full.
    pub fn tunnel_log(&self, name: &str, lines: usize) -> Vec<String> {
        log_tail(&self.runtime.log(name), lines)
    }

    /// The command line a start of `tunnel` runs, for showing it.
    pub fn tunnel_command(&self, tunnel: &Tunnel) -> Vec<String> {
        std::iter::once(self.openssh.ssh_program().as_os_str().to_owned())
            .chain(start_args(
                &self.runtime.log(&tunnel.name),
                &tunnel.host,
                &tunnel.forward,
            ))
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    pub fn add_tunnel(&self, tunnel: Tunnel) -> Result<(), AppError> {
        self.tunnels.transact(|file| {
            if file.find(&tunnel.name).is_some() {
                return Err(TunnelRefusal::Exists(tunnel.name.clone()).into());
            }
            self.check_tunnel(&tunnel)?;
            file.insert(tunnel.clone())
                .map_err(|_| TunnelRefusal::Exists(tunnel.name).into())
        })
    }

    pub fn edit_tunnel(&self, name: &str, changes: TunnelChanges) -> Result<(), AppError> {
        if self.run_state(&self.saved_name(name)?).is_running() {
            return Err(TunnelRefusal::Running(self.saved_name(name)?).into());
        }
        let renamed = self.tunnels.transact(|file: &mut TunnelsFile| {
            let current = file
                .find(name)
                .cloned()
                .ok_or_else(|| TunnelRefusal::Unknown(name.into()))?;
            let checked = changes.host.is_some() || changes.forward.is_some();
            let edited = Tunnel::new(
                changes.name.unwrap_or_else(|| current.name.clone()),
                changes.host.unwrap_or_else(|| current.host.clone()),
                changes.forward.unwrap_or_else(|| current.forward.clone()),
            )?;
            if checked {
                self.check_tunnel(&edited)?;
            }
            file.replace(&current.name, edited.clone())
                .map_err(|_| AppError::from(TunnelRefusal::Exists(edited.name.clone())))?;
            Ok::<_, AppError>((current.name != edited.name).then_some(current.name))
        })?;
        if let Some(old) = renamed {
            self.forget_run(&old);
        }
        Ok(())
    }

    pub fn remove_tunnel(&self, name: &str) -> Result<(), AppError> {
        let saved = self.saved_name(name)?;
        if self.run_state(&saved).is_running() {
            return Err(TunnelRefusal::Running(saved).into());
        }
        self.tunnels.transact(|file| {
            file.remove(name)
                .map_err(|_| AppError::from(TunnelRefusal::Unknown(name.into())))
        })?;
        self.forget_run(&saved);
        Ok(())
    }

    /// Starts a saved tunnel and waits until ssh has established it or failed. `cancelled`
    /// kills ssh while it is still connecting.
    pub fn start_tunnel(
        &self,
        name: &str,
        cancelled: Option<&AtomicBool>,
        in_tui: bool,
    ) -> Result<StartOutcome, AppError> {
        let file = self.tunnels.load()?;
        let tunnel = file
            .find(name)
            .cloned()
            .ok_or_else(|| TunnelRefusal::Unknown(name.into()))?;
        let dir = self.runtime.path().to_path_buf();
        self.runtime.ensure().map_err(runtime_error(dir))?;
        let lock_path = self.runtime.lock();
        let lock = open_lock(&lock_path).map_err(runtime_error(lock_path.clone()))?;
        acquire_lock(&lock, cancelled).map_err(runtime_error(lock_path))?;

        if let TunnelStatus::Running { pid, .. } = self.run_state_locked(&tunnel.name) {
            return Ok(StartOutcome::AlreadyRunning { pid });
        }
        let effective = self.check_tunnel(&tunnel)?;
        let running: Vec<Tunnel> = file
            .tunnels()
            .iter()
            .filter(|other| self.run_state_locked(&other.name).is_running())
            .cloned()
            .collect();
        if let Some(other) = running.iter().find(|other| collide(&tunnel, other)) {
            return Err(TunnelRefusal::Collides(other.name.clone()).into());
        }
        let mut listeners = listeners_in_effective_config(&effective);
        for address in tunnel.forward.listen_addresses() {
            let listener = (address, tunnel.forward.bind_port);
            if !listeners.contains(&listener) {
                listeners.push(listener);
            }
        }
        for (address, port) in listeners {
            match try_bind(&address, port) {
                Bind::Free => {}
                Bind::InUse => {
                    // Only a tunnel listening on this very address can be the holder.
                    let by = running
                        .iter()
                        .find(|other| {
                            other.forward.bind_port == port
                                && other
                                    .forward
                                    .listen_addresses()
                                    .iter()
                                    .any(|listen| addresses_overlap(listen, &address))
                        })
                        .map(|other| other.name.clone());
                    return Err(TunnelRefusal::PortInUse { address, port, by }.into());
                }
                Bind::Denied => return Err(TunnelRefusal::Denied { address, port }.into()),
            }
        }
        self.launch(&tunnel, cancelled, in_tui)
    }

    /// Runs the tunnel's ssh, waits for it to background itself, and records the result.
    fn launch(
        &self,
        tunnel: &Tunnel,
        cancelled: Option<&AtomicBool>,
        in_tui: bool,
    ) -> Result<StartOutcome, AppError> {
        let (record_path, log) = (
            self.runtime.record(&tunnel.name),
            self.runtime.log(&tunnel.name),
        );
        Record::remove(&record_path).map_err(runtime_error(record_path.clone()))?;
        let create = |path: &PathBuf| {
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)
                .map_err(runtime_error(path.clone()))
        };
        create(&log)?;
        // stderr goes to a file, not a pipe: a ProxyCommand child (`ssh -W`, `nc`) inherits it and
        // lives as long as the tunnel, so a pipe would never reach end of file.
        let stderr_path = self.runtime.stderr(&tunnel.name);
        let stderr_file = create(&stderr_path)?;
        let mut child = Command::new(self.openssh.ssh_program())
            .args(start_args(&log, &tunnel.host, &tunnel.forward))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr_file)
            .spawn()
            .map_err(|source| OpenSshError::Io {
                program: self.openssh.ssh_program().to_path_buf(),
                source,
            })?;
        // The background ssh is forked from this one, so it cannot have started earlier.
        let since = self.proc.stat(child.id()).map(|stat| stat.start);
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|source| OpenSshError::Io {
                program: self.openssh.ssh_program().to_path_buf(),
                source,
            })? {
                break status;
            }
            if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Relaxed)) {
                // A ProxyCommand child would outlive ssh, so the whole tree goes.
                if kill_process_tree(child.id()).is_err() {
                    let _ = child.kill();
                }
                let _ = child.wait();
                let _ = Record::remove(&stderr_path);
                return Err(OpenSshError::Cancelled.into());
            }
            thread::sleep(Duration::from_millis(20));
        };
        let stderr = log_tail(&stderr_path, 200).join("\n");
        // The proxy child keeps writing to its open file; the name is no longer needed.
        let _ = Record::remove(&stderr_path);
        let uid = current_uid();
        if status.success()
            && let Some((pid, start)) = self.proc.find_tunnel(&log, uid, since)
        {
            let record = Record::Running(RunningRecord {
                pid,
                start,
                boot: self.proc.boot_id().unwrap_or_default(),
                started: now(),
                host: tunnel.host.clone(),
                forward: format!("{} {}", tunnel.forward.kind.flag(), tunnel.forward.spec()),
            });
            record
                .write(&record_path)
                .map_err(runtime_error(record_path))?;
            return Ok(StartOutcome::Started { pid });
        }
        let mut text = log_tail(&log, 200).join("\n");
        text.push('\n');
        text.push_str(&stderr);
        let (reason, explanation) = if status.success() {
            (
                log_reason(&log).unwrap_or_else(|| "ssh exited right after starting".into()),
                None,
            )
        } else {
            self.explain_tunnel_failure(&tunnel.host, &text, in_tui)
        };
        Record::Failed {
            reason: reason.clone(),
            exit: status.code(),
            finished: now(),
        }
        .write(&record_path)
        .map_err(runtime_error(record_path))?;
        Ok(StartOutcome::Failed {
            reason,
            explanation,
        })
    }

    /// Stops a tunnel: its process when one is running, and its record and log in any case.
    pub fn stop_tunnel(&self, name: &str) -> Result<StopOutcome, AppError> {
        let name = self.saved_name(name).or_else(|error| {
            // A tunnel removed from the file while it ran can still be stopped.
            self.runtime
                .recorded_names()
                .into_iter()
                .find(|recorded| recorded.eq_ignore_ascii_case(name))
                .ok_or(error)
        })?;
        let record_path = self.runtime.record(&name);
        let log = self.runtime.log(&name);
        let stopped = match self.run_state(&name) {
            TunnelStatus::Running { .. } => match Record::read(&record_path) {
                Some(Record::Running(record)) => {
                    let uid = current_uid();
                    stop_process(record.pid, || {
                        self.proc.identify(&record, &log, uid) == Identity::Alive
                    })
                    .map_err(runtime_error(record_path.clone()))?
                }
                _ => false,
            },
            _ => false,
        };
        self.forget_run(&name);
        Ok(if stopped {
            StopOutcome::Stopped
        } else {
            StopOutcome::NotRunning
        })
    }

    /// The names of the tunnels that run now, including any no longer saved.
    pub fn running_tunnels(&self) -> Result<Vec<String>, AppError> {
        // An unreadable tunnels file must not keep running tunnels from being stopped.
        let mut names: Vec<String> = self
            .tunnels
            .load()
            .map(|file| {
                file.tunnels()
                    .iter()
                    .map(|tunnel| tunnel.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        for recorded in self.runtime.recorded_names() {
            if !names
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&recorded))
            {
                names.push(recorded);
            }
        }
        let scan = OnceCell::new();
        Ok(names
            .into_iter()
            .filter(|name| self.run_state_in(name, &scan, false).is_running())
            .collect())
    }

    /// The saved tunnel's name as the file spells it.
    fn saved_name(&self, name: &str) -> Result<String, AppError> {
        Ok(self
            .tunnels
            .load()?
            .find(name)
            .map(|tunnel| tunnel.name.clone())
            .ok_or_else(|| TunnelRefusal::Unknown(name.into()))?)
    }

    /// Removes the record and log of a tunnel that is not running.
    fn forget_run(&self, name: &str) {
        let _ = Record::remove(&self.runtime.record(name));
        let _ = Record::remove(&self.runtime.log(name));
    }

    /// Whether the tunnel runs, failed or is stopped, from its record and `/proc`, without
    /// looking at ports. A record from another boot is deleted, and a running tunnel without a
    /// record gets one again.
    fn run_state(&self, name: &str) -> TunnelStatus {
        self.run_state_in(name, &OnceCell::new(), false)
    }

    /// `run_state` for a caller that holds `start.lock`, as a start does.
    fn run_state_locked(&self, name: &str) -> TunnelStatus {
        self.run_state_in(name, &OnceCell::new(), true)
    }

    /// `run_state`, with the `/proc` scan for tunnels without a record read at most once into
    /// `scan` across calls. `holds_lock` says the caller holds `start.lock`.
    fn run_state_in(
        &self,
        name: &str,
        scan: &OnceCell<Vec<Logged>>,
        holds_lock: bool,
    ) -> TunnelStatus {
        let (record_path, log) = (self.runtime.record(name), self.runtime.log(name));
        match Record::read(&record_path) {
            Some(Record::Running(record)) => {
                match self.proc.identify(&record, &log, current_uid()) {
                    Identity::Alive => {
                        return TunnelStatus::Running {
                            pid: record.pid,
                            started: record.started,
                        };
                    }
                    // The record may be older than the process (a status read raced a start):
                    // look for the tunnel before calling it failed.
                    Identity::Gone => {
                        return self.rediscover(name, scan, holds_lock).unwrap_or_else(|| {
                            TunnelStatus::Failed {
                                reason: log_reason(&log).unwrap_or_else(|| "exited".into()),
                                finished: None,
                            }
                        });
                    }
                    Identity::Stale => {
                        let _ = Record::remove(&record_path);
                    }
                }
            }
            Some(Record::Failed {
                reason, finished, ..
            }) => {
                return TunnelStatus::Failed {
                    reason,
                    finished: Some(finished),
                };
            }
            None => {}
        }
        self.rediscover(name, scan, holds_lock)
            .unwrap_or(TunnelStatus::Stopped)
    }

    /// Finds a backgrounded tunnel process without a valid record and writes its record again.
    /// Nothing is rebuilt while a start holds `start.lock` (unless the caller is that start):
    /// the start writes the record itself.
    fn rediscover(
        &self,
        name: &str,
        scan: &OnceCell<Vec<Logged>>,
        holds_lock: bool,
    ) -> Option<TunnelStatus> {
        let log = self.runtime.log(name);
        let (pid, start) = find_in(
            scan.get_or_init(|| self.proc.scan(current_uid())),
            &log,
            None,
        )?;
        let _lock = if holds_lock {
            None
        } else {
            self.try_start_lock()?
        };
        let record = self.proc.rebuild_record(pid, start)?;
        let status = TunnelStatus::Running {
            pid: record.pid,
            started: record.started,
        };
        let _ = Record::Running(record).write(&self.runtime.record(name));
        Some(status)
    }

    /// `start.lock` taken without waiting: `None` while a start holds it, `Some(None)` when there
    /// is no runtime directory yet (so no start can be running).
    fn try_start_lock(&self) -> Option<Option<File>> {
        if !self.runtime.path().is_dir() {
            return Some(None);
        }
        let lock = open_lock(&self.runtime.lock()).ok()?;
        FileExt::try_lock_exclusive(&lock).ok()?;
        Some(Some(lock))
    }

    /// `PortInUse` when one of the tunnel's local addresses cannot be bound now.
    fn port_status(
        &self,
        tunnel: &Tunnel,
        saved: &[(Tunnel, TunnelStatus)],
    ) -> Option<TunnelStatus> {
        let port = tunnel.forward.bind_port;
        let address = tunnel
            .forward
            .listen_addresses()
            .into_iter()
            .find(|address| try_bind(address, port) == Bind::InUse)?;
        let by = saved
            .iter()
            .find(|(other, status)| status.is_running() && collide(tunnel, other))
            .map(|(other, _)| other.name.clone());
        Some(TunnelStatus::PortInUse {
            address: socket_text(&address, port),
            by,
        })
    }

    /// Checks that the tunnel's host is known and that OpenSSH accepts the forward through it;
    /// returns OpenSSH's effective configuration for that ssh.
    fn check_tunnel(&self, tunnel: &Tunnel) -> Result<String, AppError> {
        let known = self
            .hosts()?
            .hosts
            .into_iter()
            .any(|host| host.alias.eq_ignore_ascii_case(&tunnel.host));
        if !known {
            return Err(TunnelRefusal::UnknownHost {
                name: tunnel.name.clone(),
                host: tunnel.host.clone(),
            }
            .into());
        }
        let output = Command::new(self.openssh.ssh_program())
            .args(check_args(&tunnel.host, &tunnel.forward))
            .stdin(Stdio::null())
            .output()
            .map_err(|source| OpenSshError::Io {
                program: self.openssh.ssh_program().to_path_buf(),
                source,
            })?;
        if !output.status.success() {
            return Err(TunnelRefusal::Rejected {
                name: tunnel.name.clone(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            }
            .into());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// One line on why a start failed, and an explanation when the cause is recognised.
    fn explain_tunnel_failure(
        &self,
        alias: &str,
        text: &str,
        in_tui: bool,
    ) -> (String, Option<Explanation>) {
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("debug"))
            .collect();
        let last = lines.last().map_or("ssh failed", |line| *line).to_owned();
        let simple = |headline: &str, next: &str| {
            let explanation = Explanation {
                headline: headline.into(),
                facts: Default::default(),
                next: vec![next.into()],
                raw: lines.iter().map(|line| (*line).to_owned()).collect(),
                always_raw: false,
            };
            (headline.to_owned(), Some(explanation))
        };
        if let Some(line) = lines
            .iter()
            .find(|line| line.contains("remote port forwarding failed"))
        {
            let port = line
                .rsplit(' ')
                .next()
                .filter(|port| port.parse::<u16>().is_ok())
                .map_or(String::new(), |port| format!(" on port {port}"));
            return simple(
                &format!("the server refused the remote forward{port}"),
                "the port is in use on the server, or its sshd does not allow remote forwards",
            );
        }
        if lines
            .iter()
            .any(|line| line.contains("Address already in use"))
        {
            return simple(
                "a local port is already in use",
                "stop whatever listens on it, or choose another port",
            );
        }
        if lines.iter().any(|line| {
            line.contains("bind") && line.contains("Permission denied")
                || line.contains("Privileged ports can only be forwarded by root")
        }) {
            return simple("the port needs privileges", "choose a port above 1023");
        }
        let Ok(resolved) = self.openssh.resolve_with(None, alias, None) else {
            return (last, None);
        };
        let diagnosis = diagnose(text, false, resolved.route());
        match &diagnosis.failure {
            Failure::Other(_) => (last, None),
            Failure::HostKeyUnknown => {
                let next = if in_tui {
                    "trust the host key first: press t on the host to test it".to_owned()
                } else {
                    format!("trust the host key first with `portico test {alias}`")
                };
                let (headline, explanation) = simple("the host key is not trusted", &next);
                (headline, explanation)
            }
            failure => {
                let reason = failure.to_string();
                let mut explanation = self.explain_failure(
                    diagnosis.clone(),
                    alias,
                    &resolved,
                    Operation::Connect,
                    in_tui,
                );
                if matches!(failure, Failure::AuthRefused { .. }) {
                    explanation.next.push(
                        "tunnels cannot ask for a password or passphrase: load the key into your agent (ssh-add)".into(),
                    );
                }
                (reason, Some(explanation))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{
        managed::{ConfigPaths, ManagedStore},
        openssh::OpenSsh,
        tunnel::runtime::{Proc, RuntimeDir},
    };

    /// An app whose runtime directory and `/proc` (with this boot's ID) are temporary.
    fn app(dir: &std::path::Path) -> App {
        let proc = dir.join("proc");
        fs::create_dir_all(proc.join("sys/kernel/random")).unwrap();
        fs::write(proc.join("sys/kernel/random/boot_id"), "this-boot\n").unwrap();
        fs::write(proc.join("stat"), "btime 1000\n").unwrap();
        let mut app = App::new(
            ManagedStore::new(ConfigPaths::from_home(dir.join("home"))),
            OpenSsh::new("ssh".into(), "terminal".into()),
        )
        .with_runtime_dir(RuntimeDir::at(dir.join("run")));
        app.proc = Proc::at(proc);
        app
    }

    #[test]
    fn cancelling_a_start_kills_ssh_and_its_proxy_child() {
        use std::{sync::Arc, time::Instant};

        use crate::{host::Host, test_executable::write_executable, tunnel::Kind};

        let dir = tempfile::tempdir().unwrap();
        let child_pid = dir.path().join("child-pid");
        let ssh = dir.path().join("ssh");
        // Answers `-G`; a start leaves a child (like a ProxyCommand) and keeps connecting.
        write_executable(
            &ssh,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = -G ]; then printf 'hostname x\\nport 22\\n'; exit 0; fi\n\
                 sleep 60 &\necho $! > '{}'\nsleep 60\n",
                child_pid.display()
            ),
        );
        fs::create_dir_all(dir.path().join("home/.ssh")).unwrap();
        let app = App::new(
            ManagedStore::new(ConfigPaths::from_home(dir.path().join("home"))),
            OpenSsh::new(ssh, "terminal".into()),
        )
        .with_runtime_dir(RuntimeDir::at(dir.path().join("run")));
        app.initialize(true).unwrap();
        app.add(Host::new("prod", "x", None, None, None, None).unwrap())
            .unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let forward = Forward::parse(Kind::Dynamic, &format!("127.0.0.1:{port}")).unwrap();
        app.add_tunnel(Tunnel::new("db", "prod", forward).unwrap())
            .unwrap();

        let cancelled = Arc::new(AtomicBool::new(false));
        let canceller = {
            let (cancelled, child_pid) = (Arc::clone(&cancelled), child_pid.clone());
            thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !child_pid.exists() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                thread::sleep(Duration::from_millis(50));
                cancelled.store(true, Ordering::Relaxed);
            })
        };
        let result = app.start_tunnel("db", Some(&cancelled), true);
        canceller.join().unwrap();
        assert!(
            matches!(result, Err(AppError::OpenSsh(OpenSshError::Cancelled))),
            "{result:?}"
        );
        let pid = fs::read_to_string(&child_pid).unwrap().trim().to_owned();
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|stat| !stat.contains(") Z"))
        {
            assert!(
                Instant::now() < deadline,
                "the proxy-like child {pid} survived"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_record_from_another_boot_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path());
        app.runtime.ensure().unwrap();
        let record = app.runtime.record("db");
        Record::Running(RunningRecord {
            pid: 41,
            start: 500,
            boot: "another-boot".into(),
            started: 0,
            host: "prod".into(),
            forward: "-D 1080".into(),
        })
        .write(&record)
        .unwrap();
        assert_eq!(app.run_state("db"), TunnelStatus::Stopped);
        assert!(!record.exists(), "the stale record was kept");
    }

    #[test]
    fn nothing_is_rediscovered_while_a_start_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path());
        app.runtime.ensure().unwrap();
        // A backgrounded tunnel process without a record.
        let process = dir.path().join("proc/41");
        fs::create_dir_all(&process).unwrap();
        let filler = (7..22).map(|_| "0").collect::<Vec<_>>().join(" ");
        fs::write(
            process.join("stat"),
            format!("41 (ssh) S 1 41 41 {filler} 500 0\n"),
        )
        .unwrap();
        fs::write(
            process.join("status"),
            format!("Uid:\t{0}\t{0}\t{0}\t{0}\n", current_uid()),
        )
        .unwrap();
        let log = app.runtime.log("db");
        fs::write(
            process.join("cmdline"),
            format!("ssh\0-f\0-E\0{}\0-D\01080\0--\0prod\0", log.display()),
        )
        .unwrap();

        let lock = open_lock(&app.runtime.lock()).unwrap();
        FileExt::lock_exclusive(&lock).unwrap();
        assert_eq!(app.run_state("db"), TunnelStatus::Stopped);
        assert!(!app.runtime.record("db").exists());
        drop(lock);

        // A process another test forks just now may briefly hold a copy of the lock until it
        // executes, so the free lock is retried a little.
        let mut status = app.run_state("db");
        for _ in 0..100 {
            if status.is_running() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
            status = app.run_state("db");
        }
        assert!(
            matches!(status, TunnelStatus::Running { pid: 41, .. }),
            "{status:?}"
        );
        assert!(app.runtime.record("db").exists());
    }
}
