//! `--json` with the real OpenSSH client and server: the documents a desktop panel reads while
//! it starts and stops a tunnel.

use std::{net::TcpListener, process::Output};

use serde_json::Value;

use crate::{
    client::{Client, RealHome},
    sshd::Sshd,
};

/// The document Portico printed, after checking the exit status and that stderr is empty.
fn document(output: &Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["schema"], 1);
    document
}

/// Stops every tunnel when dropped, so no ssh outlives a failed test.
struct StopAll<'a>(&'a Client);

impl Drop for StopAll<'_> {
    fn drop(&mut self) {
        let _ = self.0.run(&["tunnel", "stop", "--all"]);
    }
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn a_panel_shows_a_host_and_starts_and_stops_its_tunnel() {
    let real_home = RealHome::snapshot();
    let sshd = Sshd::start();
    let client = Client::new();
    client.init();
    client.add_host("box", &sshd, &sshd.client_key());
    client.trust("box");
    let _stop = StopAll(&client);

    let show = document(&client.run(&["show", "box", "--json"]), 0);
    assert_eq!(show["host"]["alias"], "box");
    assert_eq!(show["host"]["resolved"]["hostname"], "127.0.0.1");
    assert_eq!(
        show["host"]["resolved"]["port"],
        u64::from(sshd.port()),
        "{show}"
    );

    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let spec = format!("127.0.0.1:{port}:127.0.0.1:{}", sshd.port());
    client.run_ok(&["tunnel", "add", "db", "--on", "box", "-L", &spec]);

    let started = document(&client.run(&["tunnel", "start", "db", "--json"]), 0);
    let result = &started["results"][0];
    assert_eq!(result["outcome"], "started", "{started}");
    assert_eq!(result["tunnel"]["status"]["state"], "running");
    let pid = result["pid"].clone();
    assert_eq!(result["tunnel"]["status"]["pid"], pid);

    let list = document(&client.run(&["tunnels", "--json"]), 0);
    assert_eq!(list["tunnels"][0]["status"]["state"], "running");
    assert_eq!(list["tunnels"][0]["status"]["pid"], pid);

    let again = document(&client.run(&["tunnel", "start", "db", "--json"]), 0);
    assert_eq!(again["results"][0]["outcome"], "already-running");

    let stopped = document(&client.run(&["tunnel", "stop", "db", "--json"]), 0);
    assert_eq!(stopped["results"][0]["outcome"], "stopped", "{stopped}");
    assert_eq!(
        stopped["results"][0]["tunnel"]["status"]["state"],
        "stopped"
    );

    // A busy port: the start is refused and the report says why.
    let busy = TcpListener::bind(("127.0.0.1", port)).unwrap();
    let refused = document(&client.run(&["tunnel", "start", "db", "--json"]), 1);
    let result = &refused["results"][0];
    assert_eq!(result["outcome"], "failed", "{refused}");
    assert_eq!(result["pid"], Value::Null);
    assert_eq!(result["tunnel"]["status"]["state"], "port-in-use");
    drop(busy);
    real_home.assert_unchanged();
}
