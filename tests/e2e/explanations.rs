//! Each error explanation of `portico test`, produced by its real cause.

use crate::{
    client::{Client, RealHome, text},
    sshd::{Sshd, SshdOptions, closed_port, current_user},
};

/// Runs `portico test <alias>`, expects it to fail and returns its output.
fn failed_test(client: &Client, alias: &str) -> String {
    let output = client.run(&["test", alias]);
    let output_text = text(&output);
    assert_eq!(output.status.code(), Some(1), "{output_text}");
    output_text
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn changed_host_key_is_called_a_possible_attack() {
    let real_home = RealHome::snapshot();
    let mut sshd = Sshd::start();
    let client = Client::new();
    client.init();
    client.add_host("box", &sshd, &sshd.client_key());
    client.trust("box");
    sshd.replace_host_key();

    let output = failed_test(&client, "box");
    assert!(
        output.contains("the host key changed, which can mean an attack"),
        "{output}"
    );
    real_home.assert_unchanged();
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn closed_port_is_a_refused_connection() {
    let real_home = RealHome::snapshot();
    let sshd = Sshd::start();
    let client = Client::new();
    client.init();
    let port = closed_port().to_string();
    let user = current_user().0;
    let key = sshd.client_key();
    client.run_ok(&[
        "add",
        "shut",
        "-H",
        "127.0.0.1",
        "-p",
        &port,
        "-u",
        &user,
        "-i",
        key.to_str().unwrap(),
    ]);

    let output = failed_test(&client, "shut");
    assert!(
        output.contains("nothing accepted the connection"),
        "{output}"
    );
    assert!(output.contains(&format!("127.0.0.1:{port}")), "{output}");
    real_home.assert_unchanged();
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn password_only_server_says_the_test_cannot_type_a_password() {
    let real_home = RealHome::snapshot();
    let sshd = Sshd::start_with(SshdOptions {
        password_only: true,
    });
    let client = Client::new();
    client.init();
    client.add_host("pw", &sshd, &sshd.client_key());
    client.trust("pw");

    let output = failed_test(&client, "pw");
    assert!(
        output.contains("the server accepts only a password"),
        "{output}"
    );
    assert!(output.contains("`portico connect pw`"), "{output}");
    real_home.assert_unchanged();
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn failing_jump_host_names_the_jump() {
    let real_home = RealHome::snapshot();
    let sshd = Sshd::start();
    let client = Client::new();
    client.init();
    let closed = closed_port().to_string();
    let live = sshd.port().to_string();
    let user = current_user().0;
    let key = sshd.client_key();
    let key = key.to_str().unwrap();
    client.run_ok(&[
        "add",
        "gate",
        "-H",
        "127.0.0.1",
        "-p",
        &closed,
        "-u",
        &user,
        "-i",
        key,
    ]);
    client.run_ok(&[
        "add",
        "inner",
        "-H",
        "127.0.0.1",
        "-p",
        &live,
        "-u",
        &user,
        "-i",
        key,
        "-J",
        "gate",
    ]);

    let output = failed_test(&client, "inner");
    assert!(
        output.contains("jump host gate failed: nothing accepted the connection"),
        "{output}"
    );
    real_home.assert_unchanged();
}
