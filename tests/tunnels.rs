mod support;

use std::{fs, net::TcpListener, path::PathBuf};

use support::TestEnv;

/// A fake `ssh`: `-G` answers like OpenSSH (refusing the alias `rejected`), and a tunnel start
/// (`-f`) logs its arguments and leaves a background stand-in that carries them, as the real
/// ssh's background process does, unless `fail-start` exists, when it fails as ssh would on a
/// busy remote port.
fn tunnel_env() -> (TestEnv, PathBuf) {
    let env = TestEnv::new();
    let log = env.home().parent().unwrap().join("ssh-calls");
    let fail = env.home().parent().unwrap().join("fail-start");
    env.install_program(
        "ssh",
        &format!(
            r#"#!/bin/sh
if [ "$1" = -G ]; then
  for alias do :; done
  if [ "$alias" = rejected ] && [ "$2" = -o ]; then
    echo "Bad local forwarding specification '1:x'" >&2
    exit 255
  fi
  printf 'hostname resolved.example\nuser resolved\nport 22\n'
  exit 0
fi
if [ "$1" = -f ]; then
  printf '%s\n' "$*" >> '{log}'
  prev=
  for arg do
    [ "$prev" = -E ] && elog=$arg
    prev=$arg
  done
  if [ -f '{fail}' ]; then
    printf '\033[31mError: remote port forwarding failed for listen port 8080\033[0m\a\n' >> "$elog"
    exit 255
  fi
  # Like ssh -f, the background process leads its own session.
  setsid sh -c 'sleep 60; exit 0' sh "$@" </dev/null >/dev/null 2>&1 &
  exit 0
fi
exit 0
"#,
            log = log.display(),
            fail = fail.display(),
        ),
    );
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "prod", "-H", "prod.example"])
        .assert_success();
    (env, log)
}

fn stdout(env: &TestEnv, args: &[&str]) -> String {
    let output = env.run(args).0;
    assert!(
        output.status.success(),
        "{args:?}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Stops every tunnel when dropped, so no stand-in outlives a failed test.
struct StopAll<'a>(&'a TestEnv);

impl Drop for StopAll<'_> {
    fn drop(&mut self) {
        let _ = self.0.run(["tunnel", "stop", "--all"]);
    }
}

#[test]
fn tunnels_are_added_listed_shown_edited_and_removed() {
    let (env, _) = tunnel_env();
    assert!(stdout(&env, &["tunnels"]).contains("No tunnels."));
    // Port 1 needs privileges, so nothing listens on it and it never shows as in use.
    env.run([
        "tunnel",
        "add",
        "db",
        "--on",
        "prod",
        "-L",
        "127.0.0.1:1:localhost:5432",
    ])
    .assert_stdout_contains("Added db (127.0.0.1:1 \u{2192} localhost:5432 through prod).");
    env.run(["tunnel", "add", "socks", "--on", "prod", "-D", "1080"])
        .assert_success();
    let list = stdout(&env, &["tunnels"]);
    assert!(
        list.contains("db     prod  127.0.0.1:1 \u{2192} localhost:5432  stopped"),
        "{list}"
    );
    assert!(list.contains("socks  prod  socks 1080"), "{list}");
    let file = fs::read_to_string(env.home().join(".ssh/portico/tunnels")).unwrap();
    assert!(
        file.contains("Tunnel db\n    Host prod\n    LocalForward 127.0.0.1:1 localhost:5432\n"),
        "{file}"
    );

    let show = stdout(&env, &["tunnel", "show", "db"]);
    assert!(
        show.contains("Forward: local -L 127.0.0.1:1:localhost:5432"),
        "{show}"
    );
    assert!(show.contains("Status:  stopped"), "{show}");
    assert!(
        show.contains("-f -N -o ExitOnForwardFailure=yes -o BatchMode=yes -o ControlMaster=no -o ControlPath=none -o RemoteCommand=none -E "),
        "{show}"
    );

    env.run([
        "tunnel",
        "edit",
        "db",
        "--name",
        "pg",
        "-L",
        "2:localhost:5432",
    ])
    .assert_stdout_contains("Updated pg.");
    let list = stdout(&env, &["tunnels"]);
    assert!(
        list.contains("pg     prod  2 \u{2192} localhost:5432"),
        "{list}"
    );
    env.run(["tunnel", "remove", "pg"])
        .assert_failure_stderr_contains("removal requires --force");
    env.run(["tunnel", "remove", "pg", "-f"])
        .assert_stdout_contains("Removed pg.");
    assert!(!stdout(&env, &["tunnels"]).contains("pg"));
}

#[test]
fn invalid_tunnels_are_refused() {
    let (env, _) = tunnel_env();
    env.run(["tunnel", "add", "db", "--on", "prod", "-D", "1080"])
        .assert_success();
    env.run(["tunnel", "add", "DB", "--on", "prod", "-D", "1081"])
        .assert_failure_stderr_contains("a tunnel named DB already exists");
    env.run(["tunnel", "add", "web", "--on", "nowhere", "-D", "1081"])
        .assert_failure_stderr_contains("host nowhere is not in the inventory");
    env.run(["tunnel", "add", "web", "--on", "prod", "-L", "/tmp/s:db:1"])
        .assert_failure_stderr_contains("Unix-socket forwards are not supported");
    env.run(["tunnel", "add", "web", "--on", "prod"])
        .assert_failure_stderr_contains("give the forward");
    env.run(["add", "rejected", "-H", "x.example"])
        .assert_success();
    env.run(["tunnel", "add", "web", "--on", "rejected", "-D", "1081"])
        .assert_failure_stderr_contains("OpenSSH rejected tunnel web: Bad local forwarding");
    env.run(["tunnel", "show", "missing"])
        .assert_failure_stderr_contains("no tunnel named missing");
    env.run(["tunnel", "start"])
        .assert_failure_stderr_contains("choose a tunnel: portico tunnel start <name>");
}

#[test]
fn a_tunnel_starts_in_the_background_and_stops() {
    let (env, calls) = tunnel_env();
    let _stop = StopAll(&env);
    env.run([
        "tunnel",
        "add",
        "socks",
        "--on",
        "prod",
        "-D",
        "127.0.0.1:0",
    ])
    .assert_failure_stderr_contains("port must be an integer");
    // A port nobody uses: bound, read, released.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let spec = format!("127.0.0.1:{port}");
    env.run(["tunnel", "add", "socks", "--on", "prod", "-D", &spec])
        .assert_success();
    env.run(["tunnel", "start", "socks"])
        .assert_stdout_contains("Started socks (socks 127.0.0.1:");
    let call = fs::read_to_string(&calls).unwrap();
    let runtime = env.home().join(".ssh/portico/run/tunnels");
    assert_eq!(
        call.trim(),
        format!(
            "-f -N -o ExitOnForwardFailure=yes -o BatchMode=yes -o ControlMaster=no -o ControlPath=none -o RemoteCommand=none -E {} -D {spec} -- prod",
            runtime.join("socks.log").display()
        )
    );
    let list = stdout(&env, &["tunnels"]);
    assert!(list.contains("running  pid "), "{list}");
    env.run(["tunnel", "start", "socks"])
        .assert_stdout_contains("socks is already running (pid ");
    env.run(["tunnel", "edit", "socks", "-D", "1080"])
        .assert_failure_stderr_contains("tunnel socks is running; stop it first");
    env.run(["tunnel", "remove", "socks", "-f"])
        .assert_failure_stderr_contains("is running; stop it first");

    // The record may vanish (a cleared runtime directory); the process is found again.
    fs::remove_file(runtime.join("socks.run")).unwrap();
    assert!(stdout(&env, &["tunnels"]).contains("running  pid "));
    assert!(runtime.join("socks.run").exists());

    env.run(["tunnel", "stop", "socks"])
        .assert_stdout_contains("Stopped socks.");
    let list = stdout(&env, &["tunnels"]);
    assert!(list.contains("stopped"), "{list}");
    env.run(["tunnel", "stop", "socks"])
        .assert_stdout_contains("socks is not running.");
    env.run(["tunnel", "stop", "--all"])
        .assert_stdout_contains("No tunnels are running.");
}

#[test]
fn a_busy_port_is_reported_and_refuses_the_start() {
    let (env, calls) = tunnel_env();
    let _stop = StopAll(&env);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let spec = format!("127.0.0.1:{port}:localhost:22");
    env.run(["tunnel", "add", "db", "--on", "prod", "-L", &spec])
        .assert_success();
    let list = stdout(&env, &["tunnels"]);
    assert!(
        list.contains(&format!("port-in-use  127.0.0.1:{port} is in use")),
        "{list}"
    );
    env.run(["tunnel", "start", "db"])
        .assert_failure_stderr_contains(&format!("port {port} on 127.0.0.1 is in use"));
    assert!(!calls.exists(), "ssh was started");
    drop(listener);
    assert!(stdout(&env, &["tunnels"]).contains("stopped"));
}

#[test]
fn colliding_tunnels_warn_and_only_one_runs() {
    let (env, _) = tunnel_env();
    let _stop = StopAll(&env);
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    for name in ["staging", "prod-db"] {
        let spec = format!("{port}:localhost:5432");
        env.run(["tunnel", "add", name, "--on", "prod", "-L", &spec])
            .assert_success();
    }
    let list = stdout(&env, &["tunnels"]);
    assert!(
        list.contains("Warning: tunnel staging: uses the same port as tunnel prod-db"),
        "{list}"
    );
    env.run(["tunnel", "start", "staging"]).assert_success();
    env.run(["tunnel", "start", "prod-db"])
        .assert_failure_stderr_contains("tunnel staging is running on the same port");
}

#[test]
fn a_failed_start_is_recorded_and_explained() {
    let (env, _) = tunnel_env();
    fs::write(env.home().parent().unwrap().join("fail-start"), "").unwrap();
    env.run([
        "tunnel",
        "add",
        "preview",
        "--on",
        "prod",
        "-R",
        "8080:localhost:3000",
    ])
    .assert_success();
    env.run(["tunnel", "start", "preview"])
        .assert_failure_stderr_contains("the server refused the remote forward");
    let list = stdout(&env, &["tunnels"]);
    assert!(
        list.contains("failed  the server refused the remote forward on port 8080"),
        "{list}"
    );
    let record =
        fs::read_to_string(env.home().join(".ssh/portico/run/tunnels/preview.run")).unwrap();
    assert!(
        !record.contains('\u{1b}') && !record.contains('\u{7}'),
        "escape sequences reached the record: {record:?}"
    );
    let show = stdout(&env, &["tunnel", "show", "preview"]);
    assert!(
        show.contains("Log:\n  Error: remote port forwarding failed for listen port 8080"),
        "{show}"
    );
    env.run(["tunnel", "stop", "preview"])
        .assert_stdout_contains("preview is not running.");
    assert!(stdout(&env, &["tunnels"]).contains("stopped"));
}

#[test]
fn a_busy_address_names_only_a_tunnel_listening_there() {
    let (env, _) = tunnel_env();
    let _stop = StopAll(&env);
    let held = TcpListener::bind("127.0.0.2:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let a = format!("127.0.0.1:{port}:localhost:22");
    let b = format!("127.0.0.2:{port}:localhost:22");
    env.run(["tunnel", "add", "a", "--on", "prod", "-L", &a])
        .assert_success();
    env.run(["tunnel", "add", "b", "--on", "prod", "-L", &b])
        .assert_success();
    env.run(["tunnel", "start", "a"]).assert_success();
    let refused = env.run(["tunnel", "start", "b"]).0;
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains(&format!("port {port} on 127.0.0.2 is in use"))
            && !stderr.contains("by tunnel"),
        "{stderr}"
    );
}

#[test]
fn stop_all_works_when_the_tunnels_file_is_unreadable() {
    let (env, _) = tunnel_env();
    let _stop = StopAll(&env);
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let spec = format!("127.0.0.1:{port}");
    env.run(["tunnel", "add", "socks", "--on", "prod", "-D", &spec])
        .assert_success();
    env.run(["tunnel", "start", "socks"]).assert_success();
    fs::write(env.home().join(".ssh/portico/tunnels"), "Garbage\n").unwrap();
    env.run(["tunnel", "stop", "--all"])
        .assert_stdout_contains("Stopped socks.");
}
