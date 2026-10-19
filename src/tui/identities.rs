//! Identities in the TUI: the section of the host details, and the identity inventory the
//! IdentityFile field suggests from, built on a worker thread because it resolves every host.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

use crate::{
    app::{App, key_files_in},
    identity::{self, Agent, Availability, HostIdentities, IdentityInventory, PublicKey},
    suggest::{Candidate, Origin},
};

type Built = Result<IdentityInventory, String>;

struct Request {
    generation: u64,
    cancelled: Arc<AtomicBool>,
}

/// Builds the identity inventory once per session, when a form first needs it.
pub(super) struct Indexer {
    requests: mpsc::Sender<Request>,
    results: mpsc::Receiver<(u64, Built)>,
    generation: u64,
    /// The running build's cancellation flag.
    running: Option<Arc<AtomicBool>>,
    /// Whether this generation was requested; `reset` starts a new one.
    requested: bool,
}

impl Indexer {
    pub(super) fn spawn<'scope>(
        scope: &'scope thread::Scope<'scope, '_>,
        app: &'scope App,
    ) -> Self {
        let (requests, receiver) = mpsc::channel();
        let (sender, results) = mpsc::channel();
        scope.spawn(move || index_worker(app, &receiver, &sender));
        Self {
            requests,
            results,
            generation: 0,
            running: None,
            requested: false,
        }
    }

    /// Starts building, unless this generation was already requested.
    pub(super) fn request(&mut self) {
        if self.requested {
            return;
        }
        self.requested = true;
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = Request {
            generation: self.generation,
            cancelled: Arc::clone(&cancelled),
        };
        if self.requests.send(request).is_ok() {
            self.running = Some(cancelled);
        }
    }

    /// Forgets the built inventory's generation, cancelling a running build, so the next request
    /// builds again (after a refresh).
    pub(super) fn reset(&mut self) {
        if let Some(cancelled) = self.running.take() {
            cancelled.store(true, Ordering::Relaxed);
        }
        self.generation += 1;
        self.requested = false;
    }

    pub(super) fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The current generation's inventory, if it has arrived.
    pub(super) fn try_finished(&mut self) -> Option<Built> {
        let mut current = None;
        while let Ok((generation, built)) = self.results.try_recv() {
            if generation == self.generation && self.running.take().is_some() {
                current = Some(built);
            }
        }
        current
    }
}

impl Drop for Indexer {
    fn drop(&mut self) {
        if let Some(cancelled) = self.running.take() {
            cancelled.store(true, Ordering::Relaxed);
        }
    }
}

fn index_worker(
    app: &App,
    requests: &mpsc::Receiver<Request>,
    results: &mpsc::Sender<(u64, Built)>,
) {
    while let Ok(request) = requests.recv() {
        if request.cancelled.load(Ordering::Relaxed) {
            continue;
        }
        let built = app
            .identity_inventory_cancellable(&request.cancelled)
            .map_err(|error| error.to_string());
        if request.cancelled.load(Ordering::Relaxed) {
            continue;
        }
        if results.send((request.generation, built)).is_err() {
            return;
        }
    }
}

/// The Identities section of the host details, as lines of `label  value` under the resolved
/// fields.
pub(super) fn detail_lines(identities: &HostIdentities, home: &Path) -> String {
    const WIDTH: usize = 13;
    let mut text = format!(
        "{:<WIDTH$}{}\n",
        "Agent",
        agent_line(&identities.agent, home)
    );
    let mut rows = identities
        .identities
        .iter()
        .map(|identity| {
            let mut value = format!(
                "{}  {} {}",
                crate::home::tilde_text(&identity.configured, home),
                identity.availability.symbol(),
                identity.availability.word()
            );
            if let Some(reason) = identity.availability.reason() {
                value.push_str(&format!(" ({reason})"));
            }
            if let Some(key) = &identity.public_key {
                value.push_str(&format!("  {}", key_label(key)));
            }
            value
        })
        .collect::<Vec<_>>();
    if identities.defaults {
        rows.push(if rows.is_empty() {
            "OpenSSH defaults: none present".into()
        } else {
            "other OpenSSH defaults: none present".into()
        });
    } else if rows.is_empty() {
        rows.push("none".into());
    }
    let label = if identities.defaults {
        "Identity"
    } else {
        "IdentityFile"
    };
    for (index, row) in rows.iter().enumerate() {
        let label = if index == 0 { label } else { "" };
        text.push_str(&format!("{label:<WIDTH$}{row}\n"));
    }
    if identities.identities_only {
        text.push_str(&format!("{:<WIDTH$}yes\n", "Only listed"));
    } else if identities.agent.keys().is_some() {
        let offered = identities
            .also_offered
            .iter()
            .map(key_label)
            .collect::<Vec<_>>();
        let value = if offered.is_empty() {
            "none".to_owned()
        } else {
            offered.join(", ")
        };
        text.push_str(&format!("{:<WIDTH$}{value}\n", "Also offered"));
    }
    if let Some(provider) = &identities.pkcs11_provider {
        text.push_str(&format!("{:<WIDTH$}{provider}\n", "PKCS#11"));
    }
    text
}

fn agent_line(agent: &Agent, home: &Path) -> String {
    match agent {
        Agent::Listed { provider, keys, .. } => format!(
            "{} · {}  {}",
            provider.name(),
            identity::count(keys.len(), "key"),
            agent
                .socket()
                .map(|socket| crate::home::tilde(socket, home))
                .unwrap_or_default()
        ),
        _ => agent.describe(home),
    }
}

/// The comment, or the shortened fingerprint of a key without one.
fn key_label(key: &PublicKey) -> String {
    if key.comment.is_empty() {
        identity::short_fingerprint(&key.fingerprint)
    } else {
        key.comment.clone()
    }
}

/// The IdentityFile suggestions: every file of the inventory, or, until it is built, the key
/// files in `~/.ssh`. Missing files come last.
pub(super) fn candidates(inventory: Option<&IdentityInventory>, home: &Path) -> Vec<Candidate> {
    let Some(inventory) = inventory else {
        if home.as_os_str().is_empty() {
            return Vec::new();
        }
        return key_files_in(&home.join(".ssh"))
            .into_iter()
            .map(|path| Candidate {
                origin: Origin::Note(if identity::is_public(&path) {
                    "public key".into()
                } else {
                    "key file".into()
                }),
                alias: crate::home::tilde(&path, home),
            })
            .collect();
    };
    let mut candidates: Vec<(bool, Candidate)> = inventory
        .files
        .iter()
        .map(|file| {
            let mut note = file.availability.word().to_owned();
            if file.availability == Availability::InAgent
                && let Some(key) = &file.public_key
                && let Some(holder) = inventory.agents.iter().find(|known| {
                    known.agent.keys().is_some_and(|keys| {
                        keys.iter().any(|held| held.fingerprint == key.fingerprint)
                    })
                })
                && let Agent::Listed { provider, .. } = &holder.agent
            {
                note.push_str(&format!(" · {}", provider.name()));
            }
            if !file.used_by.is_empty() {
                note.push_str(&format!(
                    " · used by {}",
                    identity::count(file.used_by.len(), "host")
                ));
            }
            let alias = match &file.path {
                Some(path) => crate::home::tilde(path, home),
                None => file.configured.clone(),
            };
            (
                matches!(file.availability, Availability::Missing(_)),
                Candidate {
                    alias,
                    origin: Origin::Note(note),
                },
            )
        })
        .collect();
    candidates.sort_by_key(|(missing, _)| *missing);
    candidates
        .into_iter()
        .map(|(_, candidate)| candidate)
        .collect()
}

/// `ranked` with missing files moved after the rest, keeping each group's order.
pub(super) fn missing_last(ranked: Vec<Candidate>) -> Vec<Candidate> {
    let (missing, present): (Vec<_>, Vec<_>) = ranked.into_iter().partition(
        |candidate| matches!(&candidate.origin, Origin::Note(note) if note.starts_with("missing")),
    );
    present.into_iter().chain(missing).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Identity, InventoryAgent, InventoryFile, MissingReason, Provider};

    fn key(fingerprint: &str, comment: &str) -> PublicKey {
        PublicKey {
            key_type: "ED25519".into(),
            bits: 256,
            fingerprint: fingerprint.into(),
            comment: comment.into(),
        }
    }

    fn one_password(keys: Vec<PublicKey>) -> Agent {
        Agent::Listed {
            socket: "/home/me/.1password/agent.sock".into(),
            provider: Provider::OnePassword,
            keys,
        }
    }

    #[test]
    fn detail_lines_show_each_identity_with_a_symbol_and_word() {
        let identities = HostIdentities {
            agent: one_password(vec![
                key("SHA256:gh", "GitHub"),
                key("SHA256:abcdefghijk", ""),
            ]),
            defaults: false,
            identities: vec![
                Identity {
                    configured: "~/.ssh/github.pub".into(),
                    path: None,
                    public_key: Some(key("SHA256:gh", "GitHub")),
                    availability: Availability::InAgent,
                },
                Identity {
                    configured: "/home/me/.ssh/id_work".into(),
                    path: None,
                    public_key: None,
                    availability: Availability::Missing(MissingReason::NoSuchFile),
                },
                Identity {
                    configured: "~/.ssh/%C".into(),
                    path: None,
                    public_key: None,
                    availability: Availability::Unchecked { token: "%C".into() },
                },
            ],
            identities_only: false,
            also_offered: vec![key("SHA256:abcdefghijk", "")],
            pkcs11_provider: None,
        };
        assert_eq!(
            detail_lines(&identities, Path::new("/home/me")),
            "Agent        1Password · 2 keys  ~/.1password/agent.sock\n\
             IdentityFile ~/.ssh/github.pub  ✓ in agent  GitHub\n\
             \x20            ~/.ssh/id_work  ✗ missing (no such file)\n\
             \x20            ~/.ssh/%C  ? unchecked (Portico does not expand %C)\n\
             Also offered SHA256:abcdefgh…\n"
        );
    }

    #[test]
    fn detail_lines_collapse_defaults_and_note_identities_only() {
        let identities = HostIdentities {
            agent: Agent::Disabled,
            defaults: true,
            identities: Vec::new(),
            identities_only: true,
            also_offered: Vec::new(),
            pkcs11_provider: Some("/p11.so".into()),
        };
        assert_eq!(
            detail_lines(&identities, Path::new("/home/me")),
            "Agent        none (IdentityAgent none)\n\
             Identity     OpenSSH defaults: none present\n\
             Only listed  yes\n\
             PKCS#11      /p11.so\n"
        );
    }

    #[test]
    fn candidates_name_availability_holder_and_users_with_missing_last() {
        let inventory = IdentityInventory {
            agents: vec![InventoryAgent {
                agent: one_password(vec![key("SHA256:gh", "GitHub")]),
                environment: true,
                used_by: Vec::new(),
            }],
            files: vec![
                InventoryFile {
                    configured: "~/.ssh/a_gone".into(),
                    path: Some("/home/me/.ssh/a_gone".into()),
                    public_key: None,
                    availability: Availability::Missing(MissingReason::NoSuchFile),
                    used_by: vec!["x".into()],
                },
                InventoryFile {
                    configured: "~/.ssh/github.pub".into(),
                    path: Some("/home/me/.ssh/github.pub".into()),
                    public_key: Some(key("SHA256:gh", "GitHub")),
                    availability: Availability::InAgent,
                    used_by: vec!["x".into(), "y".into()],
                },
                InventoryFile {
                    configured: "%d/.ssh/%C".into(),
                    path: None,
                    public_key: None,
                    availability: Availability::Unchecked { token: "%C".into() },
                    used_by: vec!["z".into()],
                },
            ],
            warnings: Vec::new(),
        };
        let candidates = candidates(Some(&inventory), Path::new("/home/me"));
        let shown: Vec<(&str, String)> = candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.alias.as_str(),
                    crate::suggest::origin_label(&candidate.origin, None),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                (
                    "~/.ssh/github.pub",
                    "in agent · 1Password · used by 2 hosts".to_owned()
                ),
                ("%d/.ssh/%C", "unchecked · used by 1 host".to_owned()),
                ("~/.ssh/a_gone", "missing · used by 1 host".to_owned()),
            ]
        );
        let ranked = missing_last(candidates);
        assert_eq!(ranked.last().unwrap().alias, "~/.ssh/a_gone");
    }

    #[test]
    fn candidates_before_the_inventory_come_from_the_ssh_directory() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        for name in ["pair", "pair.pub", "agent_only.pub", "pair-cert.pub"] {
            std::fs::write(ssh.join(name), "").unwrap();
        }
        let shown: Vec<(String, String)> = candidates(None, dir.path())
            .into_iter()
            .map(|candidate| {
                (
                    candidate.alias,
                    crate::suggest::origin_label(&candidate.origin, None),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("~/.ssh/agent_only.pub".to_owned(), "public key".to_owned()),
                ("~/.ssh/pair".to_owned(), "key file".to_owned()),
            ]
        );
    }
}
