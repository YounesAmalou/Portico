//! Quick forwards: the ports a host or a container suggests, a temporary forward to one of them
//! in one step, and keeping a temporary forward as a saved tunnel.

use std::sync::atomic::AtomicBool;

use super::{App, AppError, ContainerError, StartOutcome, TunnelRefusal};
use crate::{
    backend::{
        self, BackendId, Container, ListFailure, Reach, ResolveError, RunFailure, Target, Via,
        display_name, remember_key, resolve_lowest,
    },
    domain::openssh::OpenSshError,
    domain::tunnel::{
        ContainerTarget, Destination, Forward, Kind, Tunnel, TunnelReport, is_temporary,
        ports::{HOST_PORTS_SCRIPT, parse_listeners, suggested_ports},
        runtime::{default_local_port, try_bind},
        temporary_name, unique_name,
    },
};

/// What a quick forward reaches: a host of the inventory, or a container on one.
#[derive(Clone, Debug, Eq, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "one is built per forward dialog or command; callers match the container directly"
)]
pub enum ForwardTarget {
    Host(String),
    Container {
        target: Target,
        backend: BackendId,
        container: Container,
        /// The tool the listing had to run through, passed on to the backend.
        via: Option<Via>,
    },
}

/// One remote port worth forwarding, with what is known about it (`published 8080`,
/// `loopback only`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PortSuggestion {
    pub port: u16,
    pub note: Option<String>,
}

/// The ports a target suggests, most likely first.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Suggestions {
    pub ports: Vec<PortSuggestion>,
    /// Where a container can be reached, for the forward that follows.
    pub reach: Option<Reach>,
    /// Why there are no suggestions, in one line, when asking failed.
    pub unavailable: Option<String>,
}

/// A temporary forward saved as a tunnel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SaveOutcome {
    pub tunnel: Tunnel,
    /// The saved tunnel's start, when the temporary forward was running.
    pub restarted: Option<StartOutcome>,
    /// Something to say once about the saved tunnel; none is said now.
    pub note: Option<String>,
    /// Whether the restart was cancelled: the tunnel is saved but not started.
    pub cancelled: bool,
}

impl ForwardTarget {
    /// What it reaches by name: `prod`, `web on prod`, `CT 101 on pve`.
    pub fn label(&self) -> String {
        match self {
            ForwardTarget::Host(alias) => alias.clone(),
            ForwardTarget::Container {
                target,
                backend: BackendId::Proxmox,
                container,
                ..
            } => format!("CT {} on {target}", container.id),
            ForwardTarget::Container { target, .. } => format!("{} on {target}", self.short()),
        }
    }

    /// The short name a temporary forward's name starts from: `prod`, `web`, `101`.
    pub fn short(&self) -> String {
        match self {
            ForwardTarget::Host(alias) => alias.clone(),
            ForwardTarget::Container {
                backend: BackendId::Proxmox,
                container,
                ..
            } => container.id.clone(),
            ForwardTarget::Container { container, .. } => {
                display_name(container, std::slice::from_ref(container))
            }
        }
    }
}

impl App {
    /// The ports `target` suggests: a host's listening TCP ports, a container's published then
    /// exposed ports with where it can be reached. Never fails: why it could not ask goes to
    /// `unavailable`, and the ports stay empty. ssh never prompts; `cancelled` stops it.
    pub fn forward_suggestions(
        &self,
        target: &ForwardTarget,
        cancelled: Option<&AtomicBool>,
    ) -> Suggestions {
        match target {
            ForwardTarget::Host(alias) => match self.host_ports(alias, cancelled) {
                Ok(ports) => Suggestions {
                    ports,
                    ..Suggestions::default()
                },
                Err(reason) => Suggestions {
                    unavailable: Some(format!("could not list the ports of {alias}: {reason}")),
                    ..Suggestions::default()
                },
            },
            ForwardTarget::Container {
                target: on,
                backend,
                container,
                via,
            } => match self.container_reach(on, *backend, container, *via, cancelled) {
                Ok(reach) => Suggestions {
                    ports: reach
                        .ports
                        .iter()
                        .map(|offer| PortSuggestion {
                            port: offer.port,
                            note: match offer.published {
                                Some(host) => Some(format!("published {host}")),
                                None => offer.loopback_only.then(|| "loopback only".into()),
                            },
                        })
                        .collect(),
                    reach: Some(reach),
                    unavailable: None,
                },
                Err(error) => Suggestions {
                    unavailable: Some(format!(
                        "could not list the ports of {}: {}",
                        target.label(),
                        container_reason(&error)
                    )),
                    ..Suggestions::default()
                },
            },
        }
    }

    /// Starts a temporary forward from `127.0.0.1:<local>` to port `remote` of `target`, and
    /// waits as [`App::start_temporary`] does. A host's port is reached at its `localhost`; a
    /// container's through the host port it is published on, at `localhost` when it shares the
    /// host's network, and otherwise at its address, from `reach` or asked now. `local`
    /// defaults to the remote port when `127.0.0.1` binds it (see [`default_local_port`]). The
    /// forward is named after the target and the port; a running forward of that name leaves
    /// it to the next free suffix, a failed one is replaced.
    pub fn quick_forward(
        &self,
        target: &ForwardTarget,
        reach: Option<&Reach>,
        remote: u16,
        local: Option<u16>,
        cancelled: Option<&AtomicBool>,
        in_tui: bool,
    ) -> Result<(Tunnel, StartOutcome), AppError> {
        if remote == 0 {
            return Err(TunnelRefusal::NoPort.into());
        }
        if local == Some(0) {
            return Err(TunnelRefusal::NoLocalPort.into());
        }
        let label = target.label();
        let mut container_target = None;
        let (host, destination) = match target {
            ForwardTarget::Host(alias) => (
                self.connectable_alias(alias)?,
                Destination {
                    host: "localhost".into(),
                    port: remote,
                },
            ),
            ForwardTarget::Container {
                target: Target::Local,
                ..
            } => return Err(TunnelRefusal::LocalTarget.into()),
            ForwardTarget::Container {
                target: Target::Host(alias),
                backend,
                container,
                via,
            } => {
                let alias = self.connectable_alias(alias)?;
                let asked;
                let reach = match reach {
                    Some(reach) => reach,
                    None => {
                        asked = self
                            .container_reach(
                                &Target::Host(alias.clone()),
                                *backend,
                                container,
                                *via,
                                cancelled,
                            )
                            .map_err(|error| match error {
                                ContainerError::App(error) => error,
                                error => TunnelRefusal::ReachFailed {
                                    label: label.clone(),
                                    reason: container_reason(&error),
                                }
                                .into(),
                            })?;
                        &asked
                    }
                };
                let destination = destination(reach, remote).ok_or_else(|| match backend {
                    BackendId::Proxmox => TunnelRefusal::NoCtAddress {
                        id: container.id.clone(),
                        host: alias.clone(),
                    },
                    _ => TunnelRefusal::Unreachable {
                        label: label.clone(),
                    },
                })?;
                container_target = Some(ContainerTarget {
                    backend: *backend,
                    reference: temporary_reference(*backend, container),
                    port: remote,
                });
                (alias, destination)
            }
        };
        let local =
            match local {
                Some(local) => local,
                None => default_local_port(remote, |port| try_bind("127.0.0.1", port)).ok_or_else(
                    || TunnelRefusal::PortInUse {
                        address: "127.0.0.1".into(),
                        port: remote,
                        by: None,
                    },
                )?,
            };
        let name = self.quick_name(&temporary_name(&target.short(), remote))?;
        let tunnel = Tunnel {
            name,
            host,
            forward: Forward {
                kind: Kind::Local,
                bind_address: Some("127.0.0.1".into()),
                bind_port: local,
                destination: Some(destination),
            },
            container: container_target,
        };
        let outcome = self.start_temporary(tunnel.clone(), &label, cancelled, in_tui)?;
        Ok((tunnel, outcome))
    }

    /// Where each port of a container on this machine is already reachable, without a
    /// forward: a published port at `127.0.0.1:<host port>`; on Linux also any other port at
    /// the container's address (elsewhere that address is inside Docker Desktop's VM).
    pub fn local_addresses(&self, _container: &Container, reach: &Reach) -> Vec<(u16, String)> {
        reachable_here(reach, cfg!(target_os = "linux"))
    }

    /// Saves temporary forward `temporary` as tunnel `name`, as the tunnel form would. A
    /// running forward is stopped and the saved tunnel started at once (ssh finds a tunnel by
    /// its log path, which cannot change while it runs); a failed one is saved without
    /// starting. A forward to a container is saved as a container tunnel, which looks the
    /// container's address up at each start. Setting `cancelled` cancels the restart, never
    /// the save.
    pub fn save_tunnel(
        &self,
        temporary: &str,
        name: &str,
        cancelled: Option<&AtomicBool>,
    ) -> Result<SaveOutcome, AppError> {
        if !is_temporary(temporary) {
            return Err(TunnelRefusal::NotTemporary(temporary.into()).into());
        }
        let report = self.tunnel(temporary)?;
        // A record rebuilt without its host and forward has nothing to save.
        if !report.temporary || report.tunnel.host.is_empty() {
            return Err(TunnelRefusal::Unknown(temporary.into()).into());
        }
        let tunnel = match report.tunnel.container {
            Some(mut target) => {
                // A temporary forward to one replica keeps its number; a saved tunnel does not.
                if target.reference.contains('/')
                    && let Some((service, number)) = target.reference.rsplit_once('#')
                    && number.parse::<u32>().is_ok()
                {
                    target.reference = service.to_owned();
                }
                Tunnel::to_container(
                    name,
                    report.tunnel.host.clone(),
                    report.tunnel.forward,
                    target,
                )?
            }
            None => Tunnel::new(name, report.tunnel.host.clone(), report.tunnel.forward)?,
        };
        self.add_tunnel(tunnel.clone())?;
        // The tunnel is saved from here on: a stop or start refused is its failed start, not an
        // error, so the save is never half applied. A cancelled start leaves it saved.
        let mut restart_cancelled = false;
        let restarted = if report.status.is_running() {
            let started = self
                .stop_tunnel(&report.tunnel.name)
                .and_then(|_| self.start_tunnel(&tunnel.name, cancelled, false));
            match started {
                Err(AppError::OpenSsh(OpenSshError::Cancelled)) => {
                    restart_cancelled = true;
                    None
                }
                started => Some(started.unwrap_or_else(|error| StartOutcome::Failed {
                    reason: first_line(&error.to_string()),
                    explanation: None,
                })),
            }
        } else {
            self.forget_run(&report.tunnel.name);
            None
        };
        Ok(SaveOutcome {
            tunnel,
            restarted,
            note: None,
            cancelled: restart_cancelled,
        })
    }

    /// The backend whose containers on host `host` include `reference`, with the reference a
    /// saved tunnel keeps for the container (`project/service` for a Compose container, a CT's
    /// VMID): each backend installed there is listed in turn, `first` (when given) then the
    /// others in the [`backend::AUTOMATIC`] order (ssh never prompts), and the first whose listing resolves the reference (a Compose
    /// service to its lowest replica) is the one. A reference matching several containers of
    /// one backend is refused as a start refuses it. When no backend has it, the first backend
    /// that refused to list is the failure, or else the container is not running there.
    pub fn find_container(
        &self,
        host: &str,
        reference: &str,
        first: Option<BackendId>,
    ) -> Result<(BackendId, String), AppError> {
        let on = Target::Host(self.connectable_alias(host)?);
        let mut refused = None;
        let order = first.into_iter().chain(
            backend::AUTOMATIC
                .into_iter()
                .filter(|id| Some(*id) != first),
        );
        for backend in order {
            let list = match self.containers(&on, &[backend], true, None) {
                Ok(list) => list,
                Err(ContainerError::Backend(issue))
                    if issue.problem == backend::Problem::NotInstalled =>
                {
                    continue;
                }
                Err(ContainerError::Backend(issue)) => {
                    refused.get_or_insert(ContainerError::Backend(issue));
                    continue;
                }
                Err(ContainerError::App(error)) => return Err(error),
                Err(error) => return Err(AppError::Container(Box::new(error))),
            };
            match resolve_lowest(reference, &list.listing.containers) {
                Ok(container) => return Ok((backend, saved_reference(backend, container))),
                Err(ResolveError::NotFound) => {}
                Err(ResolveError::Ambiguous(candidates)) => {
                    return Err(TunnelRefusal::AmbiguousContainer {
                        reference: reference.to_owned(),
                        host: host.to_owned(),
                        candidates,
                    }
                    .into());
                }
            }
        }
        Err(match refused {
            Some(error) => AppError::Container(Box::new(error)),
            None => TunnelRefusal::NotRunningAnywhere {
                reference: reference.to_owned(),
                host: host.to_owned(),
            }
            .into(),
        })
    }

    /// Container tunnel `tunnel` as it starts now: the containers of its host listed with its
    /// backend, its reference resolved (a Compose service to its lowest replica), and its
    /// destination found as a quick forward's is. The tunnel keeps its name, listen side and
    /// container target. ssh never prompts; `cancelled` stops it.
    pub(super) fn resolve_container(
        &self,
        tunnel: &Tunnel,
        target: &ContainerTarget,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Tunnel, AppError> {
        let host = &tunnel.host;
        let on = Target::Host(self.connectable_alias(host)?);
        let list = self
            .containers(&on, &[target.backend], true, cancelled)
            .map_err(|error| match error {
                ContainerError::App(error) => error,
                error => AppError::Container(Box::new(error)),
            })?;
        let container = resolve_lowest(&target.reference, &list.listing.containers).map_err(
            |error| match error {
                ResolveError::NotFound => TunnelRefusal::NotRunning {
                    reference: target.reference.clone(),
                    host: host.clone(),
                    backend: target.backend,
                },
                ResolveError::Ambiguous(candidates) => TunnelRefusal::AmbiguousContainer {
                    reference: target.reference.clone(),
                    host: host.clone(),
                    candidates,
                },
            },
        )?;
        let reach = self
            .container_reach(&on, target.backend, container, list.via, cancelled)
            .map_err(|error| match error {
                ContainerError::App(error) => error,
                error => TunnelRefusal::ReachFailed {
                    label: format!("{} on {host}", target.reference),
                    reason: container_reason(&error),
                }
                .into(),
            })?;
        let destination = destination(&reach, target.port).ok_or_else(|| match target.backend {
            BackendId::Proxmox => TunnelRefusal::NoCtAddress {
                id: container.id.clone(),
                host: host.clone(),
            },
            _ => TunnelRefusal::Unroutable {
                reference: target.reference.clone(),
                host: host.clone(),
            },
        })?;
        Ok(Tunnel {
            forward: Forward {
                kind: Kind::Local,
                destination: Some(destination),
                ..tunnel.forward.clone()
            },
            ..tunnel.clone()
        })
    }

    /// The listening ports of host `alias`, or why they could not be read, in one line.
    fn host_ports(
        &self,
        alias: &str,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Vec<PortSuggestion>, String> {
        let alias = self
            .connectable_alias(alias)
            .map_err(|error| error.to_string())?;
        let output = self
            .openssh
            .run_remote(&alias, HOST_PORTS_SCRIPT, true, cancelled)
            .map_err(|error| error.to_string())?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if output.status.code() == Some(255) && stdout.trim().is_empty() {
            return Err(first_line(
                &self
                    .explain_connection(&alias, &output.stderr, true)
                    .map(|explanation| explanation.headline)
                    .unwrap_or_else(|| last_line(&output.stderr, "ssh failed")),
            ));
        }
        let listeners = parse_listeners(&stdout);
        if listeners.is_empty() && !output.status.success() {
            return Err("neither ss nor netstat answered".into());
        }
        Ok(suggested_ports(&listeners)
            .into_iter()
            .map(|(port, loopback)| PortSuggestion {
                port,
                note: loopback.then(|| "loopback only".into()),
            })
            .collect())
    }

    /// Where `container` can be reached from `target`, asked with the listing's transport;
    /// ssh never prompts.
    fn container_reach(
        &self,
        target: &Target,
        backend: BackendId,
        container: &Container,
        via: Option<Via>,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Reach, ContainerError> {
        let target = self.container_target(target)?;
        backend::reach(backend, &target, container, via, &mut |words| {
            self.capture(&target, words, true, cancelled, None)
                .map_err(|error| match error {
                    ContainerError::App(AppError::OpenSsh(OpenSshError::Cancelled)) => {
                        RunFailure::Cancelled(error)
                    }
                    error => RunFailure::Failed(error),
                })
        })
        .map_err(|failure| match failure {
            ListFailure::Run(error) => error,
            ListFailure::Issue(issue) => ContainerError::Backend(issue),
        })
    }

    /// The name of a new temporary forward from `base`: `base` itself, or when a forward of
    /// that name runs, the next free suffix. A failed forward of the chosen name is forgotten,
    /// so the new one replaces it.
    fn quick_name(&self, base: &str) -> Result<String, AppError> {
        let temporaries: Vec<TunnelReport> = self
            .tunnels()?
            .into_iter()
            .filter(|report| report.temporary)
            .collect();
        let name = unique_name(base, |candidate| {
            temporaries.iter().any(|report| {
                report.status.is_running() && report.tunnel.name.eq_ignore_ascii_case(candidate)
            })
        });
        if temporaries
            .iter()
            .any(|report| report.tunnel.name.eq_ignore_ascii_case(&name))
        {
            self.forget_run(&name);
        }
        Ok(name)
    }
}

/// The reference a container is saved under, the one that survives a redeploy: a CT's VMID,
/// a Compose container's `project/service`, any other container's name.
fn saved_reference(backend: BackendId, container: &Container) -> String {
    match backend {
        BackendId::Proxmox => container.id.clone(),
        _ => remember_key(container),
    }
}

/// The reference a temporary forward keeps for its container: as [`saved_reference`], except
/// that a numbered Compose replica keeps its number (`project/service#n`), so a restart goes
/// back to the same replica.
fn temporary_reference(backend: BackendId, container: &Container) -> String {
    match (backend, container.number) {
        (BackendId::Proxmox, _) | (_, None) => saved_reference(backend, container),
        (_, Some(number)) if container.project.is_some() && container.service.is_some() => {
            format!("{}#{number}", remember_key(container))
        }
        _ => saved_reference(backend, container),
    }
}

/// Where port `remote` of a container is reached from its host: through the host port it is
/// published on (at the one address it is published on, else `localhost`), at `localhost` on
/// the host's network, else at the container's address.
fn destination(reach: &Reach, remote: u16) -> Option<Destination> {
    let offer = reach.ports.iter().find(|offer| offer.port == remote);
    let published = offer.and_then(|offer| Some((offer.published?, &offer.published_address)));
    let (host, port) = match published {
        Some((published, Some(address))) => (address.clone(), published),
        Some((published, None)) => ("localhost".to_owned(), published),
        None if reach.host_network => ("localhost".to_owned(), remote),
        None => (reach.address.clone()?, remote),
    };
    Some(Destination { host, port })
}

/// The ports of `reach` this machine reaches directly, with their addresses: published ports,
/// and when `linux`, the others at the container's address (or `127.0.0.1` on the host's
/// network). A port listening on the container's loopback only is reached by nothing else.
fn reachable_here(reach: &Reach, linux: bool) -> Vec<(u16, String)> {
    reach
        .ports
        .iter()
        .filter_map(|offer| {
            let address = match (offer.published, &offer.published_address) {
                (Some(published), Some(address)) => socket(address, published),
                (Some(published), None) => format!("127.0.0.1:{published}"),
                (None, _) if !linux || offer.loopback_only => return None,
                (None, _) if reach.host_network => format!("127.0.0.1:{}", offer.port),
                (None, _) => socket(reach.address.as_ref()?, offer.port),
            };
            Some((offer.port, address))
        })
        .collect()
}

/// `address:port`, with an IPv6 address in brackets.
fn socket(address: &str, port: u16) -> String {
    if address.contains(':') {
        format!("[{address}]:{port}")
    } else {
        format!("{address}:{port}")
    }
}

/// A container error in one line, for a suggestion's `unavailable` or a refusal.
fn container_reason(error: &ContainerError) -> String {
    first_line(&match error {
        ContainerError::Ssh {
            explanation: Some(explanation),
            ..
        } => explanation.headline.clone(),
        ContainerError::Ssh { stderr, alias, .. } => {
            last_line(stderr.as_bytes(), &format!("ssh to {alias} failed"))
        }
        error => error.to_string(),
    })
}

/// The first line of `text`, trimmed.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_owned()
}

/// The last non-empty line of `bytes`, or `fallback`.
fn last_line(bytes: &[u8], fallback: &str) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::{
        backend::PortOffer,
        domain::host::Host,
        domain::managed::{ConfigPaths, ManagedStore},
        domain::openssh::OpenSsh,
        domain::tunnel::runtime::{Record, RuntimeDir},
        test_executable::write_executable,
    };

    /// The `ss -Hltn` sample of the ports parser's tests.
    const SS: &str = "LISTEN 0 4096 0.0.0.0:22 0.0.0.0:*\n\
LISTEN 0 511 [::]:80 [::]:*\n\
LISTEN 0 128 *:8080 *:*\n\
LISTEN 0 244 127.0.0.1:5432 0.0.0.0:*\n\
LISTEN 0 244 [::1]:5432 [::]:*\n";

    /// `docker inspect` of `web`: on a bridge network at 172.18.0.4, exposing 80 and 443.
    const INSPECT: &str =
        r#"{"bridge":{"IPAddress":"172.18.0.4"}}|{"80/tcp":{},"443/tcp":{}}|default"#;

    /// An app with the host `prod` whose `ssh` is a fake: `-G` answers like OpenSSH, a tunnel
    /// start (`-f`) logs its arguments to `starts` and leaves a background stand-in that
    /// carries them, as ssh's background process does (and when `refuse` exists, logs a
    /// refused channel as ssh does when nothing answers); any other call logs its remote
    /// command to `remote` and sources `behaviour` when it exists.
    struct Fixture {
        dir: TempDir,
        app: App,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let ssh = base.join("ssh");
        write_executable(
            &ssh,
            &format!(
                r#"#!/bin/sh
if [ "$1" = -G ]; then printf 'hostname x\nport 22\n'; exit 0; fi
if [ "$1" = -f ]; then
  printf '%s\n' "$*" >> '{starts}'
  if [ -f '{refuse}' ]; then
    prev=
    for arg do
      [ "$prev" = -E ] && echo 'channel 2: open failed: connect failed: Connection refused' >> "$arg"
      prev=$arg
    done
  fi
  setsid sh -c 'sleep 60; exit 0' sh "$@" </dev/null >/dev/null 2>&1 &
  exit 0
fi
for a; do last=$a; done
printf '%s\n' "$last" >> '{remote}'
if [ -f '{behaviour}' ]; then . '{behaviour}'; fi
exit 0
"#,
                starts = base.join("starts").display(),
                remote = base.join("remote").display(),
                behaviour = base.join("behaviour").display(),
                refuse = base.join("refuse").display(),
            ),
        );
        fs::create_dir_all(base.join("home/.ssh")).unwrap();
        let app = App::new(
            ManagedStore::new(ConfigPaths::from_home(base.join("home"))),
            OpenSsh::new(ssh, "terminal".into()),
        )
        .with_runtime_dir(RuntimeDir::at(base.join("run")));
        app.initialize(true).unwrap();
        app.add(Host::new("prod", "x", None, None, None, None).unwrap())
            .unwrap();
        Fixture { dir, app }
    }

    impl Fixture {
        fn behave(&self, script: &str) {
            fs::write(self.dir.path().join("behaviour"), script).unwrap();
        }

        /// Leaves the record of temporary forward `name` to `prod`, failed.
        fn failed(&self, name: &str, forward: &str) {
            let path = self.app.runtime.record(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            Record::Failed {
                reason: "the network dropped".into(),
                exit: Some(255),
                finished: 1,
                host: Some("prod".into()),
                forward: Some(forward.into()),
                reaches: Some("prod".into()),
                container: None,
            }
            .write(&path)
            .unwrap();
        }

        /// Makes the fake host's docker list `ps` (one line per container) and inspect every
        /// container as `inspect`; either can change between calls.
        fn docker(&self, ps: &[String], inspect: &str) {
            let base = self.dir.path();
            fs::write(base.join("ps"), ps.join("\n") + "\n").unwrap();
            fs::write(base.join("inspect"), format!("{inspect}\n")).unwrap();
            self.behave(&format!(
                "case \"$last\" in\n  *'ps --no-trunc'*) cat '{}' ;;\n  *inspect*) cat '{}' ;;\nesac\n",
                base.join("ps").display(),
                base.join("inspect").display()
            ));
        }

        fn read(&self, name: &str) -> String {
            fs::read_to_string(self.dir.path().join(name)).unwrap_or_default()
        }
    }

    impl Drop for Fixture {
        /// Stops every tunnel, so no stand-in outlives the test.
        fn drop(&mut self) {
            for name in self.app.running_tunnels().unwrap_or_default() {
                let _ = self.app.stop_tunnel(&name);
            }
        }
    }

    fn container(name: &str, ports: &str) -> Container {
        Container {
            id: format!("{name}-id"),
            name: name.into(),
            image: "nginx".into(),
            command: String::new(),
            running_for: String::new(),
            status: "Up".into(),
            ports: ports.into(),
            networks: String::new(),
            mounts: String::new(),
            project: None,
            service: None,
            number: None,
            working_dir: None,
        }
    }

    /// A `docker ps` line of a running container, of a Compose service when `compose` gives
    /// its project, service and replica number.
    fn ps_line(name: &str, compose: Option<(&str, &str, u32)>) -> String {
        let (project, service, number) = compose.map_or(
            (String::new(), String::new(), String::new()),
            |(p, s, n)| (p.to_owned(), s.to_owned(), n.to_string()),
        );
        format!(
            r#"{{"id":"{name}-id","name":"{name}","image":"nginx","command":"","running_for":"","status":"Up","ports":"","networks":"","mounts":"","project":"{project}","service":"{service}","number":"{number}","working_dir":""}}"#
        )
    }

    /// `docker inspect` of a container on a bridge network at `address`, exposing 80.
    fn inspect_at(address: &str) -> String {
        format!(r#"{{"bridge":{{"IPAddress":"{address}"}}}}|{{"80/tcp":{{}}}}|default"#)
    }

    /// Saved container tunnel `api` on `prod`, from `127.0.0.1:<local>` to port 80 of
    /// `reference` (docker).
    fn add_container_tunnel(app: &App, reference: &str, local: u16) {
        let listen = Forward::parse(Kind::Dynamic, &format!("127.0.0.1:{local}")).unwrap();
        let target = ContainerTarget {
            backend: BackendId::Docker,
            reference: reference.into(),
            port: 80,
        };
        app.add_tunnel(Tunnel::to_container("api", "prod", listen, target).unwrap())
            .unwrap();
    }

    fn docker_web() -> ForwardTarget {
        ForwardTarget::Container {
            target: Target::Host("prod".into()),
            backend: BackendId::Docker,
            container: container("web", "0.0.0.0:8080->80/tcp"),
            via: None,
        }
    }

    /// A port nobody uses: bound, read, released.
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn suggestion(port: u16, note: Option<&str>) -> PortSuggestion {
        PortSuggestion {
            port,
            note: note.map(str::to_owned),
        }
    }

    #[test]
    fn labels_and_short_names() {
        let host = ForwardTarget::Host("prod".into());
        assert_eq!((host.label(), host.short()), ("prod".into(), "prod".into()));
        let web = docker_web();
        assert_eq!(
            (web.label(), web.short()),
            ("web on prod".into(), "web".into())
        );
        let mut service = container("shop-web-1", "");
        service.project = Some("shop".into());
        service.service = Some("web".into());
        service.number = Some(1);
        let compose = ForwardTarget::Container {
            target: Target::Host("prod".into()),
            backend: BackendId::Docker,
            container: service,
            via: None,
        };
        assert_eq!(compose.short(), "web");
        let mut ct = container("db", "");
        ct.id = "101".into();
        let ct = ForwardTarget::Container {
            target: Target::Host("pve".into()),
            backend: BackendId::Proxmox,
            container: ct,
            via: None,
        };
        assert_eq!(
            (ct.label(), ct.short()),
            ("CT 101 on pve".into(), "101".into())
        );
    }

    #[test]
    fn a_host_suggests_its_listening_ports() {
        let fixture = fixture();
        fixture.behave(&format!("printf '{}'\n", SS.replace('\n', "\\n")));

        let suggestions = fixture
            .app
            .forward_suggestions(&ForwardTarget::Host("prod".into()), None);

        assert_eq!(
            suggestions,
            Suggestions {
                ports: vec![
                    suggestion(80, None),
                    suggestion(5432, Some("loopback only")),
                    suggestion(8080, None),
                ],
                reach: None,
                unavailable: None,
            }
        );
        assert!(fixture.read("remote").contains("ss -Hltn"));
    }

    #[test]
    fn a_host_that_does_not_answer_has_no_suggestions_and_says_why() {
        let fixture = fixture();
        fixture.behave("echo 'ssh: connect to host x port 22: Connection refused' >&2\nexit 255\n");

        let suggestions = fixture
            .app
            .forward_suggestions(&ForwardTarget::Host("prod".into()), None);

        assert!(suggestions.ports.is_empty());
        let why = suggestions.unavailable.expect("a reason");
        assert!(
            why.starts_with("could not list the ports of prod: "),
            "{why}"
        );
        assert!(!why.contains('\n'), "{why}");
    }

    #[test]
    fn a_container_suggests_its_published_then_exposed_ports() {
        let fixture = fixture();
        fixture.behave(&format!("echo '{INSPECT}'\n"));

        let suggestions = fixture.app.forward_suggestions(&docker_web(), None);

        assert_eq!(
            suggestions.ports,
            vec![
                suggestion(80, Some("published 8080")),
                suggestion(443, None)
            ]
        );
        let reach = suggestions.reach.expect("the container's reach");
        assert_eq!(reach.address.as_deref(), Some("172.18.0.4"));
        assert_eq!(suggestions.unavailable, None);
        assert!(fixture.read("remote").contains("inspect"));
    }

    #[test]
    fn a_container_whose_inspect_fails_says_why() {
        let fixture = fixture();
        fixture.behave("echo 'Error: No such object: web-id' >&2\nexit 1\n");

        let suggestions = fixture.app.forward_suggestions(&docker_web(), None);

        assert!(suggestions.ports.is_empty());
        assert_eq!(suggestions.reach, None);
        let why = suggestions.unavailable.expect("a reason");
        assert!(
            why.starts_with("could not list the ports of web on prod: "),
            "{why}"
        );
        assert!(!why.contains('\n'), "{why}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_published_port_is_reached_through_the_host() {
        let fixture = fixture();
        fixture.behave(&format!("echo '{INSPECT}'\n"));
        let local = free_port();

        let (tunnel, outcome) = fixture
            .app
            .quick_forward(&docker_web(), None, 80, Some(local), None, false)
            .unwrap();

        assert!(
            matches!(outcome, StartOutcome::Started { .. }),
            "{outcome:?}"
        );
        assert_eq!(tunnel.name, "_web-80");
        assert_eq!(tunnel.host, "prod");
        assert!(
            fixture
                .read("starts")
                .contains(&format!("-L 127.0.0.1:{local}:localhost:8080 ")),
            "{}",
            fixture.read("starts")
        );
        let report = fixture.app.tunnel("_web-80").unwrap();
        assert!(report.temporary);
        assert_eq!(report.reaches, "web on prod");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_unpublished_port_is_reached_at_the_containers_address() {
        let fixture = fixture();
        let local = free_port();
        let reach = Reach {
            address: Some("172.18.0.4".into()),
            host_network: false,
            ports: vec![PortOffer {
                port: 443,
                published: None,
                published_address: None,
                loopback_only: false,
            }],
        };

        let (tunnel, _) = fixture
            .app
            .quick_forward(&docker_web(), Some(&reach), 443, Some(local), None, false)
            .unwrap();

        assert_eq!(tunnel.name, "_web-443");
        assert!(
            fixture
                .read("starts")
                .contains(&format!("-L 127.0.0.1:{local}:172.18.0.4:443 ")),
            "{}",
            fixture.read("starts")
        );
        // The reach was given: nothing asked the host again.
        assert_eq!(fixture.read("remote"), "");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_host_network_container_is_reached_at_localhost() {
        let fixture = fixture();
        let local = free_port();
        let reach = Reach {
            address: None,
            host_network: true,
            ports: Vec::new(),
        };

        fixture
            .app
            .quick_forward(&docker_web(), Some(&reach), 9000, Some(local), None, false)
            .unwrap();

        assert!(
            fixture
                .read("starts")
                .contains(&format!("-L 127.0.0.1:{local}:localhost:9000 "))
        );
    }

    #[test]
    fn rootless_podman_without_an_address_is_unreachable() {
        let fixture = fixture();
        fixture.behave("echo '{\"podman\":{\"IPAddress\":\"\"}}|{\"80/tcp\":{}}|slirp4netns'\n");
        let target = ForwardTarget::Container {
            target: Target::Host("prod".into()),
            backend: BackendId::Podman,
            container: container("web", ""),
            via: None,
        };

        let refused = fixture
            .app
            .quick_forward(&target, None, 80, Some(free_port()), None, false);

        match refused {
            Err(AppError::Tunnel(refusal @ TunnelRefusal::Unreachable { .. })) => assert_eq!(
                refusal.to_string(),
                "web on prod has no address the host can reach; publish the port"
            ),
            other => panic!("expected Unreachable, got {other:?}"),
        }
        assert_eq!(fixture.read("starts"), "");
    }

    #[test]
    fn a_ct_without_an_address_says_so_rather_than_to_publish() {
        let fixture = fixture();
        fixture.behave("printf -- '---\\nLISTEN 0 511 0.0.0.0:80 0.0.0.0:*\\n'\n");
        let target = ForwardTarget::Container {
            target: Target::Host("prod".into()),
            backend: BackendId::Proxmox,
            container: container("101", ""),
            via: None,
        };

        let refused = fixture
            .app
            .quick_forward(&target, None, 80, Some(free_port()), None, false);

        match refused {
            Err(AppError::Tunnel(refusal @ TunnelRefusal::NoCtAddress { .. })) => assert_eq!(
                refusal.to_string(),
                "could not read the address of CT 101-id on prod"
            ),
            other => panic!("expected NoCtAddress, got {other:?}"),
        }
        assert_eq!(fixture.read("starts"), "");
    }

    #[test]
    fn a_local_port_of_zero_is_refused() {
        let fixture = fixture();
        let refused = fixture.app.quick_forward(
            &ForwardTarget::Host("prod".into()),
            None,
            80,
            Some(0),
            None,
            false,
        );
        assert!(
            matches!(refused, Err(AppError::Tunnel(TunnelRefusal::NoLocalPort))),
            "{refused:?}"
        );
        assert_eq!(fixture.read("starts"), "");
    }

    #[test]
    fn a_container_on_this_machine_gets_no_forward() {
        let fixture = fixture();
        let target = ForwardTarget::Container {
            target: Target::Local,
            backend: BackendId::Docker,
            container: container("web", ""),
            via: None,
        };

        let refused = fixture
            .app
            .quick_forward(&target, None, 80, None, None, false);

        assert!(
            matches!(refused, Err(AppError::Tunnel(TunnelRefusal::LocalTarget))),
            "{refused:?}"
        );
    }

    #[test]
    fn port_zero_is_refused() {
        let fixture = fixture();

        let refused = fixture.app.quick_forward(
            &ForwardTarget::Host("prod".into()),
            None,
            0,
            None,
            None,
            false,
        );

        assert!(
            matches!(refused, Err(AppError::Tunnel(TunnelRefusal::NoPort))),
            "{refused:?}"
        );
        assert_eq!(fixture.read("starts"), "");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_same_running_target_again_gets_the_next_suffix() {
        let fixture = fixture();
        let host = ForwardTarget::Host("prod".into());

        let (first, _) = fixture
            .app
            .quick_forward(&host, None, 5432, Some(free_port()), None, false)
            .unwrap();
        let (second, outcome) = fixture
            .app
            .quick_forward(&host, None, 5432, Some(free_port()), None, false)
            .unwrap();

        assert_eq!(first.name, "_prod-5432");
        assert_eq!(second.name, "_prod-5432-2");
        assert!(
            matches!(outcome, StartOutcome::Started { .. }),
            "{outcome:?}"
        );
        assert!(second.forward.spec().ends_with(":localhost:5432"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_forward_of_the_same_target_is_replaced() {
        let fixture = fixture();
        fixture.failed("_prod-5432", "-L 127.0.0.1:1:localhost:5432");

        let (tunnel, outcome) = fixture
            .app
            .quick_forward(
                &ForwardTarget::Host("prod".into()),
                None,
                5432,
                Some(free_port()),
                None,
                false,
            )
            .unwrap();

        assert_eq!(tunnel.name, "_prod-5432");
        assert!(
            matches!(outcome, StartOutcome::Started { .. }),
            "{outcome:?}"
        );
        let temporaries: Vec<_> = fixture
            .app
            .tunnels()
            .unwrap()
            .into_iter()
            .filter(|report| report.temporary)
            .collect();
        assert_eq!(temporaries.len(), 1, "{temporaries:?}");
        assert!(temporaries[0].status.is_running());
    }

    #[test]
    fn this_machine_shows_published_ports_and_on_linux_the_address() {
        let fixture = fixture();
        let reach = Reach {
            address: Some("172.18.0.4".into()),
            host_network: false,
            ports: vec![
                PortOffer {
                    port: 80,
                    published: Some(8080),
                    published_address: None,
                    loopback_only: false,
                },
                PortOffer {
                    port: 443,
                    published: None,
                    published_address: None,
                    loopback_only: false,
                },
            ],
        };

        let addresses = fixture
            .app
            .local_addresses(&container("web", "0.0.0.0:8080->80/tcp"), &reach);

        let mut expected = vec![(80, "127.0.0.1:8080".to_string())];
        if cfg!(target_os = "linux") {
            expected.push((443, "172.18.0.4:443".into()));
        }
        assert_eq!(addresses, expected);
    }

    #[test]
    fn a_port_published_on_one_address_is_reached_there() {
        let offer = |published_address: Option<&str>| Reach {
            address: Some("172.18.0.4".into()),
            host_network: false,
            ports: vec![PortOffer {
                port: 80,
                published: Some(8080),
                published_address: published_address.map(String::from),
                loopback_only: false,
            }],
        };
        let at = |reach: &Reach| destination(reach, 80).map(|d| (d.host, d.port));

        assert_eq!(
            at(&offer(Some("192.168.1.10"))),
            Some(("192.168.1.10".into(), 8080))
        );
        assert_eq!(at(&offer(None)), Some(("localhost".into(), 8080)));
        assert_eq!(
            reachable_here(&offer(Some("192.168.1.10")), true),
            vec![(80, "192.168.1.10:8080".to_string())]
        );
        assert_eq!(
            reachable_here(&offer(Some("fd00::5")), false),
            vec![(80, "[fd00::5]:8080".to_string())]
        );
    }

    #[test]
    fn a_ports_column_decides_where_a_published_port_is_reached() {
        let fixture = fixture();
        fixture.behave(&format!("echo '{INSPECT}'\n"));
        for (ports, expected) in [
            ("192.168.1.10:8080->80/tcp", "192.168.1.10"),
            ("[::]:8080->80/tcp", "localhost"),
            ("0.0.0.0:8080->80/tcp", "localhost"),
        ] {
            let target = ForwardTarget::Container {
                target: Target::Host("prod".into()),
                backend: BackendId::Docker,
                container: container("web", ports),
                via: None,
            };
            let reach = fixture
                .app
                .forward_suggestions(&target, None)
                .reach
                .expect("the container's reach");
            let found = destination(&reach, 80).map(|d| (d.host, d.port));
            assert_eq!(found, Some((expected.to_string(), 8080)), "{ports}");
        }
    }

    #[test]
    fn elsewhere_only_published_ports_are_reachable_here() {
        let reach = Reach {
            address: Some("172.18.0.4".into()),
            host_network: false,
            ports: vec![
                PortOffer {
                    port: 80,
                    published: Some(8080),
                    published_address: None,
                    loopback_only: false,
                },
                PortOffer {
                    port: 443,
                    published: None,
                    published_address: None,
                    loopback_only: false,
                },
                PortOffer {
                    port: 6379,
                    published: None,
                    published_address: None,
                    loopback_only: true,
                },
            ],
        };

        assert_eq!(
            reachable_here(&reach, false),
            vec![(80, "127.0.0.1:8080".to_string())]
        );
        assert_eq!(
            reachable_here(&reach, true),
            vec![
                (80, "127.0.0.1:8080".to_string()),
                (443, "172.18.0.4:443".to_string())
            ]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn saving_a_running_forward_restarts_it_under_its_new_name() {
        let fixture = fixture();
        let local = free_port();
        let (temporary, _) = fixture
            .app
            .quick_forward(
                &ForwardTarget::Host("prod".into()),
                None,
                5432,
                Some(local),
                None,
                false,
            )
            .unwrap();

        let saved = fixture
            .app
            .save_tunnel(&temporary.name, "prod-5432", None)
            .unwrap();

        assert_eq!(saved.tunnel.name, "prod-5432");
        assert_eq!(saved.tunnel.host, "prod");
        assert_eq!(saved.tunnel.forward, temporary.forward);
        assert_eq!(saved.note, None);
        assert!(
            matches!(saved.restarted, Some(StartOutcome::Started { .. })),
            "{:?}",
            saved.restarted
        );
        let reports = fixture.app.tunnels().unwrap();
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].tunnel.name, "prod-5432");
        assert!(!reports[0].temporary);
        assert!(reports[0].status.is_running(), "{:?}", reports[0].status);
        assert!(!fixture.app.runtime.record(&temporary.name).exists());
        assert!(!fixture.app.runtime.log(&temporary.name).exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_save_whose_restart_is_refused_stays_saved_and_shows_the_failure() {
        let fixture = fixture();
        let local = free_port();
        let (temporary, _) = fixture
            .app
            .quick_forward(
                &ForwardTarget::Host("prod".into()),
                None,
                5432,
                Some(local),
                None,
                false,
            )
            .unwrap();
        // Something else takes the port (the stand-in does not hold it): the restart is refused.
        let _busy = std::net::TcpListener::bind(("127.0.0.1", local)).unwrap();

        let saved = fixture
            .app
            .save_tunnel(&temporary.name, "prod-5432", None)
            .unwrap();

        match &saved.restarted {
            Some(StartOutcome::Failed { reason, .. }) => {
                assert!(reason.contains(&local.to_string()), "{reason}");
                assert!(!reason.contains('\n'), "{reason}");
            }
            other => panic!("expected a failed restart, got {other:?}"),
        }
        let reports = fixture.app.tunnels().unwrap();
        assert!(
            reports
                .iter()
                .any(|report| report.tunnel.name == "prod-5432" && !report.temporary),
            "{reports:?}"
        );
        assert!(
            reports.iter().all(|report| !report.temporary),
            "{reports:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_cancelled_restart_leaves_the_forward_saved() {
        let fixture = fixture();
        let (temporary, _) = fixture
            .app
            .quick_forward(
                &ForwardTarget::Host("prod".into()),
                None,
                5432,
                Some(free_port()),
                None,
                false,
            )
            .unwrap();
        let cancelled = AtomicBool::new(true);

        let saved = fixture
            .app
            .save_tunnel(&temporary.name, "prod-5432", Some(&cancelled))
            .unwrap();

        assert!(saved.cancelled);
        assert_eq!(saved.restarted, None);
        let reports = fixture.app.tunnels().unwrap();
        assert!(
            reports
                .iter()
                .any(|report| report.tunnel.name == "prod-5432" && !report.temporary),
            "{reports:?}"
        );
        assert!(!fixture.app.runtime.record(&temporary.name).exists());
    }

    #[test]
    fn a_failed_forward_is_saved_without_starting() {
        let fixture = fixture();
        fixture.failed("_prod-5432", "-L 127.0.0.1:15432:localhost:5432");

        let saved = fixture
            .app
            .save_tunnel("_prod-5432", "prod-5432", None)
            .unwrap();

        assert_eq!(saved.restarted, None);
        assert_eq!(
            saved.tunnel.forward.spec(),
            "127.0.0.1:15432:localhost:5432"
        );
        assert!(!fixture.app.runtime.record("_prod-5432").exists());
        let reports = fixture.app.tunnels().unwrap();
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert!(!reports[0].temporary);
    }

    #[test]
    fn only_a_temporary_forward_is_saved() {
        let fixture = fixture();

        let refused = fixture.app.save_tunnel("socks", "other", None);

        assert!(
            matches!(
                refused,
                Err(AppError::Tunnel(TunnelRefusal::NotTemporary(_)))
            ),
            "{refused:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_temporary_forward_to_a_replica_restarts_at_it_but_saves_the_service() {
        let fixture = fixture();
        fixture.docker(
            &[
                ps_line("shop-api-1", Some(("shop", "api", 1))),
                ps_line("shop-api-2", Some(("shop", "api", 2))),
            ],
            &inspect_at("172.18.0.4"),
        );
        let mut replica = container("shop-api-2", "");
        replica.project = Some("shop".into());
        replica.service = Some("api".into());
        replica.number = Some(2);
        let target = ForwardTarget::Container {
            target: Target::Host("prod".into()),
            backend: BackendId::Docker,
            container: replica,
            via: None,
        };
        let (temporary, _) = fixture
            .app
            .quick_forward(&target, None, 80, Some(free_port()), None, false)
            .unwrap();

        let kept = fixture.app.tunnel(&temporary.name).unwrap();
        assert_eq!(kept.tunnel.container.unwrap().reference, "shop/api#2");
        assert!(
            fixture.read("remote").contains("shop-api-2-id"),
            "{}",
            fixture.read("remote")
        );

        let saved = fixture
            .app
            .save_tunnel(&temporary.name, "api", None)
            .unwrap();
        assert_eq!(saved.tunnel.container.unwrap().reference, "shop/api");
        assert_eq!(
            temporary_reference(BackendId::Docker, &container("web", "")),
            "web"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_saved_container_tunnel_starts_at_the_current_address_and_follows_it() {
        let fixture = fixture();
        fixture.docker(&[ps_line("web", None)], &inspect_at("172.18.0.4"));
        let local = free_port();
        add_container_tunnel(&fixture.app, "web", local);

        let outcome = fixture.app.start_tunnel("api", None, false).unwrap();
        assert!(
            matches!(outcome, StartOutcome::Started { warning: None, .. }),
            "{outcome:?}"
        );
        assert!(
            fixture
                .read("starts")
                .contains(&format!("-L 127.0.0.1:{local}:172.18.0.4:80 ")),
            "{}",
            fixture.read("starts")
        );
        let report = fixture.app.tunnel("api").unwrap();
        assert_eq!(report.reaches, "web on prod");
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        // A saved tunnel's target is read from the file, not from its record.
        let record = fs::read_to_string(fixture.app.runtime.record("api")).unwrap();
        assert!(!record.contains("container "), "{record}");

        // The container is recreated at another address: the next start follows it.
        fixture.app.stop_tunnel("api").unwrap();
        fixture.docker(&[ps_line("web", None)], &inspect_at("172.18.0.9"));
        fixture.app.start_tunnel("api", None, false).unwrap();
        assert!(
            fixture
                .read("starts")
                .contains(&format!("-L 127.0.0.1:{local}:172.18.0.9:80 ")),
            "{}",
            fixture.read("starts")
        );
        // The file still names the container, not an address.
        let file = fixture.app.tunnels.load().unwrap().render();
        assert!(
            file.contains(&format!("ContainerForward 127.0.0.1:{local} docker web 80")),
            "{file}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_compose_service_starts_against_its_lowest_replica() {
        let fixture = fixture();
        fixture.docker(
            &[
                ps_line("shop-api-2", Some(("shop", "api", 2))),
                ps_line("shop-api-1", Some(("shop", "api", 1))),
            ],
            &inspect_at("172.18.0.4"),
        );
        add_container_tunnel(&fixture.app, "shop/api", free_port());

        let outcome = fixture.app.start_tunnel("api", None, false).unwrap();

        assert!(
            matches!(outcome, StartOutcome::Started { .. }),
            "{outcome:?}"
        );
        assert!(
            fixture.read("remote").contains("shop-api-1-id"),
            "{}",
            fixture.read("remote")
        );
    }

    #[test]
    fn a_container_that_is_not_running_is_refused_and_leaves_no_record() {
        let fixture = fixture();
        fixture.docker(&[ps_line("db", None)], &inspect_at("172.18.0.4"));
        add_container_tunnel(&fixture.app, "web", free_port());

        let refused = fixture.app.start_tunnel("api", None, false);

        match refused {
            Err(AppError::Tunnel(refusal @ TunnelRefusal::NotRunning { .. })) => {
                assert_eq!(refusal.to_string(), "web is not running on prod (docker)")
            }
            other => panic!("expected NotRunning, got {other:?}"),
        }
        assert_eq!(fixture.read("starts"), "");
        assert!(!fixture.app.runtime.record("api").exists());
    }

    #[test]
    fn an_ambiguous_container_message_strips_escapes_from_names() {
        let refusal = TunnelRefusal::AmbiguousContainer {
            reference: "api".into(),
            host: "prod".into(),
            candidates: vec!["a\u{1b}[31mb".into(), "c".into()],
        };

        let message = refusal.to_string();

        assert!(!message.contains('\u{1b}'), "{message:?}");
        assert!(message.contains(", c"), "{message:?}");
    }

    #[test]
    fn a_service_of_several_projects_is_refused_with_their_names() {
        let fixture = fixture();
        fixture.docker(
            &[
                ps_line("shop-api-1", Some(("shop", "api", 1))),
                ps_line("blog-api-1", Some(("blog", "api", 1))),
            ],
            &inspect_at("172.18.0.4"),
        );
        add_container_tunnel(&fixture.app, "api", free_port());

        let refused = fixture.app.start_tunnel("api", None, false);

        match refused {
            Err(AppError::Tunnel(refusal @ TunnelRefusal::AmbiguousContainer { .. })) => {
                assert_eq!(
                    refusal.to_string(),
                    "api matches several containers on prod: blog-api-1, shop-api-1"
                )
            }
            other => panic!("expected AmbiguousContainer, got {other:?}"),
        }
        assert_eq!(fixture.read("starts"), "");
    }

    #[test]
    fn a_container_without_a_routable_address_is_refused() {
        let fixture = fixture();
        fixture.docker(&[ps_line("web", None)], &inspect_at(""));
        add_container_tunnel(&fixture.app, "web", free_port());

        let refused = fixture.app.start_tunnel("api", None, false);

        match refused {
            Err(AppError::Tunnel(refusal @ TunnelRefusal::Unroutable { .. })) => assert_eq!(
                refusal.to_string(),
                "web on prod has no address the host can route to; publish the port, or attach it to a network prod routes"
            ),
            other => panic!("expected Unroutable, got {other:?}"),
        }
        assert_eq!(fixture.read("starts"), "");
        assert!(!fixture.app.runtime.record("api").exists());
    }

    #[test]
    fn a_listing_that_fails_is_its_own_explained_failure() {
        let fixture = fixture();
        fixture.behave(
            "case \"$last\" in\n  *'ps --no-trunc'*) echo 'Error: something odd happened' >&2; exit 1 ;;\nesac\n",
        );
        add_container_tunnel(&fixture.app, "web", free_port());

        let refused = fixture.app.start_tunnel("api", None, false);

        match refused {
            Err(AppError::Container(error)) => {
                assert!(matches!(*error, ContainerError::Backend(_)), "{error:?}");
            }
            other => panic!("expected the listing's failure, got {other:?}"),
        }
        assert!(!fixture.read("remote").contains("inspect"));
        assert_eq!(fixture.read("starts"), "");
        assert!(!fixture.app.runtime.record("api").exists());
    }

    #[test]
    fn a_ct_tunnel_without_an_address_says_so() {
        let fixture = fixture();
        fixture.behave(
            "case \"$last\" in\n  *'pct exec'*) printf -- '---\\nLISTEN 0 511 0.0.0.0:80 0.0.0.0:*\\n' ;;\n  *'pct list'*) printf 'via=none\\nVMID       Status     Lock         Name\\n101        running                 db\\n' ;;\nesac\n",
        );
        let listen = Forward::parse(Kind::Dynamic, &format!("127.0.0.1:{}", free_port())).unwrap();
        let target = ContainerTarget {
            backend: BackendId::Proxmox,
            reference: "101".into(),
            port: 80,
        };
        fixture
            .app
            .add_tunnel(Tunnel::to_container("db", "prod", listen, target).unwrap())
            .unwrap();

        let refused = fixture.app.start_tunnel("db", None, false);

        match refused {
            Err(AppError::Tunnel(refusal @ TunnelRefusal::NoCtAddress { .. })) => assert_eq!(
                refusal.to_string(),
                "could not read the address of CT 101 on prod"
            ),
            other => panic!("expected NoCtAddress, got {other:?}"),
        }
        assert!(fixture.read("remote").contains("pct exec"));
        assert_eq!(fixture.read("starts"), "");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_container_that_refuses_the_test_connection_starts_with_a_warning() {
        let fixture = fixture();
        fixture.docker(&[ps_line("web", None)], &inspect_at("172.18.0.4"));
        fs::write(fixture.dir.path().join("refuse"), "").unwrap();
        add_container_tunnel(&fixture.app, "web", free_port());
        let warning = "started, but nothing answers on port 80 inside web; an app that listens only on 127.0.0.1 inside its container cannot be reached";

        let outcome = fixture.app.start_tunnel("api", None, false).unwrap();

        match outcome {
            StartOutcome::Started {
                warning: Some(text),
                ..
            } => assert_eq!(text, warning),
            other => panic!("expected a warning, got {other:?}"),
        }
        let report = fixture.app.tunnel("api").unwrap();
        assert_eq!(
            report.warnings,
            vec![crate::domain::tunnel::TunnelWarning::NothingAnswers {
                port: 80,
                reference: "web".into()
            }]
        );
        assert_eq!(report.warnings[0].to_string(), warning);
    }

    /// A quick forward to port 80 of Compose service `shop/api`, whose one replica is at
    /// 172.18.0.4.
    #[cfg(target_os = "linux")]
    fn quick_shop_api(fixture: &Fixture, local: u16) -> Tunnel {
        let replica = ps_line("shop-api-1", Some(("shop", "api", 1)));
        fixture.docker(std::slice::from_ref(&replica), &inspect_at("172.18.0.4"));
        let mut container = container("shop-api-1", "");
        container.project = Some("shop".into());
        container.service = Some("api".into());
        container.number = Some(1);
        let target = ForwardTarget::Container {
            target: Target::Host("prod".into()),
            backend: BackendId::Docker,
            container,
            via: None,
        };
        let (tunnel, outcome) = fixture
            .app
            .quick_forward(&target, None, 80, Some(local), None, false)
            .unwrap();
        assert!(
            matches!(outcome, StartOutcome::Started { .. }),
            "{outcome:?}"
        );
        tunnel
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_quick_container_forward_restarts_at_the_new_address() {
        let fixture = fixture();
        let local = free_port();
        let tunnel = quick_shop_api(&fixture, local);
        let record = fs::read_to_string(fixture.app.runtime.record(&tunnel.name)).unwrap();
        assert!(
            record.contains("container docker shop/api#1 80\n"),
            "{record}"
        );
        assert!(
            record.contains(&format!("forward -L 127.0.0.1:{local}:172.18.0.4:80\n")),
            "{record}"
        );

        // The ssh exits; the container is recreated elsewhere; the restart follows it.
        let report = fixture.app.tunnel(&tunnel.name).unwrap();
        let crate::domain::tunnel::TunnelStatus::Running { pid, .. } = report.status else {
            panic!("not running: {:?}", report.status);
        };
        std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while fixture
            .app
            .tunnel(&tunnel.name)
            .unwrap()
            .status
            .is_running()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the stand-in survived"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        fixture.docker(
            &[ps_line("shop-api-1", Some(("shop", "api", 1)))],
            &inspect_at("172.18.0.9"),
        );

        let outcome = fixture.app.start_tunnel(&tunnel.name, None, false).unwrap();

        assert!(
            matches!(outcome, StartOutcome::Started { .. }),
            "{outcome:?}"
        );
        assert!(
            fixture
                .read("starts")
                .contains(&format!("-L 127.0.0.1:{local}:172.18.0.9:80 ")),
            "{}",
            fixture.read("starts")
        );
        let record = fs::read_to_string(fixture.app.runtime.record(&tunnel.name)).unwrap();
        assert!(
            record.contains("container docker shop/api#1 80\n"),
            "{record}"
        );
        assert_eq!(
            fixture.app.tunnel(&tunnel.name).unwrap().reaches,
            "api on prod"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn saving_a_quick_container_forward_saves_the_container_by_name() {
        let fixture = fixture();
        let local = free_port();
        let tunnel = quick_shop_api(&fixture, local);

        let saved = fixture.app.save_tunnel(&tunnel.name, "api", None).unwrap();

        assert_eq!(saved.note, None);
        assert!(
            matches!(saved.restarted, Some(StartOutcome::Started { .. })),
            "{:?}",
            saved.restarted
        );
        let file = fixture.app.tunnels.load().unwrap().render();
        assert!(
            file.contains(&format!(
                "\nTunnel api\n    Host prod\n    ContainerForward 127.0.0.1:{local} docker shop/api 80\n"
            )),
            "{file}"
        );
        assert_eq!(
            fixture.app.tunnel("api").unwrap().reaches,
            "shop/api on prod"
        );
    }
}
