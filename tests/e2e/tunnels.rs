//! Tunnels with the real OpenSSH client and server: a started tunnel is a background `ssh` that
//! carries traffic after `portico` exits, is found again, and is the only process `stop` signals.

use std::{
    fs,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::Path,
    process::{Child, Command},
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
    while Path::new(&format!("/proc/{pid}")).exists()
        && !fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| stat.contains(") Z"))
    {
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
    assert!(
        Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    wait_gone(pid);
    let list = run_ok(&client, &["tunnels"]);
    assert!(list.contains("failed"), "{list}");
    let stop = run_ok(&client, &["tunnel", "stop", "socks"]);
    assert!(stop.contains("socks is not running."), "{stop}");
}

/// A `sleep` that dies with the test.
struct Stranger(Child);

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
    let mut stranger = Stranger(Command::new("sleep").arg("30").spawn().unwrap());
    let pid = stranger.0.id();
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start: u64 = stat[stat.rfind(')').unwrap() + 2..]
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
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
    // Tab completes the alias, and then moves on.
    terminal.key(Key::Tab);
    for field in [
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
    fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| {
                let args: Vec<&[u8]> = cmdline.split(|byte| *byte == 0).collect();
                args.contains(&b"-W".as_slice())
                    && args
                        .iter()
                        .any(|arg| String::from_utf8_lossy(arg).contains(&port.to_string()))
            })
        })
        .collect()
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
            panic!("`tunnel start` through a jump host did not return");
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
