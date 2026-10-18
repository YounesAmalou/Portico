//! `portico connect` in a real terminal: OpenSSH takes the terminal over and
//! gives it back when the session ends.

use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

use crate::{
    client::{Client, RealHome},
    sshd::Sshd,
    terminal::Terminal,
};

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn connect_opens_a_real_session() {
    let real_home = RealHome::snapshot();
    let sshd = Sshd::start();
    let client = Client::new();
    client.init();
    client.add_host("box", &sshd, &sshd.client_key());
    client.trust("box");

    let mut terminal = Terminal::spawn(&client, &["connect", "box"]);
    terminal.wait_for("portico-e2e ready");
    // The session is the server's script, never the user's login shell.
    let before_banner = terminal.screen();
    assert!(
        !before_banner.contains("$ ") && !before_banner.contains("% "),
        "a shell prompt appeared:\n{before_banner}"
    );
    terminal.type_text("hello\r");
    terminal.wait_for("portico-e2e got hello");
    let status = terminal.wait_exit();
    assert!(status.success(), "{status:?}\n{}", terminal.screen());

    real_home.assert_unchanged();
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn dropping_a_terminal_reaps_its_child() {
    let sshd = Sshd::start();
    let client = Client::new();
    client.init();
    client.add_host("box", &sshd, &sshd.client_key());
    client.trust("box");

    let mut terminal = Terminal::spawn(&client, &["connect", "box"]);
    terminal.wait_for("portico-e2e ready");
    let pid = terminal.pid();
    drop(terminal);

    let stat = format!("/proc/{pid}/stat");
    let deadline = Instant::now() + Duration::from_secs(2);
    while Path::new(&stat).exists() {
        assert!(Instant::now() < deadline, "portico {pid} was not reaped");
        thread::sleep(Duration::from_millis(10));
    }
}
