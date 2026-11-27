//! Saved tunnels and temporary forwards as the CLI and the TUI use them: their reports, their
//! changes, and starting and stopping them.

use std::{
    cell::OnceCell,
    fs::{File, OpenOptions},
    io::{self, Read},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use fs2::FileExt;
use thiserror::Error;

use super::{App, AppError};
use crate::{
    backend::BackendId,
    domain::explain::{Explanation, Operation},
    domain::openssh::OpenSshError,
    domain::probe::{Failure, diagnose},
    domain::tunnel::{
        ContainerTarget, Destination, Forward, Kind, Tunnel, TunnelReport, TunnelStatus,
        TunnelWarning, TunnelsFile, addresses_overlap, check_args, collide, collisions,
        is_temporary, listeners_in_effective_config,
        runtime::{
            Bind, Identity, READY_BOUND, Record, RunningRecord, authenticated, find_in, listening,
            log_reason, log_tail, nothing_answers, now, ready, try_bind, up_at_bound,
        },
        start_args,
    },
    platform::fs::{acquire_lock, open_lock, private},
    platform::process::{
        Logged, SSH_BACKGROUNDS, current_uid, detach, kill_process_tree, stop_process,
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
    #[error("{0} is not a temporary forward")]
    NotTemporary(String),
    /// A container whose port is neither published nor at an address the host can use.
    #[error("{label} has no address the host can reach; publish the port")]
    Unreachable { label: String },
    /// A container tunnel's container is not among the running containers of its host.
    #[error("{reference} is not running on {host} ({})", backend.as_str())]
    NotRunning {
        reference: String,
        host: String,
        backend: BackendId,
    },
    /// A container tunnel's reference names several running containers of its host.
    #[error("{reference} matches several containers on {host}: {}", candidates.join(", "))]
    AmbiguousContainer {
        reference: String,
        host: String,
        candidates: Vec<String>,
    },
    /// A container tunnel's container has neither the port published nor an address its host
    /// routes to.
    #[error(
        "{reference} on {host} has no address the host can route to; publish the port, or attach it to a network {host} routes"
    )]
    Unroutable { reference: String, host: String },
    /// A Proxmox CT whose address could not be read: it has no ports to publish.
    #[error("could not read the address of CT {id} on {host}")]
    NoCtAddress { id: String, host: String },
    /// A container on this machine is reached at its own addresses, without a forward.
    #[error("a container on this machine needs no forward; use its address directly")]
    LocalTarget,
    /// Asking where a container can be reached failed, before any forward started.
    #[error("could not find where {label} can be reached: {reason}")]
    ReachFailed { label: String, reason: String },
    /// A quick forward needs a remote port.
    #[error("no remote port given; type the port to forward")]
    NoPort,
    /// A quick forward's local port, when given, is a port.
    #[error("local port 0 is not a port; give one from 1 through 65535")]
    NoLocalPort,
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
    /// `warning`, for a container forward, says that the container refused the test
    /// connection made right after the start.
    Started {
        pid: u32,
        warning: Option<String>,
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

/// A temporary forward as its record describes it.
struct Temporary {
    name: String,
    /// `None` for a failed record without its host and forward, which cannot be started again.
    tunnel: Option<Tunnel>,
    /// The record's `reaches` line.
    reaches: Option<String>,
    status: TunnelStatus,
    /// Whether the log says the container it reaches refused a connection.
    refused: bool,
    /// When it started, or for a failed record when it failed, in Unix seconds.
    since: u64,
}

impl Temporary {
    fn report(self) -> TunnelReport {
        let reaches = self
            .reaches
            .or_else(|| {
                self.tunnel
                    .as_ref()
                    .and_then(|tunnel| tunnel.forward.destination_text())
            })
            .unwrap_or_else(|| "unknown".into());
        let warnings = self
            .tunnel
            .as_ref()
            .and_then(|tunnel| answer_warning(tunnel, &self.status, self.refused))
            .into_iter()
            .collect();
        let tunnel = self.tunnel.unwrap_or_else(|| Tunnel {
            name: self.name,
            host: String::new(),
            forward: Forward {
                kind: Kind::Dynamic,
                bind_address: None,
                bind_port: 0,
                destination: None,
            },
            container: None,
        });
        TunnelReport {
            tunnel,
            status: self.status,
            warnings,
            temporary: true,
            reaches,
        }
    }
}

/// The tunnel a record's `host` and `forward` lines (`-L 127.0.0.1:8080:localhost:80`)
/// describe, with the container of its `container` line.
fn recorded_tunnel(
    name: &str,
    host: &str,
    forward: &str,
    container: Option<&str>,
) -> Option<Tunnel> {
    let (flag, spec) = forward.split_once(' ')?;
    Some(Tunnel {
        name: name.to_owned(),
        host: host.to_owned(),
        forward: Forward::parse(Kind::from_flag(flag)?, spec).ok()?,
        container: container.and_then(ContainerTarget::from_record_text),
    })
}

/// The "nothing answers" warning of a running container tunnel whose log has `refused` a
/// connection.
fn answer_warning(tunnel: &Tunnel, status: &TunnelStatus, refused: bool) -> Option<TunnelWarning> {
    let target = tunnel.container.as_ref()?;
    (refused && status.is_running()).then(|| TunnelWarning::NothingAnswers {
        port: target.port,
        reference: target.reference.clone(),
    })
}

/// A temporary forward's `container` line, from its container target.
fn container_text(tunnel: &Tunnel) -> Option<String> {
    tunnel
        .container
        .as_ref()
        .filter(|_| is_temporary(&tunnel.name))
        .map(ContainerTarget::record_text)
}

/// The addresses a test connection to local listeners on `addresses`, port `port`, goes to: a
/// wildcard address is reached at the loopback of its family, and a host name at what it
/// resolves to.
fn local_sockets(addresses: &[String], port: u16) -> Vec<SocketAddr> {
    let mut sockets = Vec::new();
    for address in addresses {
        let resolved: Vec<SocketAddr> = match address.parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, port)],
            Err(_) => (address.as_str(), port)
                .to_socket_addrs()
                .map(Iterator::collect)
                .unwrap_or_default(),
        };
        for socket in resolved {
            let ip = match socket.ip() {
                IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
                ip => ip,
            };
            let socket = SocketAddr::new(ip, port);
            if !sockets.contains(&socket) {
                sockets.push(socket);
            }
        }
    }
    sockets
}

/// Whether a container forward's far side refuses a connection: connects to the first of
/// `addresses` that accepts (each tried in turn, again until [`ANSWER_WAIT`] has passed, as a
/// listener may come up on one address before another), then waits, within the same bound,
/// for `log` to say ssh's channel was refused. A byte from the far side means something
/// answers; a connection ssh closed without that line in the log a short while later means
/// nothing is known. The connection is closed on return.
fn refused_after_connect(addresses: &[SocketAddr], log: &Path) -> bool {
    let started = Instant::now();
    let left = || ANSWER_WAIT.saturating_sub(started.elapsed());
    let mut stream = None;
    'connect: while !addresses.is_empty() {
        for address in addresses {
            let wait = left().min(CONNECT_WAIT);
            if wait.is_zero() {
                break 'connect;
            }
            if let Ok(connected) = TcpStream::connect_timeout(address, wait) {
                stream = Some(connected);
                break 'connect;
            }
        }
        thread::sleep(POLL.min(left()));
    }
    let Some(mut stream) = stream else {
        return nothing_answers(log);
    };
    let _ = stream.set_nonblocking(true);
    let mut closed_at = None;
    loop {
        if nothing_answers(log) {
            return true;
        }
        let elapsed = started.elapsed();
        if elapsed >= ANSWER_WAIT {
            return false;
        }
        let mut byte = [0; 1];
        match stream.read(&mut byte) {
            // The far side answered: something listens.
            Ok(1..) => return false,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            // ssh closed the connection: its log line follows shortly, if it says anything.
            _ => {
                let closed = *closed_at.get_or_insert(elapsed);
                if elapsed - closed >= CLOSED_GRACE {
                    return false;
                }
            }
        }
        thread::sleep(POLL);
    }
}

/// The record of a failed start of `tunnel`: a temporary forward's keeps its host, forward and
/// `reaches`, so it stays listed and can start again.
fn failed_record(
    tunnel: &Tunnel,
    reaches: Option<&str>,
    reason: &str,
    exit: Option<i32>,
) -> Record {
    let temporary = is_temporary(&tunnel.name);
    Record::Failed {
        reason: reason.to_owned(),
        exit,
        finished: now(),
        host: temporary.then(|| tunnel.host.clone()),
        forward: temporary.then(|| forward_text(&tunnel.forward)),
        reaches: reaches.filter(|_| temporary).map(str::to_owned),
        container: container_text(tunnel),
    }
}

/// A forward as a record's `forward` line writes it.
fn forward_text(forward: &Forward) -> String {
    format!("{} {}", forward.kind.flag(), forward.spec())
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
    /// Every saved tunnel with its status and warnings, in name order, then every temporary
    /// forward, newest first.
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
        let temporaries = self.temporaries(&scan, false);
        // What can hold a saved tunnel's port: the saved tunnels and the temporary forwards.
        let holders: Vec<(Tunnel, TunnelStatus)> = running
            .iter()
            .cloned()
            .chain(temporaries.iter().filter_map(|temporary| {
                let tunnel = temporary.tunnel.clone()?;
                Some((tunnel, temporary.status.clone()))
            }))
            .collect();
        let mut reports: Vec<TunnelReport> = running
            .iter()
            .map(|(tunnel, state)| {
                let status = if state.is_running() {
                    state.clone()
                } else {
                    self.port_status(tunnel, &holders)
                        .unwrap_or_else(|| state.clone())
                };
                let mut warnings = Vec::new();
                if tunnel.container.is_some() {
                    warnings.extend(answer_warning(
                        tunnel,
                        &status,
                        nothing_answers(&self.runtime.log(&tunnel.name)),
                    ));
                }
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
                    temporary: false,
                    reaches: match &tunnel.container {
                        Some(target) => format!("{} on {}", target.reference, tunnel.host),
                        None => tunnel.host.clone(),
                    },
                }
            })
            .collect();
        reports.extend(temporaries.into_iter().map(Temporary::report));
        Ok(reports)
    }

    /// The temporary forwards that have a record, or a process whose record is gone, newest
    /// first. `holds_lock` says the caller holds `start.lock`.
    fn temporaries(&self, scan: &OnceCell<Vec<Logged>>, holds_lock: bool) -> Vec<Temporary> {
        let mut temporaries: Vec<Temporary> = self
            .temporary_names(scan)
            .into_iter()
            .filter_map(|name| {
                // Rebuilds the record of a process found without one.
                let status = self.run_state_in(&name, scan, holds_lock);
                let (tunnel, reaches, since) = match Record::read(&self.runtime.record(&name))? {
                    Record::Running(record) => (
                        recorded_tunnel(
                            &name,
                            &record.host,
                            &record.forward,
                            record.container.as_deref(),
                        ),
                        record.reaches,
                        record.started,
                    ),
                    Record::Failed {
                        finished,
                        host,
                        forward,
                        reaches,
                        container,
                        ..
                    } => (
                        host.zip(forward).and_then(|(host, forward)| {
                            recorded_tunnel(&name, &host, &forward, container.as_deref())
                        }),
                        reaches,
                        finished,
                    ),
                };
                let refused = tunnel.as_ref().is_some_and(|tunnel| {
                    tunnel.container.is_some() && nothing_answers(&self.runtime.log(&name))
                });
                Some(Temporary {
                    name,
                    tunnel,
                    reaches,
                    status,
                    refused,
                    since,
                })
            })
            .collect();
        temporaries.sort_by(|a, b| b.since.cmp(&a.since).then_with(|| a.name.cmp(&b.name)));
        temporaries
    }

    /// The names of the temporary forwards with a record or a process, in no particular order.
    fn temporary_names(&self, scan: &OnceCell<Vec<Logged>>) -> Vec<String> {
        let mut names: Vec<String> = self
            .runtime
            .recorded_names()
            .into_iter()
            .filter(|name| is_temporary(name))
            .collect();
        for process in scan.get_or_init(|| self.proc.scan(&current_uid())) {
            if process.log.parent() != Some(self.runtime.path()) {
                continue;
            }
            if let Some(name) = process
                .log
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".log"))
                && is_temporary(name)
                && !names.iter().any(|known| known == name)
            {
                names.push(name.to_owned());
            }
        }
        names
    }

    /// One tunnel's report, saved or temporary.
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
                SSH_BACKGROUNDS,
            ))
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    pub fn add_tunnel(&self, tunnel: Tunnel) -> Result<(), AppError> {
        self.tunnels.transact(|file| {
            if file.find(&tunnel.name).is_some() {
                return Err(TunnelRefusal::Exists(tunnel.name.clone()).into());
            }
            self.check_tunnel(&stand_in(&tunnel))?;
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

    /// Starts a saved tunnel, or a listed temporary forward again from its record, and waits
    /// until ssh has established it or failed. `cancelled` kills ssh while it is still
    /// connecting.
    pub fn start_tunnel(
        &self,
        name: &str,
        cancelled: Option<&AtomicBool>,
        in_tui: bool,
    ) -> Result<StartOutcome, AppError> {
        if let Some(tunnel) = self.tunnels.load()?.find(name).cloned() {
            return self.launch_resolved(&tunnel, None, cancelled, in_tui);
        }
        let unknown = || AppError::from(TunnelRefusal::Unknown(name.into()));
        if !is_temporary(name) {
            return Err(unknown());
        }
        let temporary = self
            .temporaries(&OnceCell::new(), false)
            .into_iter()
            .find(|temporary| temporary.name.eq_ignore_ascii_case(name))
            .ok_or_else(unknown)?;
        let tunnel = temporary.tunnel.ok_or_else(unknown)?;
        self.launch_resolved(&tunnel, temporary.reaches.as_deref(), cancelled, in_tui)
    }

    /// `launch_checked`, a container tunnel's address looked up first (unless it runs) and
    /// its start followed by the "nothing answers" check. A tunnel that cannot be resolved is
    /// refused before anything changes.
    fn launch_resolved(
        &self,
        tunnel: &Tunnel,
        reaches: Option<&str>,
        cancelled: Option<&AtomicBool>,
        in_tui: bool,
    ) -> Result<StartOutcome, AppError> {
        let Some(target) = &tunnel.container else {
            return self.launch_checked(tunnel, reaches, cancelled, in_tui);
        };
        if let TunnelStatus::Running { pid, .. } = self.run_state(&tunnel.name) {
            return Ok(StartOutcome::AlreadyRunning { pid });
        }
        let resolved = self.resolve_container(tunnel, target, cancelled)?;
        let outcome = self.launch_checked(&resolved, reaches, cancelled, in_tui);
        self.check_answers(&resolved, outcome)
    }

    /// Adds the "nothing answers" warning to the start of container tunnel `tunnel`: one test
    /// connection to its local port, then up to [`ANSWER_WAIT`] for ssh's log to say the
    /// container refused it. The connection is closed at once and not reused.
    fn check_answers(
        &self,
        tunnel: &Tunnel,
        outcome: Result<StartOutcome, AppError>,
    ) -> Result<StartOutcome, AppError> {
        let (Ok(StartOutcome::Started { pid, warning: None }), Some(target)) =
            (&outcome, &tunnel.container)
        else {
            return outcome;
        };
        let addresses = local_sockets(&tunnel.forward.listen_addresses(), tunnel.forward.bind_port);
        let refused = refused_after_connect(&addresses, &self.runtime.log(&tunnel.name));
        if !refused {
            return outcome;
        }
        Ok(StartOutcome::Started {
            pid: *pid,
            warning: Some(
                TunnelWarning::NothingAnswers {
                    port: target.port,
                    reference: target.reference.clone(),
                }
                .to_string(),
            ),
        })
    }

    /// Starts a new temporary forward (`tunnel.name` is a temporary name) that reaches
    /// `reaches`, and waits as `start_tunnel` does. A start that does not succeed leaves
    /// nothing behind when no record of that name existed; when one did (the caller kept a
    /// failed forward's record), the outcome of this start replaces it. [`App::quick_forward`]
    /// forgets a failed forward of the same name first, so a quick forward that fails leaves
    /// nothing behind either.
    pub fn start_temporary(
        &self,
        tunnel: Tunnel,
        reaches: &str,
        cancelled: Option<&AtomicBool>,
        in_tui: bool,
    ) -> Result<StartOutcome, AppError> {
        if !is_temporary(&tunnel.name) {
            return Err(TunnelRefusal::NotTemporary(tunnel.name).into());
        }
        let first = !self.runtime.record(&tunnel.name).exists();
        let outcome = self.launch_checked(&tunnel, Some(reaches), cancelled, in_tui);
        let outcome = self.check_answers(&tunnel, outcome);
        if first
            && !matches!(
                outcome,
                Ok(StartOutcome::Started { .. } | StartOutcome::AlreadyRunning { .. })
            )
        {
            self.forget_run(&tunnel.name);
        }
        outcome
    }

    /// Starts `tunnel`, saved or temporary, once nothing stands in its way: it is not running
    /// yet, OpenSSH accepts it, no running tunnel or temporary forward uses its port and its
    /// local addresses are free. `reaches` is a temporary forward's label, kept in its record.
    fn launch_checked(
        &self,
        tunnel: &Tunnel,
        reaches: Option<&str>,
        cancelled: Option<&AtomicBool>,
        in_tui: bool,
    ) -> Result<StartOutcome, AppError> {
        let file = self.tunnels.load()?;
        let dir = self.runtime.path().to_path_buf();
        self.runtime.ensure().map_err(runtime_error(dir))?;
        let lock_path = self.runtime.lock();
        let lock = open_lock(&lock_path).map_err(runtime_error(lock_path.clone()))?;
        acquire_lock(&lock, cancelled).map_err(|source| -> AppError {
            // Cancelled while another start held the lock.
            if source.kind() == io::ErrorKind::Interrupted
                && cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Relaxed))
            {
                OpenSshError::Cancelled.into()
            } else {
                runtime_error(lock_path)(source).into()
            }
        })?;

        if let TunnelStatus::Running { pid, .. } = self.run_state_locked(&tunnel.name) {
            return Ok(StartOutcome::AlreadyRunning { pid });
        }
        let effective = self.check_tunnel(tunnel)?;
        let scan = OnceCell::new();
        let mut running: Vec<Tunnel> = file
            .tunnels()
            .iter()
            .filter(|other| self.run_state_in(&other.name, &scan, true).is_running())
            .cloned()
            .collect();
        running.extend(
            self.temporaries(&scan, true)
                .into_iter()
                .filter(|temporary| temporary.status.is_running())
                .filter_map(|temporary| temporary.tunnel),
        );
        if let Some(other) = running.iter().find(|other| collide(tunnel, other)) {
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
        self.launch(tunnel, reaches, cancelled, in_tui)
    }

    /// Runs the tunnel's ssh, waits for it to background itself (or, where ssh cannot, for its
    /// forward to come up while it runs detached), and records the result. A temporary
    /// forward's failed record keeps its host, forward and `reaches`, so it can start again,
    /// and one is written however the start ends without success, a cancel included: its
    /// record is the only place its forward lives.
    fn launch(
        &self,
        tunnel: &Tunnel,
        reaches: Option<&str>,
        cancelled: Option<&AtomicBool>,
        in_tui: bool,
    ) -> Result<StartOutcome, AppError> {
        let outcome = self.run_start(tunnel, reaches, cancelled, in_tui);
        if let Err(error) = &outcome
            && is_temporary(&tunnel.name)
        {
            let reason = match error {
                AppError::OpenSsh(OpenSshError::Cancelled) => "cancelled".to_owned(),
                error => error.to_string(),
            };
            let _ = failed_record(tunnel, reaches, &reason, None)
                .write(&self.runtime.record(&tunnel.name));
        }
        outcome
    }

    /// `launch` without the failed record of a temporary forward whose start ended in an error.
    fn run_start(
        &self,
        tunnel: &Tunnel,
        reaches: Option<&str>,
        cancelled: Option<&AtomicBool>,
        in_tui: bool,
    ) -> Result<StartOutcome, AppError> {
        let (record_path, log) = (
            self.runtime.record(&tunnel.name),
            self.runtime.log(&tunnel.name),
        );
        let failed = |reason: &str, exit: Option<i32>| failed_record(tunnel, reaches, reason, exit);
        Record::remove(&record_path).map_err(runtime_error(record_path.clone()))?;
        let create = |path: &PathBuf| {
            private(
                OpenOptions::new().write(true).create(true).truncate(true),
                0o600,
            )
            .open(path)
            .map_err(runtime_error(path.clone()))
        };
        let log_file = create(&log)?;
        // stderr goes to a file, not a pipe: a ProxyCommand child (`ssh -W`, `nc`) inherits it and
        // lives as long as the tunnel, so a pipe would never reach end of file. A foreground ssh
        // logs there too, to the tunnel's log, which Portico opened so it can read it while ssh
        // runs (Windows' ssh would keep its own `-E` file from being read).
        let stderr_path = self.runtime.stderr(&tunnel.name);
        let stderr_file = if SSH_BACKGROUNDS {
            create(&stderr_path)?
        } else {
            log_file
        };
        let mut command = Command::new(self.openssh.ssh_program());
        command
            .args(start_args(
                &log,
                &tunnel.host,
                &tunnel.forward,
                SSH_BACKGROUNDS,
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr_file);
        if !SSH_BACKGROUNDS {
            // This ssh is the tunnel: it must outlive Portico and its console.
            detach(&mut command);
        }
        let mut child = command.spawn().map_err(|source| OpenSshError::Io {
            program: self.openssh.ssh_program().to_path_buf(),
            source,
        })?;
        let spawned = Instant::now();
        // The background ssh is forked from this one, so it cannot have started earlier.
        let since = self.proc.stat(child.id()).map(|stat| stat.start);
        let mut authenticated_at = None;
        let status = loop {
            // A foreground ssh is the tunnel: its forward is judged before ssh is seen running,
            // so it counts as up only while ssh runs.
            let (up, timed_out) = if SSH_BACKGROUNDS {
                (false, false)
            } else {
                if authenticated_at.is_none() && authenticated(&log) {
                    authenticated_at = Some(Instant::now());
                }
                let timed_out = spawned.elapsed() >= READY_BOUND;
                let up = ready(
                    &tunnel.forward,
                    authenticated_at.is_some(),
                    authenticated_at,
                ) || (timed_out
                    && up_at_bound(&tunnel.forward, listening(&tunnel.forward)));
                (up, timed_out)
            };
            if let Some(status) = child.try_wait().map_err(|source| OpenSshError::Io {
                program: self.openssh.ssh_program().to_path_buf(),
                source,
            })? {
                break status;
            }
            // The foreground ssh is the tunnel itself, so no search for it is needed.
            if up
                && let Some(start) =
                    since.or_else(|| self.proc.stat(child.id()).map(|stat| stat.start))
            {
                // ssh keeps writing to its open file; the name is no longer needed.
                let _ = Record::remove(&stderr_path);
                return self.record_started(tunnel, reaches, &record_path, child.id(), start);
            }
            if timed_out {
                end_tree(&mut child);
                let _ = Record::remove(&stderr_path);
                let reason = format!(
                    "ssh did not report the forward up within {} s",
                    READY_BOUND.as_secs()
                );
                failed(&reason, None)
                    .write(&record_path)
                    .map_err(runtime_error(record_path))?;
                return Ok(StartOutcome::Failed {
                    reason,
                    explanation: None,
                });
            }
            if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Relaxed)) {
                end_tree(&mut child);
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
            && let Some((pid, start)) = self.proc.find_tunnel(&log, &uid, since)
        {
            return self.record_started(tunnel, reaches, &record_path, pid, start);
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
        failed(&reason, status.code())
            .write(&record_path)
            .map_err(runtime_error(record_path))?;
        Ok(StartOutcome::Failed {
            reason,
            explanation,
        })
    }

    /// Records that `tunnel`, reaching `reaches` when it is a temporary forward, runs as `pid`,
    /// which started at `start`.
    fn record_started(
        &self,
        tunnel: &Tunnel,
        reaches: Option<&str>,
        record_path: &Path,
        pid: u32,
        start: u64,
    ) -> Result<StartOutcome, AppError> {
        Record::Running(RunningRecord {
            pid,
            start,
            boot: self.proc.boot_id().unwrap_or_default(),
            started: now(),
            host: tunnel.host.clone(),
            forward: forward_text(&tunnel.forward),
            reaches: reaches.map(str::to_owned),
            container: container_text(tunnel),
        })
        .write(record_path)
        .map_err(runtime_error(record_path.to_path_buf()))?;
        Ok(StartOutcome::Started { pid, warning: None })
    }

    /// Stops a tunnel: its process when one is running, and its record and log in any case. A
    /// temporary forward stopped this way is gone.
    pub fn stop_tunnel(&self, name: &str) -> Result<StopOutcome, AppError> {
        let name = self.saved_name(name).or_else(|error| {
            // A tunnel removed from the file while it ran can still be stopped, and so can a
            // temporary forward, even one whose record is gone.
            self.runtime
                .recorded_names()
                .into_iter()
                .find(|recorded| recorded.eq_ignore_ascii_case(name))
                .or_else(|| {
                    is_temporary(name)
                        .then(|| self.temporary_names(&OnceCell::new()))
                        .and_then(|names| {
                            names
                                .into_iter()
                                .find(|known| known.eq_ignore_ascii_case(name))
                        })
                })
                .ok_or(error)
        })?;
        let record_path = self.runtime.record(&name);
        let log = self.runtime.log(&name);
        let stopped = match self.run_state(&name) {
            TunnelStatus::Running { .. } => match Record::read(&record_path) {
                Some(Record::Running(record)) => {
                    let uid = current_uid();
                    stop_process(record.pid, || {
                        self.proc.identify(&record, &log, &uid) == Identity::Alive
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

    /// The names of the tunnels that run now, including any no longer saved and the temporary
    /// forwards.
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
        let scan = OnceCell::new();
        for recorded in self
            .runtime
            .recorded_names()
            .into_iter()
            .chain(self.temporary_names(&scan))
        {
            if !names
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&recorded))
            {
                names.push(recorded);
            }
        }
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
    pub(super) fn forget_run(&self, name: &str) {
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
                match self.proc.identify(&record, &log, &current_uid()) {
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
            scan.get_or_init(|| self.proc.scan(&current_uid())),
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

/// How long a container tunnel's start waits for ssh to report the test connection refused.
const ANSWER_WAIT: Duration = Duration::from_millis(800);

/// How long one connection attempt to a local listener may take.
const CONNECT_WAIT: Duration = Duration::from_millis(200);

/// How long after ssh closes the test connection its log may still gain the refused line.
const CLOSED_GRACE: Duration = Duration::from_millis(200);

/// How often the test connection and the log are looked at.
const POLL: Duration = Duration::from_millis(20);

/// `tunnel` as `ssh -G` checks it when it is saved: a container tunnel's destination, the
/// reference, stands in as `localhost:<port inside>`, which validates the host and the listen
/// side.
fn stand_in(tunnel: &Tunnel) -> Tunnel {
    let mut checked = tunnel.clone();
    if let Some(target) = &tunnel.container {
        checked.forward.destination = Some(Destination {
            host: "localhost".into(),
            port: target.port,
        });
    }
    checked
}

/// Ends a start's ssh and what it started: a ProxyCommand child would outlive ssh, so the
/// whole tree goes.
fn end_tree(child: &mut Child) {
    if kill_process_tree(child.id()).is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{
        domain::managed::{ConfigPaths, ManagedStore},
        domain::openssh::OpenSsh,
        domain::tunnel::runtime::RuntimeDir,
        platform::process::Proc,
    };

    #[cfg(target_os = "linux")]
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

        use crate::{domain::host::Host, domain::tunnel::Kind, test_executable::write_executable};

        let dir = tempfile::tempdir().unwrap();
        let child_pid = dir.path().join("child-pid");
        let ssh = dir.path().join("ssh");
        // Answers `-G`; a start leaves a child (like a ProxyCommand) and keeps connecting.
        write_executable(
            &ssh,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = -G ]; then printf 'hostname x\\nport 22\\n'; exit 0; fi\n\
                 sleep 60 &\nchild=$!\n\
                 [ -r /proc/$child/winpid ] && child=$(cat /proc/$child/winpid)\n\
                 echo $child > '{}'\nsleep 60\n",
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
        let pid: u32 = fs::read_to_string(&child_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Proc::system()
            .stat(pid)
            .is_some_and(|stat| stat.state != 'Z')
        {
            assert!(
                Instant::now() < deadline,
                "the proxy-like child {pid} survived"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(target_os = "linux")]
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
            reaches: None,
            container: None,
        })
        .write(&record)
        .unwrap();
        assert_eq!(app.run_state("db"), TunnelStatus::Stopped);
        assert!(!record.exists(), "the stale record was kept");
    }

    #[cfg(target_os = "linux")]
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

    /// An app with the host `prod` whose `ssh` fakes tunnel starts: `-G` answers like OpenSSH,
    /// and a start leaves a background stand-in that carries its arguments, as ssh's background
    /// process does, unless `<dir>/fail-start` exists, when it fails as ssh does on a refused
    /// forward, or `<dir>/hang-start`, when it keeps connecting (and says so in
    /// `<dir>/connecting`).
    #[cfg(target_os = "linux")]
    fn tunnel_app(dir: &Path) -> App {
        use crate::{domain::host::Host, test_executable::write_executable};

        let ssh = dir.join("ssh");
        write_executable(
            &ssh,
            &format!(
                r#"#!/bin/sh
if [ "$1" = -G ]; then printf 'hostname x\nport 22\n'; exit 0; fi
elog=/dev/null
prev=
for arg do
  [ "$prev" = -E ] && elog=$arg
  prev=$arg
done
if [ -f '{fail}' ]; then
  echo 'Error: remote port forwarding failed for listen port 8080' >> "$elog"
  exit 255
fi
if [ -f '{hang}' ]; then
  : > '{connecting}'
  exec sleep 60
fi
setsid sh -c 'sleep 60; exit 0' sh "$@" </dev/null >/dev/null 2>&1 &
exit 0
"#,
                fail = dir.join("fail-start").display(),
                hang = dir.join("hang-start").display(),
                connecting = dir.join("connecting").display(),
            ),
        );
        fs::create_dir_all(dir.join("home/.ssh")).unwrap();
        let app = App::new(
            ManagedStore::new(ConfigPaths::from_home(dir.join("home"))),
            OpenSsh::new(ssh, "terminal".into()),
        )
        .with_runtime_dir(RuntimeDir::at(dir.join("run")));
        app.initialize(true).unwrap();
        app.add(Host::new("prod", "x", None, None, None, None).unwrap())
            .unwrap();
        app
    }

    /// A port nobody uses: bound, read, released.
    #[cfg(target_os = "linux")]
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A temporary forward from `127.0.0.1:<port>` to port 80 of `prod`.
    #[cfg(target_os = "linux")]
    fn temporary(port: u16) -> Tunnel {
        use crate::domain::tunnel::{Kind, temporary_name};

        Tunnel {
            name: temporary_name("prod", port),
            host: "prod".into(),
            forward: Forward::parse(Kind::Local, &format!("127.0.0.1:{port}:localhost:80"))
                .unwrap(),
            container: None,
        }
    }

    /// Stops every tunnel when dropped, so no stand-in outlives a failed test.
    #[cfg(target_os = "linux")]
    struct StopAll<'a>(&'a App);

    #[cfg(target_os = "linux")]
    impl Drop for StopAll<'_> {
        fn drop(&mut self) {
            for name in self.0.running_tunnels().unwrap_or_default() {
                let _ = self.0.stop_tunnel(&name);
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_temporary_forward_starts_lists_and_stops() {
        use crate::domain::tunnel::Kind;

        let dir = tempfile::tempdir().unwrap();
        let app = tunnel_app(dir.path());
        let _stop = StopAll(&app);
        let saved = Forward::parse(Kind::Dynamic, &format!("127.0.0.1:{}", free_port())).unwrap();
        app.add_tunnel(Tunnel::new("socks", "prod", saved).unwrap())
            .unwrap();
        let tunnel = temporary(free_port());
        let name = tunnel.name.clone();

        let outcome = app.start_temporary(tunnel, "prod", None, false).unwrap();
        assert!(
            matches!(outcome, StartOutcome::Started { .. }),
            "{outcome:?}"
        );
        let record = fs::read_to_string(app.runtime.record(&name)).unwrap();
        assert!(record.contains("reaches prod\n"), "{record}");

        let reports = app.tunnels().unwrap();
        assert_eq!(reports.len(), 2, "{reports:?}");
        assert_eq!(reports[0].tunnel.name, "socks");
        assert!(!reports[0].temporary);
        assert_eq!(reports[0].reaches, "prod");
        let last = &reports[1];
        assert_eq!(last.tunnel.name, name);
        assert!(last.temporary);
        assert_eq!(last.reaches, "prod");
        assert!(last.status.is_running(), "{:?}", last.status);
        assert_eq!(app.running_tunnels().unwrap(), vec![name.clone()]);
        assert_eq!(app.tunnel(&name).unwrap().tunnel.name, name);

        assert_eq!(app.stop_tunnel(&name).unwrap(), StopOutcome::Stopped);
        assert!(!app.runtime.record(&name).exists());
        assert!(!app.runtime.log(&name).exists());
        assert!(
            app.tunnels()
                .unwrap()
                .iter()
                .all(|report| !report.temporary)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_temporary_forward_that_fails_first_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let app = tunnel_app(dir.path());
        let _stop = StopAll(&app);
        fs::write(dir.path().join("fail-start"), "").unwrap();
        let tunnel = temporary(free_port());
        let name = tunnel.name.clone();

        let outcome = app.start_temporary(tunnel, "prod", None, false).unwrap();
        assert!(
            matches!(outcome, StartOutcome::Failed { .. }),
            "{outcome:?}"
        );
        assert!(!app.runtime.record(&name).exists());
        assert!(!app.runtime.log(&name).exists());
        assert!(app.tunnels().unwrap().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_restart_keeps_the_temporary_forward_listed() {
        let dir = tempfile::tempdir().unwrap();
        let app = tunnel_app(dir.path());
        let _stop = StopAll(&app);
        let tunnel = temporary(free_port());
        let name = tunnel.name.clone();
        let StartOutcome::Started { pid, .. } =
            app.start_temporary(tunnel, "prod", None, false).unwrap()
        else {
            panic!("the forward did not start");
        };

        // The ssh exits (the network dropped): the forward is listed as failed.
        Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.run_state(&name).is_running() {
            assert!(Instant::now() < deadline, "the stand-in survived");
            thread::sleep(Duration::from_millis(10));
        }

        fs::write(dir.path().join("fail-start"), "").unwrap();
        let outcome = app.start_tunnel(&name, None, false).unwrap();
        assert!(
            matches!(outcome, StartOutcome::Failed { .. }),
            "{outcome:?}"
        );
        let Some(Record::Failed {
            host,
            forward,
            reaches,
            ..
        }) = Record::read(&app.runtime.record(&name))
        else {
            panic!("no failed record");
        };
        assert_eq!(host.as_deref(), Some("prod"));
        assert!(forward.is_some_and(|forward| forward.starts_with("-L 127.0.0.1:")));
        assert_eq!(reaches.as_deref(), Some("prod"));
        let reports = app.tunnels().unwrap();
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].tunnel.name, name);
        assert_eq!(reports[0].status.name(), "failed");
        assert_eq!(reports[0].reaches, "prod");

        // And it can be started again from its record.
        fs::remove_file(dir.path().join("fail-start")).unwrap();
        let outcome = app.start_tunnel(&name, None, false).unwrap();
        assert!(
            matches!(outcome, StartOutcome::Started { .. }),
            "{outcome:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_saved_tunnel_cannot_start_on_a_temporary_forwards_port() {
        let dir = tempfile::tempdir().unwrap();
        let app = tunnel_app(dir.path());
        let _stop = StopAll(&app);
        let port = free_port();
        let tunnel = temporary(port);
        let name = tunnel.name.clone();
        app.start_temporary(tunnel, "prod", None, false).unwrap();
        let forward = Forward::parse(
            crate::domain::tunnel::Kind::Local,
            &format!("127.0.0.1:{port}:localhost:22"),
        )
        .unwrap();
        app.add_tunnel(Tunnel::new("web", "prod", forward).unwrap())
            .unwrap();

        let refused = app
            .start_tunnel("web", None, false)
            .unwrap_err()
            .to_string();
        assert!(refused.contains(&format!("tunnel {name}")), "{refused}");

        // And the reverse: a temporary forward on a running saved tunnel's port.
        app.stop_tunnel(&name).unwrap();
        app.start_tunnel("web", None, false).unwrap();
        let refused = app
            .start_temporary(temporary(port), "prod", None, false)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("tunnel web"), "{refused}");
        assert!(!app.runtime.record(&name).exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_busy_local_port_refuses_a_temporary_start() {
        let dir = tempfile::tempdir().unwrap();
        let app = tunnel_app(dir.path());
        let _stop = StopAll(&app);
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = held.local_addr().unwrap().port();
        let tunnel = temporary(port);
        let name = tunnel.name.clone();

        let refused = app.start_temporary(tunnel, "prod", None, false);
        assert!(
            matches!(
                refused,
                Err(AppError::Tunnel(TunnelRefusal::PortInUse { port: busy, .. })) if busy == port
            ),
            "{refused:?}"
        );
        assert!(!app.runtime.record(&name).exists());
        assert!(!app.runtime.log(&name).exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_a_temporary_name_starts_a_temporary_forward() {
        let dir = tempfile::tempdir().unwrap();
        let app = tunnel_app(dir.path());
        let mut tunnel = temporary(free_port());
        tunnel.name = "web".into();
        let refused = app.start_temporary(tunnel, "prod", None, false);
        assert!(
            matches!(
                &refused,
                Err(AppError::Tunnel(TunnelRefusal::NotTemporary(name))) if name == "web"
            ),
            "{refused:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_temporary_record_without_its_forward_is_listed_but_cannot_start() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path());
        app.runtime.ensure().unwrap();
        Record::Failed {
            reason: "exited".into(),
            exit: None,
            finished: 0,
            host: None,
            forward: None,
            reaches: None,
            container: None,
        }
        .write(&app.runtime.record("_web-80"))
        .unwrap();
        let reports = app.tunnels().unwrap();
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].tunnel.name, "_web-80");
        assert!(reports[0].temporary);
        assert_eq!(reports[0].reaches, "unknown");
        assert!(matches!(
            app.start_tunnel("_web-80", None, false),
            Err(AppError::Tunnel(TunnelRefusal::Unknown(_)))
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn temporary_forwards_follow_the_saved_ones_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path());
        app.runtime.ensure().unwrap();
        for (name, finished) in [("_old-80", 100), ("_new-81", 200)] {
            Record::Failed {
                reason: "exited".into(),
                exit: None,
                finished,
                host: Some("prod".into()),
                forward: Some("-L 127.0.0.1:8080:172.18.0.4:80".into()),
                reaches: None,
                container: None,
            }
            .write(&app.runtime.record(name))
            .unwrap();
        }
        let reports = app.tunnels().unwrap();
        let names: Vec<&str> = reports
            .iter()
            .map(|report| report.tunnel.name.as_str())
            .collect();
        assert_eq!(names, ["_new-81", "_old-80"]);
        // A record without `reaches` shows the destination.
        assert_eq!(reports[0].reaches, "172.18.0.4:80");
    }

    /// A listed temporary forward whose ssh failed, as `start_temporary` leaves it.
    #[cfg(target_os = "linux")]
    fn failed_temporary(app: &App) -> String {
        let tunnel = temporary(free_port());
        app.runtime.ensure().unwrap();
        failed_record(&tunnel, Some("prod"), "exited", Some(255))
            .write(&app.runtime.record(&tunnel.name))
            .unwrap();
        tunnel.name
    }

    /// The forward is still listed, failed, with its host, forward and `reaches`.
    #[cfg(target_os = "linux")]
    fn assert_kept(app: &App, name: &str) {
        let Some(Record::Failed {
            host,
            forward,
            reaches,
            ..
        }) = Record::read(&app.runtime.record(name))
        else {
            panic!("no failed record for {name}");
        };
        assert_eq!(host.as_deref(), Some("prod"));
        assert!(forward.is_some());
        assert_eq!(reaches.as_deref(), Some("prod"));
        let reports = app.tunnels().unwrap();
        assert!(
            reports
                .iter()
                .any(|report| report.tunnel.name == name && report.status.name() == "failed"),
            "{reports:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_cancelled_restart_keeps_the_temporary_forward() {
        let dir = tempfile::tempdir().unwrap();
        let app = tunnel_app(dir.path());
        let _stop = StopAll(&app);
        let name = failed_temporary(&app);
        let cancelled = AtomicBool::new(true);
        let result = app.start_tunnel(&name, Some(&cancelled), false);
        assert!(
            matches!(result, Err(AppError::OpenSsh(OpenSshError::Cancelled))),
            "{result:?}"
        );
        assert_kept(&app, &name);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_restart_cancelled_while_connecting_keeps_the_temporary_forward() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let app = tunnel_app(dir.path());
        let _stop = StopAll(&app);
        let name = failed_temporary(&app);
        fs::write(dir.path().join("hang-start"), "").unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let canceller = {
            let (cancelled, connecting) = (Arc::clone(&cancelled), dir.path().join("connecting"));
            thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !connecting.exists() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                cancelled.store(true, Ordering::Relaxed);
            })
        };
        let result = app.start_tunnel(&name, Some(&cancelled), false);
        canceller.join().unwrap();
        assert!(
            matches!(result, Err(AppError::OpenSsh(OpenSshError::Cancelled))),
            "{result:?}"
        );
        assert_kept(&app, &name);
        let Some(Record::Failed { reason, .. }) = Record::read(&app.runtime.record(&name)) else {
            unreachable!()
        };
        assert_eq!(reason, "cancelled");
    }

    /// What `refused_after_connect` says, and how long it took, while a listener the test owns
    /// accepts the connection and `serve` handles it; `serve` gets the log path. The accepted
    /// connection stays open until the probe returns unless `serve` shuts it down.
    fn probe(
        serve: impl FnOnce(&mut std::net::TcpStream, &Path) + Send + 'static,
    ) -> (bool, Duration) {
        probe_through(false, serve)
    }

    /// `probe`, with an address that refuses tried first when `refusing_first`: the
    /// listener's port at `127.0.0.2`, which nothing else can bind while the test holds
    /// `127.0.0.1` on it (a released port could be taken by another test meanwhile).
    fn probe_through(
        refusing_first: bool,
        serve: impl FnOnce(&mut std::net::TcpStream, &Path) + Send + 'static,
    ) -> (bool, Duration) {
        use std::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("api.log");
        fs::write(&log, "Authenticated to x.\n").unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (done, finished) = mpsc::channel::<()>();
        let server = {
            let log = log.clone();
            thread::spawn(move || {
                // Never waits past a probe that did not connect.
                listener.set_nonblocking(true).unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "the probe never connected");
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                serve(&mut stream, &log);
                let _ = finished.recv();
            })
        };
        let mut addresses = vec![address];
        if refusing_first {
            addresses.insert(0, SocketAddr::new([127, 0, 0, 2].into(), address.port()));
        }
        let started = Instant::now();
        let refused = refused_after_connect(&addresses, &log);
        let elapsed = started.elapsed();
        drop(done);
        server.join().unwrap();
        (refused, elapsed)
    }

    #[test]
    fn a_refused_channel_logged_after_the_connection_is_nothing_answering() {
        let (refused, elapsed) = probe(|stream, log| {
            thread::sleep(Duration::from_millis(100));
            let mut text = fs::read_to_string(log).unwrap();
            text.push_str("channel 2: open failed: connect failed: Connection refused\n");
            fs::write(log, text).unwrap();
            stream.shutdown(std::net::Shutdown::Both).unwrap();
        });
        assert!(refused);
        assert!(
            elapsed < ANSWER_WAIT + Duration::from_millis(400),
            "{elapsed:?}"
        );
    }

    #[test]
    fn a_byte_from_the_far_side_is_an_answer_at_once() {
        use std::io::Write;

        let (refused, elapsed) = probe(|stream, _| {
            stream.write_all(b"S").unwrap();
        });
        assert!(!refused);
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_address_that_refuses_is_passed_over_for_the_next() {
        use std::io::Write;

        let (refused, elapsed) = probe_through(true, |stream, _| {
            stream.write_all(b"S").unwrap();
        });
        assert!(!refused);
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    }

    #[test]
    fn a_silent_far_side_is_given_the_whole_wait_and_no_more() {
        let (refused, elapsed) = probe(|_, _| {});
        assert!(!refused);
        assert!(elapsed >= ANSWER_WAIT, "{elapsed:?}");
        assert!(
            elapsed < ANSWER_WAIT + Duration::from_millis(400),
            "{elapsed:?}"
        );
    }

    #[test]
    fn a_connection_closed_without_a_logged_refusal_says_nothing() {
        let (refused, elapsed) = probe(|stream, _| {
            stream.shutdown(std::net::Shutdown::Both).unwrap();
        });
        assert!(!refused);
        assert!(elapsed < ANSWER_WAIT, "{elapsed:?}");
    }

    #[test]
    fn nothing_to_connect_to_reads_the_log_alone() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("api.log");
        let started = Instant::now();
        assert!(!refused_after_connect(&[], &log));
        fs::write(
            &log,
            "channel 2: open failed: connect failed: Connection refused\n",
        )
        .unwrap();
        assert!(refused_after_connect(&[], &log));
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn listen_addresses_become_the_sockets_a_test_connection_reaches() {
        let sockets = local_sockets(
            &[
                "0.0.0.0".into(),
                "::".into(),
                "127.0.0.1".into(),
                "localhost".into(),
            ],
            8080,
        );
        assert_eq!(sockets[0], "127.0.0.1:8080".parse().unwrap());
        assert_eq!(sockets[1], "[::1]:8080".parse().unwrap());
        // The duplicate is dropped; `localhost` resolves to the loopback.
        assert_eq!(
            sockets.iter().filter(|s| s.port() == 8080).count(),
            sockets.len()
        );
        assert_eq!(
            sockets
                .iter()
                .filter(|s| **s == "127.0.0.1:8080".parse().unwrap())
                .count(),
            1
        );
        assert!(
            sockets.iter().all(|socket| socket.ip().is_loopback()),
            "{sockets:?}"
        );
    }
}
