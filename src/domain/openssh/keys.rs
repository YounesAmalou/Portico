use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    process::Command,
    sync::atomic::AtomicBool,
};

use super::connect::Watched;
use super::{OpenSsh, OpenSshError};
use crate::domain::probe::{Failure, Route, TEST_DEADLINE, classify, fetch_key_args};
use crate::platform::fs::{private, write_unique};

/// One line of `ssh-keygen -l`: `<bits> <fingerprint> <host> (<TYPE>)`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyFingerprint {
    pub host: String,
    pub fingerprint: String,
    pub key_type: String,
}

impl KeyFingerprint {
    /// The line that shows this key to the user. A hashed known-hosts name is unreadable, so the
    /// tested `alias` is named in its place.
    pub fn describe(&self, alias: &str) -> String {
        let host = if self.host.starts_with("|1|") {
            alias
        } else {
            &self.host
        };
        format!(
            "{host} presents an {} key with fingerprint {}",
            self.key_type, self.fingerprint
        )
    }
}

/// The known-hosts lines a server presented, with their fingerprints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresentedKey {
    pub lines: Vec<u8>,
    pub fingerprints: Vec<KeyFingerprint>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyFetch {
    Presented(PresentedKey),
    Failed(Failure),
}

impl OpenSsh {
    /// Connects to `alias` with every login method disabled, letting OpenSSH
    /// record the server's host key in a private temporary file, and returns
    /// that key with its fingerprints. Nothing is written to the user's own
    /// known-hosts files.
    pub(crate) fn fetch_host_key(
        &self,
        entry: Option<&Path>,
        alias: &str,
        route: Route<'_>,
        cancelled: &AtomicBool,
    ) -> Result<KeyFetch, OpenSshError> {
        let io_error = |source| OpenSshError::Io {
            program: self.ssh.clone(),
            source,
        };
        let known_hosts = write_unique(&std::env::temp_dir(), "portico-host-key", b"", 0o600)
            .map_err(io_error)?;
        let args = fetch_key_args(entry, known_hosts.path(), alias);
        // Every login method is off, so the run can only end or time out.
        let (stderr, deadline_reached) =
            match self.watch_ssh(args, route, cancelled, TEST_DEADLINE)? {
                Watched::Authenticated(_) => (String::new(), false),
                Watched::Ended {
                    stderr,
                    deadline_reached,
                } => (stderr, deadline_reached),
            };
        let lines = fs::read(known_hosts.path()).map_err(io_error)?;
        if lines.is_empty() {
            return Ok(KeyFetch::Failed(classify(
                &stderr,
                deadline_reached,
                route.jump(),
            )));
        }
        let output = self.checked_output(
            Command::new(&self.keygen)
                .arg("-l")
                .arg("-f")
                .arg(known_hosts.path()),
            Some(cancelled),
        )?;
        let fingerprints = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(parse_fingerprint)
            .collect::<Result<_, _>>()?;
        Ok(KeyFetch::Presented(PresentedKey {
            lines,
            fingerprints,
        }))
    }
}

fn parse_fingerprint(line: &str) -> Result<KeyFingerprint, OpenSshError> {
    let invalid = || OpenSshError::InvalidOutput(format!("invalid ssh-keygen line: {line}"));
    let mut fields = line.split_whitespace();
    let (_bits, fingerprint) = (fields.next(), fields.next().ok_or_else(invalid)?);
    let rest: Vec<&str> = fields.collect();
    let (key_type, host) = rest.split_last().ok_or_else(invalid)?;
    let key_type = key_type
        .strip_prefix('(')
        .and_then(|key_type| key_type.strip_suffix(')'))
        .ok_or_else(invalid)?;
    if host.is_empty() {
        return Err(invalid());
    }
    Ok(KeyFingerprint {
        host: host.join(" "),
        fingerprint: fingerprint.into(),
        key_type: key_type.into(),
    })
}

/// Appends `lines` to the known-hosts file at `path`, starting on a new line
/// and creating the file private to the user when it is missing.
pub(crate) fn append_known_hosts(path: &Path, lines: &[u8]) -> io::Result<()> {
    let mut file = private(
        OpenOptions::new().read(true).append(true).create(true),
        0o600,
    )
    .open(path)?;
    let mut contents = Vec::with_capacity(lines.len() + 1);
    if file.metadata()?.len() > 0 {
        let mut last = [0];
        file.seek(SeekFrom::End(-1))?;
        file.read_exact(&mut last)?;
        if last != *b"\n" {
            contents.push(b'\n');
        }
    }
    contents.extend_from_slice(lines);
    file.write_all(&contents)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, sync::atomic::AtomicBool};

    use super::*;
    use crate::domain::openssh::tests::{FakeProgram, fake_ssh, fixture_path};
    use crate::test_executable::write_executable;

    /// A fake ssh that records the temporary known-hosts path at `<dir>/args`,
    /// runs `body` with it in `$f`, and exits 255.
    fn fake_fetching_ssh(body: &str) -> FakeProgram {
        let fake = fake_ssh();
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in UserKnownHostsFile=*) f=\"${{a#UserKnownHostsFile=}}\";; esac; done\nprintf '%s' \"$f\" > '{}'\n{body}\nexit 255\n",
                fake.log.display()
            ),
        );
        fake
    }

    #[test]
    fn fetch_reads_the_key_written_to_the_temporary_file_and_removes_it() {
        let ssh = fake_fetching_ssh("printf '[h]:22 ssh-ed25519 AAAA\\n' > \"$f\"");
        let keygen = fake_ssh();
        write_executable(
            &keygen.path,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '256 SHA256:abc [h]:22 (ED25519)\\n'\n",
                keygen.log.display()
            ),
        );
        let openssh =
            OpenSsh::new(ssh.path(), "xdg-terminal-exec".into()).with_keygen(keygen.path());

        let result = openssh
            .fetch_host_key(None, "dev", Route::Direct, &AtomicBool::new(false))
            .unwrap();

        assert_eq!(
            result,
            KeyFetch::Presented(PresentedKey {
                lines: b"[h]:22 ssh-ed25519 AAAA\n".to_vec(),
                fingerprints: vec![KeyFingerprint {
                    host: "[h]:22".into(),
                    fingerprint: "SHA256:abc".into(),
                    key_type: "ED25519".into(),
                }],
            })
        );
        let temporary = fs::read_to_string(ssh.log()).unwrap();
        assert!(temporary.starts_with(&std::env::temp_dir().display().to_string()));
        assert_eq!(
            fs::read_to_string(keygen.log()).unwrap(),
            format!("-l\n-f\n{temporary}\n")
        );
        assert!(!Path::new(&temporary).exists());
    }

    #[test]
    fn fetch_with_nothing_written_reports_why() {
        let ssh = fake_fetching_ssh(&format!(
            "cat '{}' >&2",
            fixture_path("connection-refused").display()
        ));
        let openssh = OpenSsh::new(ssh.path(), "xdg-terminal-exec".into())
            .with_keygen("/nonexistent/ssh-keygen".into());

        assert_eq!(
            openssh
                .fetch_host_key(None, "dev", Route::Direct, &AtomicBool::new(false))
                .unwrap(),
            KeyFetch::Failed(Failure::ConnectionRefused)
        );
        assert!(!Path::new(&fs::read_to_string(ssh.log()).unwrap()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn append_adds_a_separating_newline_and_creates_missing_files_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("known_hosts");
        fs::write(&existing, "a b c").unwrap();
        append_known_hosts(&existing, b"x y z\n").unwrap();
        assert_eq!(fs::read_to_string(&existing).unwrap(), "a b c\nx y z\n");

        let missing = dir.path().join("new_hosts");
        append_known_hosts(&missing, b"x y z\n").unwrap();
        assert_eq!(fs::read(&missing).unwrap(), b"x y z\n");
        assert_eq!(
            fs::metadata(&missing).unwrap().permissions().mode() & 0o777,
            0o600
        );

        assert!(append_known_hosts(&dir.path().join("absent/known_hosts"), b"x y z\n").is_err());
    }
}
