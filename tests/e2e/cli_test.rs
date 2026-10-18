//! `portico test` against a real server: host key trust, jump hosts and the
//! failures a login can meet.

use std::{
    net::{Ipv4Addr, TcpStream},
    thread,
    time::{Duration, Instant},
};

use crate::{
    client::{Client, RealHome, text},
    sshd::{Sshd, current_user},
};

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn portico_test_against_a_real_sshd() {
    let real_home = RealHome::snapshot();
    let sshd = Sshd::start();
    let (user, _) = current_user();
    let port = sshd.port();
    let client = Client::new();
    client.init();
    client.add_host("real", &sshd, &sshd.client_key());
    client.add_host("locked", &sshd, &sshd.locked_key());
    // `realjump` reaches the same sshd through `real` as its jump host.
    client.append_config(&format!(
        "\nHost realjump\n  HostName 127.0.0.1\n  Port {port}\n  User {user}\n  \
         IdentityFile {key}\n  ProxyJump real\n",
        key = sshd.client_key().display(),
    ));

    // An unknown host key is reported with its fingerprint and trusted by nothing.
    let first = client.run(&["test", "real"]);
    let first_text = text(&first);
    assert_eq!(first.status.code(), Some(1), "{first_text}");
    let fingerprint = first_text
        .split_whitespace()
        .find(|word| word.starts_with("SHA256:"))
        .unwrap_or_else(|| panic!("no fingerprint in:\n{first_text}"))
        .trim_end_matches(['.', ',', ')'])
        .to_owned();
    assert!(!client.known_hosts().exists(), "nothing may be trusted yet");

    // Trusting exactly that fingerprint lets the login pass.
    let accepted = client.run(&["test", "real", "--accept-host-key", &fingerprint]);
    let accepted_text = text(&accepted);
    assert_eq!(accepted.status.code(), Some(0), "{accepted_text}");
    assert!(accepted_text.contains("authenticated"), "{accepted_text}");
    assert!(
        !std::fs::read_to_string(client.known_hosts())
            .unwrap()
            .trim()
            .is_empty(),
        "the host key was not recorded"
    );

    // A login through a jump host is recognised as soon as it happens, well
    // before the test's 15 second deadline.
    let started = Instant::now();
    let jumped = client.run(&["test", "realjump"]);
    let jumped_text = text(&jumped);
    assert_eq!(jumped.status.code(), Some(0), "{jumped_text}");
    assert!(
        jumped_text.contains(&format!(
            "authenticated as {user}@127.0.0.1:{port} via real (publickey)"
        )),
        "{jumped_text}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "the jump test took {:?}",
        started.elapsed()
    );

    // A key that needs a passphrase, with no agent to supply it, says so and
    // how to load it.
    let locked = client.run(&["test", "locked"]);
    let locked_text = text(&locked);
    assert_eq!(locked.status.code(), Some(1), "{locked_text}");
    assert!(
        locked_text.contains("the key needs a passphrase"),
        "{locked_text}"
    );
    assert!(
        locked_text.contains(&format!("`ssh-add {}`", sshd.locked_key().display())),
        "{locked_text}"
    );

    // A key the server no longer accepts is an authentication failure.
    sshd.revoke_all_keys();
    let refused = client.run(&["test", "real"]);
    let refused_text = text(&refused);
    assert_eq!(refused.status.code(), Some(1), "{refused_text}");
    assert!(
        refused_text.contains("the server refused the login"),
        "{refused_text}"
    );
    assert!(refused_text.contains("offered:"), "{refused_text}");
    assert!(
        refused_text.contains(&sshd.client_key().display().to_string()),
        "{refused_text}"
    );

    real_home.assert_unchanged();
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn parallel_servers_get_distinct_ports() {
    let servers: Vec<Sshd> = (0..8)
        .map(|_| thread::spawn(Sshd::start))
        .collect::<Vec<_>>()
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    let mut ports: Vec<u16> = servers.iter().map(Sshd::port).collect();
    for port in &ports {
        TcpStream::connect((Ipv4Addr::LOCALHOST, *port))
            .unwrap_or_else(|error| panic!("port {port} does not answer: {error}"));
    }
    ports.sort_unstable();
    ports.dedup();
    assert_eq!(ports.len(), servers.len(), "two servers share a port");
}
