//! A first run of the TUI: initialize, add a host through the form and
//! connect to it, answering OpenSSH's own host key prompt in the terminal.

use std::fs;

use crate::{
    client::{Client, RealHome},
    sshd::{Sshd, current_user},
    terminal::{Key, Terminal},
};

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn first_run_adds_and_connects() {
    let real_home = RealHome::snapshot();
    let sshd = Sshd::start();
    let client = Client::new();
    let port = sshd.port().to_string();

    let mut terminal = Terminal::spawn(&client, &[]);
    terminal.wait_for("y initialize");
    terminal.type_text("y");
    terminal.wait_for("quit");

    // The form's fields: alias, host name, user, port, identity file, jump host.
    terminal.type_text("a");
    terminal.wait_for("Enter save");
    for (index, value) in [
        "box",
        "127.0.0.1",
        &current_user().0,
        &port,
        sshd.client_key().to_str().unwrap(),
    ]
    .into_iter()
    .enumerate()
    {
        if index > 0 {
            terminal.key(Key::Tab);
        }
        terminal.paste(value);
    }
    terminal.key(Key::Enter);
    terminal.wait_for("Saved box");

    // OpenSSH takes the terminal over, asks about the unknown host key and
    // runs the session; the TUI comes back when it ends.
    terminal.key(Key::Enter);
    terminal.wait_for("continue connecting");
    terminal.type_text("yes\r");
    terminal.wait_for("portico-e2e ready");
    terminal.type_text("hi\r");
    terminal.wait_for("portico-e2e got hi");
    // Back on the TUI's own screen, which the session's output never touched.
    terminal.wait_gone("portico-e2e got hi");
    terminal.wait_for("quit");
    terminal.wait_for("box");

    let managed = fs::read_to_string(client.managed_config()).unwrap();
    assert!(managed.contains("Host box\n"), "{managed}");
    assert!(managed.contains(&format!("Port {port}\n")), "{managed}");
    let known = fs::read_to_string(client.known_hosts()).unwrap_or_default();
    assert!(!known.trim().is_empty(), "the host key was not recorded");

    terminal.type_text("q");
    let status = terminal.wait_exit();
    assert!(status.success(), "{status:?}\n{}", terminal.screen());
    real_home.assert_unchanged();
}
