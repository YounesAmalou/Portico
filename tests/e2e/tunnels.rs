//! Tunnels with the real OpenSSH client and server: a started tunnel is a background `ssh` that
//! carries traffic after `portico` exits, is found again, and is the only process `stop` signals.

use std::{
    fs,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::{
    client::{Client, RealHome, text},
    sshd::Sshd,
    terminal::{Key, Terminal},
};

/// A TCP server on `127.0.0.1` that echoes each connection back, for as long as the test runs.
fn echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut stream = stream;
                let mut buffer = [0; 256];
                while let Ok(read) = stream.read(&mut buffer) {
                    if read == 0 || stream.write_all(&buffer[..read]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

/// A port of `127.0.0.1` that was free a moment ago.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Sends `message` through `stream` and expects it back.
fn assert_echoes(mut stream: TcpStream, message: &str) {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(message.as_bytes()).unwrap();
    let mut buffer = vec![0; message.len()];
    stream.read_exact(&mut buffer).unwrap();
    assert_eq!(String::from_utf8(buffer).unwrap(), message);
    let _ = stream.shutdown(Shutdown::Both);
}

/// Connects to `port`, retrying briefly: a remote forward listens once the server has set it up.
fn connect(port: u16) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => return stream,
            Err(error) if Instant::now() >= deadline => panic!("port {port}: {error}"),
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn run_ok(client: &Client, args: &[&str]) -> String {
    text(&client.run_ok(args))
}

fn pid_of(client: &Client, name: &str) -> u32 {
    let record = fs::read_to_string(client.tunnel_runtime().join(format!("{name}.run"))).unwrap();
    record
        .lines()
        .find_map(|line| line.strip_prefix("pid "))
        .unwrap()
        .parse()
        .unwrap()
}

fn wait_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while crate::procs::running(pid) {
        assert!(Instant::now() < deadline, "process {pid} is still running");
        thread::sleep(Duration::from_millis(20));
    }
}

/// A client with a trusted host `box` on its own server.
fn setup() -> (Sshd, Client) {
    let sshd = Sshd::start();
    let client = Client::new();
    client.init();
    client.add_host("box", &sshd, &sshd.client_key());
    client.trust("box");
    (sshd, client)
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
fn a_local_forward_outlives_portico_and_stops_cleanly() {
    let real_home = RealHome::snapshot();
    let (_sshd, client) = setup();
    let _stop = StopAll(&client);
    let echo = echo_server();
    let port = free_port();
    let spec = format!("127.0.0.1:{port}:127.0.0.1:{echo}");
    run_ok(
        &client,
        &["tunnel", "add", "db", "--on", "box", "-L", &spec],
    );
    let started = run_ok(&client, &["tunnel", "start", "db"]);
    assert!(started.contains("Started db"), "{started}");

    // `portico` has exited; the tunnel still carries traffic.
    assert_echoes(connect(port), "through the tunnel");
    let list = run_ok(&client, &["tunnels"]);
    assert!(list.contains("running  pid "), "{list}");
    let pid = pid_of(&client, "db");

    // A second tunnel on the same port is refused while the first runs.
    let other = format!("127.0.0.1:{port}:127.0.0.1:{echo}");
    run_ok(
        &client,
        &["tunnel", "add", "db2", "--on", "box", "-L", &other],
    );
    let refused = client.run(&["tunnel", "start", "db2"]);
    assert!(!refused.status.success());
    assert!(
        text(&refused).contains("tunnel db is running on the same port"),
        "{}",
        text(&refused)
    );
    assert!(run_ok(&client, &["tunnels"]).contains(&format!(
        "port-in-use  127.0.0.1:{port} is used by tunnel db"
    )));

    // A lost record is rebuilt from the running process.
    fs::remove_file(client.tunnel_runtime().join("db.run")).unwrap();
    let list = run_ok(&client, &["tunnels"]);
    let row = list.lines().find(|line| line.starts_with("db ")).unwrap();
    assert!(
        row.contains("running") && row.contains(&format!("pid {pid}")),
        "{list}"
    );
    assert_eq!(pid_of(&client, "db"), pid);

    let stopped = run_ok(&client, &["tunnel", "stop", "db"]);
    assert!(stopped.contains("Stopped db."), "{stopped}");
    wait_gone(pid);
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    assert!(run_ok(&client, &["tunnels"]).contains("stopped"));

    // A port someone else holds shows, and refuses the start before ssh runs.
    let holder = TcpListener::bind(("127.0.0.1", port)).unwrap();
    assert!(run_ok(&client, &["tunnels"]).contains(&format!("127.0.0.1:{port} is in use")));
    let busy = client.run(&["tunnel", "start", "db"]);
    assert!(
        text(&busy).contains(&format!("port {port} on 127.0.0.1 is in use")),
        "{}",
        text(&busy)
    );
    drop(holder);

    real_home.assert_unchanged();
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn a_dynamic_forward_answers_socks() {
    let (_sshd, client) = setup();
    let _stop = StopAll(&client);
    let echo = echo_server();
    let port = free_port();
    let spec = format!("127.0.0.1:{port}");
    run_ok(
        &client,
        &["tunnel", "add", "socks", "--on", "box", "-D", &spec],
    );
    run_ok(&client, &["tunnel", "start", "socks"]);

    let mut stream = connect(port);
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    // SOCKS5: no authentication, then CONNECT 127.0.0.1:<echo>.
    stream.write_all(&[5, 1, 0]).unwrap();
    let mut greeting = [0; 2];
    stream.read_exact(&mut greeting).unwrap();
    assert_eq!(greeting, [5, 0]);
    let [high, low] = echo.to_be_bytes();
    stream
        .write_all(&[5, 1, 0, 1, 127, 0, 0, 1, high, low])
        .unwrap();
    let mut reply = [0; 10];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(&reply[..2], [5, 0], "SOCKS connect refused: {reply:?}");
    assert_echoes(stream, "through socks");
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn a_remote_forward_listens_on_the_server_and_a_busy_one_fails() {
    let (_sshd, client) = setup();
    let _stop = StopAll(&client);
    let echo = echo_server();
    let remote = free_port();
    let spec = format!("127.0.0.1:{remote}:127.0.0.1:{echo}");
    run_ok(
        &client,
        &["tunnel", "add", "back", "--on", "box", "-R", &spec],
    );
    run_ok(&client, &["tunnel", "start", "back"]);
    // The server is this machine, so its side of the forward is reachable here.
    assert_echoes(connect(remote), "from the server side");

    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = format!(
        "127.0.0.1:{}:127.0.0.1:{echo}",
        taken.local_addr().unwrap().port()
    );
    run_ok(
        &client,
        &["tunnel", "add", "busy", "--on", "box", "-R", &busy],
    );
    let failed = client.run(&["tunnel", "start", "busy"]);
    assert!(!failed.status.success());
    assert!(
        text(&failed).contains("the server refused the remote forward on port"),
        "{}",
        text(&failed)
    );
    assert!(run_ok(&client, &["tunnels"]).contains("failed"));
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn a_tunnel_killed_outside_portico_shows_as_failed() {
    let (_sshd, client) = setup();
    let _stop = StopAll(&client);
    let port = free_port();
    let spec = format!("127.0.0.1:{port}");
    run_ok(
        &client,
        &["tunnel", "add", "socks", "--on", "box", "-D", &spec],
    );
    run_ok(&client, &["tunnel", "start", "socks"]);
    let pid = pid_of(&client, "socks");
    kill(pid);
    wait_gone(pid);
    let list = run_ok(&client, &["tunnels"]);
    assert!(list.contains("failed"), "{list}");
    let stop = run_ok(&client, &["tunnel", "stop", "socks"]);
    assert!(stop.contains("socks is not running."), "{stop}");
}

/// Ends `pid` at once, as something outside Portico would.
fn kill(pid: u32) {
    let mut command = if cfg!(windows) {
        let mut command = Command::new("taskkill");
        command.args(["/F", "/PID"]);
        command
    } else {
        let mut command = Command::new("kill");
        command.arg("-KILL");
        command
    };
    let output = command.arg(pid.to_string()).output().unwrap();
    assert!(output.status.success(), "{}", text(&output));
}

/// A process that waits half a minute, unless it dies with the test.
struct Stranger(Child);

impl Stranger {
    fn spawn() -> Self {
        let mut command = if cfg!(windows) {
            let mut command = Command::new("ping");
            command.args(["-n", "30", "127.0.0.1"]);
            command
        } else {
            let mut command = Command::new("sleep");
            command.arg("30");
            command
        };
        Self(command.stdout(Stdio::null()).spawn().unwrap())
    }
}

impl Drop for Stranger {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn stop_never_signals_a_stranger() {
    let (_sshd, client) = setup();
    run_ok(
        &client,
        &["tunnel", "add", "db", "--on", "box", "-D", "1080"],
    );
    let mut stranger = Stranger::spawn();
    let pid = stranger.0.id();
    let (start, boot) = crate::procs::start_and_boot(pid);
    fs::create_dir_all(client.tunnel_runtime()).unwrap();
    // The PID now belongs to another process (a wrong start time), and then a process that
    // matches the PID and start time but is not the tunnel (no log in its command line).
    for recorded_start in [start + 1, start] {
        fs::write(
            client.tunnel_runtime().join("db.run"),
            format!(
                "state running\npid {pid}\nstart {recorded_start}\nboot {}\nstarted 0\nhost box\nforward -D 1080\n",
                boot.trim()
            ),
        )
        .unwrap();
        let stop = run_ok(&client, &["tunnel", "stop", "db"]);
        assert!(stop.contains("db is not running."), "{stop}");
        thread::sleep(Duration::from_millis(100));
        assert!(
            stranger.0.try_wait().unwrap().is_none(),
            "the stranger was signalled"
        );
    }
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn an_untrusted_host_key_fails_in_batch_mode_with_a_hint() {
    let sshd = Sshd::start();
    let client = Client::new();
    client.init();
    client.add_host("box", &sshd, &sshd.client_key());
    let port = free_port();
    let spec = format!("127.0.0.1:{port}");
    run_ok(
        &client,
        &["tunnel", "add", "socks", "--on", "box", "-D", &spec],
    );
    let failed = client.run(&["tunnel", "start", "socks"]);
    assert!(!failed.status.success());
    let output = text(&failed);
    assert!(output.contains("the host key is not trusted"), "{output}");
    assert!(output.contains("portico test box"), "{output}");
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn the_tui_adds_starts_and_stops_a_tunnel() {
    let (_sshd, client) = setup();
    let _stop = StopAll(&client);
    let echo = echo_server();
    let port = free_port();

    let mut terminal = Terminal::spawn(&client, &[]);
    terminal.wait_for("box");
    terminal.type_text("T");
    terminal.wait_for("No tunnels. Press a to add one.");
    terminal.type_text("a");
    terminal.wait_for("Add tunnel");
    terminal.type_text("db");
    terminal.key(Key::Tab);
    terminal.type_text("bo");
    // Tab completes the alias, and then moves on: past Container (left empty, so Backend is
    // not offered) and Kind.
    terminal.key(Key::Tab);
    for field in [
        "",
        "",
        "127.0.0.1",
        &port.to_string(),
        "127.0.0.1",
        &echo.to_string(),
    ] {
        terminal.key(Key::Tab);
        terminal.type_text(field);
    }
    terminal.key(Key::Enter);
    terminal.wait_for("Saved db");
    terminal.wait_for("stopped");

    terminal.type_text("s");
    terminal.wait_for("Started db");
    terminal.wait_for("keep running after q");
    assert_echoes(connect(port), "from the tui");

    terminal.type_text("s");
    terminal.wait_for("Stopped db");
    terminal.wait_gone("keep running after q");
    terminal.type_text("q");
    let status = terminal.wait_exit();
    assert!(status.success(), "{status:?}\n{}", terminal.screen());
}

/// The proxy processes (`ssh -W`, which OpenSSH runs for a ProxyJump) towards `port`. A proxy
/// started before the tunnel backgrounds itself is not its child but a sibling, so they are
/// found by their command line.
fn proxies_to(port: u16) -> Vec<u32> {
    crate::procs::with_args(|args| {
        args.iter().any(|arg| arg == b"-W")
            && args
                .iter()
                .any(|arg| String::from_utf8_lossy(arg).contains(&port.to_string()))
    })
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn a_tunnel_through_a_jump_host_returns_and_stops_with_its_proxy() {
    let (sshd, client) = setup();
    let _stop = StopAll(&client);
    // `jumped` reaches the same server through `box` as its jump host.
    client.append_config(&format!(
        "\nHost jumped\n  HostName 127.0.0.1\n  Port {port}\n  User {user}\n  \
         IdentityFile {key}\n  ProxyJump box\n",
        port = sshd.port(),
        user = crate::sshd::current_user().0,
        key = sshd.client_key().display(),
    ));
    let echo = echo_server();
    let port = free_port();
    let spec = format!("127.0.0.1:{port}:127.0.0.1:{echo}");
    run_ok(
        &client,
        &["tunnel", "add", "deep", "--on", "jumped", "-L", &spec],
    );

    // The proxy child holds ssh's stderr for the tunnel's life; the start must not wait for it.
    let mut start = client
        .command(&["tunnel", "start", "deep"])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = start.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = start.kill();
            panic!(
                "`tunnel start` through a jump host did not return\n{}",
                client.tunnel_logs()
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{status:?}");
    assert_echoes(connect(port), "through the jump host");

    let pid = pid_of(&client, "deep");
    let proxies = proxies_to(sshd.port());
    assert!(!proxies.is_empty(), "no ssh -W proxy for the tunnel {pid}");
    let stopped = run_ok(&client, &["tunnel", "stop", "deep"]);
    assert!(stopped.contains("Stopped deep."), "{stopped}");
    wait_gone(pid);
    for proxy in proxies {
        wait_gone(proxy);
    }
}

/// A TCP server on `127.0.0.1`, and on `::1` where this machine has it, that writes one byte to
/// each connection, for as long as the test runs: a quick forward to a host goes to its
/// `localhost`, which the server may resolve to either address first.
fn byte_server(byte: u8) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let serve = move |listener: TcpListener| {
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let _ = stream.write_all(&[byte]);
            }
        });
    };
    if let Ok(v6) = TcpListener::bind(("::1", port)) {
        serve(v6);
    }
    serve(listener);
    port
}

/// Connects to `port` and reads the one byte the server writes.
fn read_byte(port: u16) -> u8 {
    let mut stream = connect(port);
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut byte = [0; 1];
    stream.read_exact(&mut byte).unwrap();
    byte[0]
}

/// The entry named `name` of `portico tunnels --json`.
fn listed(client: &Client, name: &str) -> serde_json::Value {
    let document: serde_json::Value =
        serde_json::from_str(&run_ok(client, &["tunnels", "--json"])).unwrap();
    document["tunnels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tunnel| tunnel["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("no tunnel {name} in {document}"))
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn a_quick_forward_reaches_a_port_and_is_saved() {
    let real_home = RealHome::snapshot();
    let (_sshd, client) = setup();
    let _stop = StopAll(&client);
    // The server is this machine, so a port listening here is the host's port.
    let remote = byte_server(42);
    let local = free_port();
    let name = format!("_box-{remote}");

    let started: serde_json::Value = serde_json::from_str(&run_ok(
        &client,
        &[
            "forward",
            "--on",
            "box",
            "--port",
            &remote.to_string(),
            "--local",
            &local.to_string(),
            "--json",
        ],
    ))
    .unwrap();
    assert_eq!(started["tunnel"]["name"], name.as_str(), "{started}");
    assert_eq!(started["tunnel"]["temporary"], true, "{started}");
    assert_eq!(
        started["tunnel"]["forward"]["bind_port"], local,
        "{started}"
    );
    assert_eq!(read_byte(local), 42);

    let temporary = listed(&client, &name);
    assert_eq!(temporary["temporary"], true, "{temporary}");
    assert_eq!(temporary["reaches"], "box", "{temporary}");
    assert_eq!(temporary["status"]["state"], "running", "{temporary}");

    let saved = run_ok(&client, &["tunnel", "save", &name, "--name", "kept"]);
    assert!(saved.contains("Saved kept."), "{saved}");
    let kept = listed(&client, "kept");
    assert_eq!(kept["temporary"], false, "{kept}");
    assert_eq!(kept["status"]["state"], "running", "{kept}");
    assert!(!run_ok(&client, &["tunnels"]).contains(&name));
    assert_eq!(read_byte(local), 42);

    let pid = pid_of(&client, "kept");
    let stopped = run_ok(&client, &["tunnel", "stop", "kept"]);
    assert!(stopped.contains("Stopped kept."), "{stopped}");
    wait_gone(pid);
    assert!(TcpStream::connect(("127.0.0.1", local)).is_err());

    real_home.assert_unchanged();
}

/// Installs a fake `docker` on `sshd`'s side that lists one running container `web`, outside
/// any Compose project and publishing nothing, while `state/address` exists, and inspects it at
/// the address that file holds, exposing `port`. No file, no container.
#[cfg(unix)]
fn install_docker(sshd: &Sshd, state: &std::path::Path, port: u16) {
    let ps = r#"{"id":"c0ffee00c0ffee00","name":"web","image":"nginx","command":"","running_for":"","status":"Up 1 minute","ports":"","networks":"bridge","mounts":"","project":"","service":"","number":"","working_dir":""}"#;
    crate::executable::write_executable(
        sshd.bin().join("docker"),
        &format!(
            r#"#!/bin/sh
address='{address}'
case "$1" in
  ps) [ -f "$address" ] && printf '%s\n' '{ps}'; exit 0 ;;
  inspect) printf '{{"bridge":{{"IPAddress":"%s"}}}}|{{"{port}/tcp":{{}}}}|default\n' "$(cat "$address")"; exit 0 ;;
esac
exit 1
"#,
            address = state.join("address").display(),
        ),
    );
}

/// A TCP server on `address`:`port` that writes one byte to each connection, for as long as
/// the test runs.
#[cfg(target_os = "linux")]
fn byte_server_at(address: &str, port: u16, byte: u8) {
    let listener = TcpListener::bind((address, port)).unwrap();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.write_all(&[byte]);
        }
    });
}

// The fake `docker` is a shell script the server runs as a remote command; Win32-OpenSSH runs
// remote commands through `cmd.exe`, so the test is about Unix servers alone.
#[cfg(unix)]
#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn a_container_tunnel_follows_its_container() {
    let real_home = RealHome::snapshot();
    let sshd = Sshd::start_with(crate::sshd::SshdOptions {
        run_commands: true,
        ..Default::default()
    });
    let client = Client::new();
    client.init();
    client.add_host("box", &sshd, &sshd.client_key());
    client.trust("box");
    let _stop = StopAll(&client);
    // The server is this machine: the container's port is a port listening here.
    let port = byte_server(1);
    let local = free_port();
    let state = tempfile::tempdir().unwrap();
    let address = state.path().join("address");
    install_docker(&sshd, state.path(), port);
    fs::write(&address, "127.0.0.1").unwrap();

    run_ok(
        &client,
        &[
            "tunnel",
            "add",
            "api",
            "--on",
            "box",
            "--container",
            "web",
            "--backend",
            "docker",
            "--port",
            &port.to_string(),
            "--local",
            &local.to_string(),
        ],
    );
    let started = run_ok(&client, &["tunnel", "start", "api"]);
    assert!(started.contains("Started api"), "{started}");
    assert!(!started.contains("nothing answers"), "{started}");
    assert_eq!(read_byte(local), 1);
    let api = listed(&client, "api");
    assert_eq!(api["reaches"], "web on box", "{api}");

    // The container moved: the next start looks its address up again. Only Linux answers on
    // the whole of 127.0.0.0/8 without configuration.
    #[cfg(target_os = "linux")]
    {
        byte_server_at("127.0.0.2", port, 2);
        fs::write(&address, "127.0.0.2").unwrap();
        let pid = pid_of(&client, "api");
        run_ok(&client, &["tunnel", "stop", "api"]);
        wait_gone(pid);
        let started = run_ok(&client, &["tunnel", "start", "api"]);
        assert!(started.contains("Started api"), "{started}");
        assert_eq!(read_byte(local), 2);
    }

    // The container is gone: the start is refused before ssh runs.
    let pid = pid_of(&client, "api");
    run_ok(&client, &["tunnel", "stop", "api"]);
    wait_gone(pid);
    fs::remove_file(&address).unwrap();
    let refused = client.run(&["tunnel", "start", "api"]);
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(
        text(&refused).contains("web is not running on box (docker)"),
        "{}",
        text(&refused)
    );

    real_home.assert_unchanged();
}
