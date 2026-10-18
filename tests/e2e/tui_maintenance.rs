//! Looking after a host from the TUI: test it, trust its key, break it with
//! an edit and see why, then remove it.

use std::fs;

use crate::{
    client::{Client, RealHome},
    sshd::{Sshd, closed_port},
    terminal::{Key, Terminal},
};

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn test_trust_edit_and_remove() {
    let real_home = RealHome::snapshot();
    let sshd = Sshd::start();
    let client = Client::new();
    client.init();
    client.add_host("box", &sshd, &sshd.client_key());

    let mut terminal = Terminal::spawn(&client, &[]);
    terminal.wait_for("box");
    terminal.wait_for("quit");

    // The first test meets an unknown key; trusting it tests again and passes.
    terminal.type_text("t");
    terminal.wait_for("Trust host key");
    terminal.wait_for("SHA256:");
    terminal.type_text("y");
    terminal.wait_for("\u{2713} box");
    let known = fs::read_to_string(client.known_hosts()).unwrap_or_default();
    assert!(!known.trim().is_empty(), "the host key was not recorded");

    // Pointing the host at a closed port makes the next test explain why.
    let closed = closed_port().to_string();
    terminal.type_text("e");
    terminal.wait_for("Enter save");
    for _ in 0..3 {
        terminal.key(Key::Tab);
    }
    for _ in 0..sshd.port().to_string().len() {
        terminal.key(Key::Backspace);
    }
    terminal.type_text(&closed);
    terminal.key(Key::Enter);
    terminal.wait_for("Saved box");
    let managed = fs::read_to_string(client.managed_config()).unwrap();
    assert!(managed.contains(&format!("Port {closed}\n")), "{managed}");
    terminal.type_text("t");
    terminal.wait_for("nothing accepted the connection");

    // Removing it takes it off the screen and out of the managed config.
    terminal.type_text("x");
    terminal.wait_for("Remove box?");
    terminal.key(Key::Enter);
    terminal.wait_for("Removed box");
    terminal.wait_gone("box  managed");
    let managed = fs::read_to_string(client.managed_config()).unwrap();
    assert!(!managed.contains("Host box"), "{managed}");

    terminal.type_text("q");
    let status = terminal.wait_exit();
    assert!(status.success(), "{status:?}\n{}", terminal.screen());
    real_home.assert_unchanged();
}
