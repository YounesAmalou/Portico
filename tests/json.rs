//! `--json`: the documents of `docs/json-output.md`, pinned by the golden files in
//! `tests/fixtures/json/`. `PORTICO_BLESS=1 cargo test --test json` rewrites them.

mod support;

use std::{
    ffi::OsStr,
    fs,
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
};

use serde_json::Value;
use support::TestEnv;

/// What every test environment's fakes need to say "secret" without it ever being printed.
const SECRET: &str = "s3cret";

/// The home directory the golden files show.
const HOME: &str = "/home/me";

/// The busy port the golden files show, whatever port the test really bound.
const BUSY_PORT: &str = "5433";

/// An initialised home with the managed host `prod`, the external hosts `bastion` and `legacy`
/// (whose resolution fails), an agent holding `prod`'s key and one other, saved tunnels in each
/// status, and fakes for `ssh`, `ssh-add`, `ssh-keygen` and `docker`.
struct Fixture {
    env: TestEnv,
    /// Holds the busy tunnel's port.
    _busy: TcpListener,
    busy_port: u16,
    /// The agent's socket. Windows' agents listen on named pipes instead.
    #[cfg(unix)]
    _agent: std::os::unix::net::UnixListener,
}

fn calls(env: &TestEnv, name: &str) -> PathBuf {
    env.bin().parent().unwrap().join(format!("{name}.calls"))
}

fn fake_ssh(env: &TestEnv) -> String {
    format!(
        r#"#!/bin/sh
if [ "$1" = -G ]; then
  for alias do :; done
  case "$alias" in
    portico-defaults) printf 'identityfile ~/.ssh/id_rsa\nidentityfile ~/.ssh/id_ed25519\n' ;;
    prod) printf 'hostname db.example.com\nuser deploy\nport 22\nidentityfile %s/.ssh/id_ed25519\nidentityfile %s/.ssh/old_rsa\nproxyjump bastion\nproxycommand sh -c "TOKEN={SECRET} nc %%h %%p"\nidentityagent %s/.1password/agent.sock\n' "$HOME" "$HOME" "$HOME" ;;
    legacy) echo 'legacy: line 9: Bad configuration option: frobnicate' >&2; exit 255 ;;
    *) printf 'hostname %s.example.com\nport 22\n' "$alias" ;;
  esac
  exit 0
fi
printf '%s\n' "$*" >> '{ssh_calls}'
if [ "$1" = -f ] || [ "$1" = -N ]; then
  elog=/dev/stderr
  prev=
  for arg do
    [ "$prev" = -E ] && elog=$arg
    prev=$arg
  done
  printf '\033[31mError: remote port forwarding failed for listen port 8080\033[0m\n' >> "$elog"
  exit 255
fi
cat <<'EOF'
{listing}
EOF
"#,
        ssh_calls = calls(env, "ssh").display(),
        listing = listing(),
    )
}

/// `docker ps` lines: a Compose service publishing port 80 on 8080, and a container outside any
/// project, whose command carries a secret.
fn listing() -> String {
    [
                r#"{"id":"4f1c2a9b7d3e","name":"shop-web-1","image":"nginx:1.27","command":"nginx -g daemon","running_for":"3 hours ago","status":"Up 3 hours","ports":"0.0.0.0:8080->80/tcp","networks":"shop_default","mounts":"/srv/shop","project":"shop","service":"web","number":"1","working_dir":"/srv/shop"}"#
        .to_owned(),
        format!(
            r#"{{"id":"9a8b7c6d5e4f","name":"cache","image":"redis:7","command":"redis-server --requirepass={SECRET}","running_for":"5 minutes ago","status":"Up 5 minutes","ports":"","networks":"bridge","mounts":"","project":"","service":"","number":"","working_dir":""}}"#
        ),
    ]
    .join("\n")
}

/// `docker inspect` of `shop-web-1`: on `shop_default` at 172.18.0.4, exposing 80.
const INSPECT: &str = r#"{"shop_default":{"IPAddress":"172.18.0.4"}}|{"80/tcp":{}}|default"#;

fn setup() -> Fixture {
    let env = TestEnv::new();
    env.install_program("ssh", &fake_ssh(&env));
    env.run(["init", "-y"]).assert_success();
    env.run([
        "add",
        "prod",
        "-H",
        "db.example.com",
        "-u",
        "deploy",
        "-i",
        "~/.ssh/id_ed25519",
        "-J",
        "bastion",
    ])
    .assert_success();

    let busy = TcpListener::bind("127.0.0.1:0").unwrap();
    let busy_port = busy.local_addr().unwrap().port();
    // Port 1 needs privileges, so nothing listens on it; `db` and `socks` share it.
    for args in [
        ["db", "-L", "127.0.0.1:1:localhost:5432"],
        ["socks", "-D", "127.0.0.1:1"],
        ["preview", "-R", "8080:localhost:3000"],
        ["busy", "-L", &format!("127.0.0.1:{busy_port}:localhost:80")],
    ] {
        env.run(["tunnel", "add", args[0], "--on", "prod", args[1], args[2]])
            .assert_success();
    }
    // `preview` failed earlier.
    let runtime = env.home().join(".ssh/portico/run/tunnels");
    fs::create_dir_all(&runtime).unwrap();
    fs::write(
        runtime.join("preview.run"),
        "state failed\nreason Error: remote port forwarding failed for listen port 8080\nexit 255\nfinished 1781861043\n",
    )
    .unwrap();
    // A temporary forward to `shop-web-1`'s address, whose ssh exited.
    fs::write(
        runtime.join("_web-80.run"),
        "state failed\nreason Connection to prod closed by remote host.\nexit 255\nfinished 1781861043\nhost prod\nforward -L 127.0.0.1:8080:172.18.0.4:80\nreaches web on prod\n",
    )
    .unwrap();
    fs::write(
        runtime.join("preview.log"),
        "debug1: Authentication succeeded (publickey).\nWarning: remote port forwarding failed for listen port 8080\nError: remote port forwarding failed for listen port 8080\n",
    )
    .unwrap();

    // External hosts, added after the managed ones so `legacy` never fails a validation.
    let mut config = fs::OpenOptions::new()
        .append(true)
        .open(env.ssh_config())
        .unwrap();
    writeln!(
        config,
        "\nHost bastion\n    HostName bastion.example.com\n\nHost legacy\n    HostName legacy.example.com\n\nHost bastion\n    HostName other.example.com"
    )
    .unwrap();

    // `prod`'s key: a private file never read and its public half, held by the agent.
    let ssh = env.home().join(".ssh");
    fs::write(
        ssh.join("id_ed25519"),
        format!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n{SECRET}\n-----END OPENSSH PRIVATE KEY-----\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(
        ssh.join("id_ed25519"),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    fs::write(
        ssh.join("id_ed25519.pub"),
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIprod deploy@prod\n",
    )
    .unwrap();
    fs::create_dir_all(env.home().join(".1password")).unwrap();
    #[cfg(unix)]
    let agent =
        std::os::unix::net::UnixListener::bind(env.home().join(".1password/agent.sock")).unwrap();
    env.install_program(
        "ssh-add",
        "#!/bin/sh\nprintf '256 SHA256:YmF6cXV4Zm9vYmFycXV1eGNvcmdlZ3JhdWx0Z2FycGx5 deploy@prod (ED25519)\\n256 SHA256:Zm9vYmFyYmF6cXV4cXV1eGNvcmdlZ3JhdWx0Z2FycGx5 work laptop (ED25519)\\n'\n",
    );
    env.install_program(
        "ssh-keygen",
        "#!/bin/sh\nprintf '256 SHA256:YmF6cXV4Zm9vYmFycXV1eGNvcmdlZ3JhdWx0Z2FycGx5 deploy@prod (ED25519)\\n'\n",
    );
    env.install_program(
        "docker",
        &format!(
            "#!/bin/sh\nif [ \"$1\" = inspect ]; then\n  echo '{INSPECT}'\n  exit 0\nfi\ncat <<'EOF'\n{}\nEOF\n",
            listing()
        ),
    );
    Fixture {
        env,
        _busy: busy,
        busy_port,
        #[cfg(unix)]
        _agent: agent,
    }
}

impl Fixture {
    /// Runs Portico with a secret in its environment, as a desktop session might have.
    fn run(&self, args: &[&str]) -> std::process::Output {
        self.env
            .run_with_env(args, "PORTICO_SECRET", OsStr::new(SECRET))
            .0
    }

    /// Runs `args`, checks the exit status, an empty stderr and the document's own form, and
    /// compares it with the golden file `name`.
    fn golden(&self, args: &[&str], code: i32, name: &str) -> Value {
        let output = self.run(args);
        check_golden(&output, code, name, self.env.home(), Some(self.busy_port))
    }
}

/// Replaces what changes from run to run (the home, the busy port, PIDs and times), so the
/// golden files are stable.
/// On Windows, paths are also spelled with `/`, an exit status and a tunnel's ssh command as on
/// Unix, so that one golden file serves every system.
fn normalize(text: &str, home: &Path, busy_port: Option<u16>) -> Value {
    let mut document: Value = serde_json::from_str(text).unwrap();
    let home = home.display().to_string();
    // The home first: its temporary name may hold the port's digits.
    let walk_text = |text: &mut String| {
        *text = text.replace(&home, HOME);
        if let Some(port) = busy_port {
            *text = text.replace(&port.to_string(), BUSY_PORT);
        }
        if cfg!(windows) {
            *text = text
                .replace('\\', "/")
                .replace("exit code: ", "exit status: ");
        }
    };
    let busy_port = busy_port.map(u64::from);
    let walk_number = |value: &mut Value| {
        if value.as_u64().is_some() && value.as_u64() == busy_port {
            *value = BUSY_PORT.parse::<u64>().unwrap().into();
        }
    };
    fn walk(value: &mut Value, walk_text: &dyn Fn(&mut String), walk_number: &dyn Fn(&mut Value)) {
        match value {
            Value::Object(map) => {
                for (key, value) in map.iter_mut() {
                    match (key.as_str(), &value) {
                        ("pid", Value::Number(_)) => *value = 41237.into(),
                        ("started", Value::Number(_)) => *value = 1781860921u64.into(),
                        ("finished", Value::Number(_)) => *value = 1781861043u64.into(),
                        ("command", Value::Array(_)) if cfg!(windows) => {
                            unix_start(value);
                            walk(value, walk_text, walk_number);
                        }
                        _ => walk(value, walk_text, walk_number),
                    }
                }
            }
            Value::Array(values) => values
                .iter_mut()
                .for_each(|value| walk(value, walk_text, walk_number)),
            Value::String(text) => walk_text(text),
            Value::Number(_) => walk_number(value),
            _ => {}
        }
    }
    walk(&mut document, &walk_text, &walk_number);
    document
}

/// A tunnel's ssh command as Portico starts it on Unix: Windows' ssh cannot background itself,
/// so there it runs without `-f`, logs at `LogLevel=VERBOSE` (by which the start sees it
/// authenticate) on standard error, and names its log in a `LocalCommand` instead of `-E`.
fn unix_start(command: &mut Value) {
    let Value::Array(words) = command else {
        return;
    };
    if let Some(at) = words
        .windows(2)
        .position(|pair| pair[0] == "-o" && pair[1] == "LogLevel=VERBOSE")
    {
        words.drain(at..at + 2);
    }
    // `-o PermitLocalCommand=no -o "LocalCommand=portico-tunnel <log>"` in place of `-E <log>`.
    if let Some(at) = words
        .iter()
        .position(|word| word == "PermitLocalCommand=no")
        && let Some(log) = words
            .get(at + 2)
            .and_then(Value::as_str)
            .and_then(|command| command.strip_prefix("LocalCommand=portico-tunnel "))
            .map(|log| log.replace("%%", "%"))
    {
        words.splice(at - 1..at + 3, ["-E".into(), log.into()]);
    }
    if words.first().is_some_and(|word| word == "ssh") && words.get(1) != Some(&"-f".into()) {
        words.insert(1, "-f".into());
    }
}

fn golden_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/json")
        .join(name)
}

fn check_golden(
    output: &std::process::Output,
    code: i32,
    name: &str,
    home: &Path,
    busy_port: Option<u16>,
) -> Value {
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    assert_eq!(
        output.status.code(),
        Some(code),
        "{name}: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{name}: stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // One pretty-printed document with sorted keys and a final newline.
    let parsed: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        stdout,
        format!("{}\n", serde_json::to_string_pretty(&parsed).unwrap()),
        "{name}"
    );
    assert_eq!(parsed["schema"], 1, "{name}");
    for unsafe_text in [SECRET, "PRIVATE KEY", "TOKEN"] {
        assert!(!stdout.contains(unsafe_text), "{name} prints {unsafe_text}");
    }
    let document = normalize(&stdout, home, busy_port);
    let text = format!("{}\n", serde_json::to_string_pretty(&document).unwrap());
    let path = golden_path(name);
    if std::env::var_os("PORTICO_BLESS").is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &text).unwrap();
    }
    let expected = fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("no golden file {name}; run with PORTICO_BLESS=1"));
    assert_eq!(text, expected, "{name} differs from its golden file");
    document
}

#[test]
fn hosts() {
    let fixture = setup();
    let document = fixture.golden(&["hosts", "--json"], 0, "hosts.json");
    assert_eq!(document["hosts"][0]["alias"], "bastion");
    assert!(
        document["warnings"][0]["message"]
            .as_str()
            .unwrap()
            .contains("duplicate alias bastion")
    );
}

#[test]
fn hosts_runs_no_ssh_and_asks_no_agent() {
    let fixture = setup();
    let ran = fixture.env.bin().parent().unwrap().join("ran");
    for program in ["ssh", "ssh-add", "ssh-keygen"] {
        fixture.env.install_program(
            program,
            &format!("#!/bin/sh\necho {program} >> '{}'\nexit 1\n", ran.display()),
        );
    }
    let output = fixture.run(&["hosts", "--json"]);
    assert!(output.status.success());
    assert!(!ran.exists(), "{}", fs::read_to_string(&ran).unwrap());
}

/// Unix-only: the host's agent is the fixture's Unix socket (1Password's); Windows' agents
/// listen on named pipes.
#[cfg(unix)]
#[test]
fn show_of_a_managed_host() {
    let fixture = setup();
    let document = fixture.golden(&["show", "prod", "--json"], 0, "show.json");
    let identities = &document["host"]["identities"];
    assert_eq!(identities["agent"]["provider"], "1password");
    assert_eq!(identities["identities"][0]["availability"], "in-agent");
    assert_eq!(identities["identities"][1]["availability"], "missing");
    assert_eq!(identities["also_offered"][0]["comment"], "work laptop");
}

#[test]
fn show_of_an_external_host_that_does_not_resolve() {
    let fixture = setup();
    let document = fixture.golden(&["show", "legacy", "--json"], 0, "show-unresolved.json");
    assert_eq!(document["host"]["declared"], Value::Null);
    assert_eq!(document["host"]["resolved"], Value::Null);
}

/// Unix-only: the fixture's agent is a Unix socket (1Password's); Windows' agents listen on
/// named pipes.
#[cfg(unix)]
#[test]
fn identities() {
    let fixture = setup();
    fixture.golden(&["identities", "--json"], 0, "identities.json");
}

#[test]
fn containers_on_this_machine() {
    let fixture = setup();
    let document = fixture.golden(&["containers", "--json"], 0, "containers.json");
    assert_eq!(document["containers"][0]["display_name"], "web");
}

#[test]
fn containers_on_a_host_run_ssh_in_batch_mode() {
    let fixture = setup();
    fixture.golden(
        &["containers", "--on", "prod", "--json"],
        0,
        "containers-on-host.json",
    );
    let ssh = fs::read_to_string(calls(&fixture.env, "ssh")).unwrap();
    assert!(ssh.contains("BatchMode=yes"), "{ssh}");
}

/// Unix-only: the offers are local Docker access through Unix groups, which Windows lacks
/// (`UNIX_GROUPS` is false).
#[cfg(unix)]
#[test]
fn docker_offers_are_reported_and_never_run() {
    let fixture = setup();
    fixture.env.install_program(
        "docker",
        "#!/bin/sh\necho 'Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?' >&2\nexit 1\n",
    );
    fixture
        .env
        .install_program("systemctl", "#!/bin/sh\nexit 0\n");
    let sudo = calls(&fixture.env, "sudo");
    fixture.env.install_program(
        "sudo",
        &format!("#!/bin/sh\necho \"$@\" >> '{}'\n", sudo.display()),
    );
    let output = fixture
        .env
        .run_with_env(
            ["containers", "--json"],
            "DOCKER_HOST",
            OsStr::new("unix:///var/run/docker.sock"),
        )
        .0;
    let document = check_golden(
        &output,
        1,
        "containers-docker-down.json",
        fixture.env.home(),
        None,
    );
    assert_eq!(document["error"]["kind"], "docker");
    assert_eq!(document["error"]["problem"], "daemon-down");
    assert!(!sudo.exists(), "ran sudo");
}

/// Fakes for a Proxmox VE node reached as `deploy`, whose `sudo -n` either runs `pct` or
/// refuses with `refusal`.
fn proxmox_node(fixture: &Fixture, refusal: Option<&str>) {
    let env = &fixture.env;
    env.install_program(
        "id",
        "#!/bin/sh\ncase \"$1\" in -u) echo 1000 ;; -un) echo deploy ;; esac\n",
    );
    env.install_program(
        "pct",
        "#!/bin/sh\nprintf 'VMID       Status     Lock         Name                \\n101        running                 web                 \\n102        stopped                 db                  \\n110        running    backup       mail                \\n'\n",
    );
    let sudo = match refusal {
        Some(message) => format!("#!/bin/sh\necho '{message}' >&2\nexit 1\n"),
        None => "#!/bin/sh\n[ \"$1\" = -n ] && shift\nexec \"$@\"\n".to_owned(),
    };
    env.install_program("sudo", &sudo);
}

#[test]
fn proxmox_containers() {
    let fixture = setup();
    proxmox_node(&fixture, None);
    let document = fixture.golden(
        &["containers", "--backend", "proxmox", "--json"],
        0,
        "containers-proxmox.json",
    );
    assert_eq!(document["backend"], "proxmox");
    assert_eq!(document["via"], "sudo");
    // Sorted by name, as Docker containers outside a project are.
    assert_eq!(document["containers"][1]["id"], "101");
    assert_eq!(document["containers"][0]["image"], Value::Null);
}

#[test]
fn proxmox_without_root_is_permission_denied() {
    let fixture = setup();
    proxmox_node(&fixture, Some("sudo: a password is required"));
    let document = fixture.golden(
        &["containers", "--backend", "proxmox", "--json"],
        1,
        "containers-proxmox-denied.json",
    );
    assert_eq!(document["error"]["kind"], "proxmox");
    assert_eq!(document["error"]["problem"], "permission-denied");
}

/// Fakes for a Podman host reached as `deploy`, whose `podman ps` prints a Compose service, a
/// lone container and a pod's infra container, or fails with `failure`.
fn podman_host(fixture: &Fixture, failure: Option<&str>) {
    let env = &fixture.env;
    env.install_program(
        "id",
        "#!/bin/sh\ncase \"$1\" in -u) echo 1000 ;; -un) echo deploy ;; esac\n",
    );
    let podman = match failure {
        Some(message) => format!("#!/bin/sh\necho '{message}' >&2\nexit 125\n"),
        None => format!("#!/bin/sh\ncat <<'EOF'\n{PODMAN_PS}\nEOF\n"),
    };
    env.install_program("podman", &podman);
}

const PODMAN_PS: &str = r#"[
  {"Id": "6f1c0a3c9e2b4d5f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f", "Names": ["shop_web_1"], "Image": "docker.io/library/nginx:latest", "Status": "Up 2 hours", "State": "running", "IsInfra": false,
   "Labels": {"com.docker.compose.project": "shop", "com.docker.compose.service": "web", "com.docker.compose.container-number": "1"}},
  {"Id": "1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b", "Names": ["toolbox"], "Image": "registry.fedoraproject.org/fedora:41", "Status": "Up 5 minutes", "State": "running", "IsInfra": false, "Labels": null},
  {"Id": "9f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c5b4a39281706f5e4d3c2b1a0", "Names": ["a1b2c3d4-infra"], "Image": "localhost/podman-pause:5.2.0", "Status": "Up 3 hours", "State": "running", "IsInfra": true}
]"#;

#[test]
fn podman_containers() {
    let fixture = setup();
    podman_host(&fixture, None);
    let document = fixture.golden(
        &["containers", "--backend", "podman", "--json"],
        0,
        "containers-podman.json",
    );
    assert_eq!(document["backend"], "podman");
    assert_eq!(document["via"], Value::Null);
    assert_eq!(document["containers"].as_array().unwrap().len(), 2);
    assert_eq!(document["containers"][0]["project"], "shop");
}

/// Unix-only: the listing names the user (`deploy`) through its shell script, which Windows,
/// with no `sh`, does not run; there `podman ps` runs directly and names no one.
#[cfg(unix)]
#[test]
fn podman_without_subordinate_ids_says_how_to_add_them() {
    let fixture = setup();
    podman_host(
        &fixture,
        Some("Error: cannot set up namespace using \"/usr/bin/newuidmap\": exit status 1"),
    );
    let document = fixture.golden(
        &["containers", "--backend", "podman", "--json"],
        1,
        "containers-podman-missing-subuid.json",
    );
    assert_eq!(document["error"]["kind"], "podman");
    assert_eq!(document["error"]["problem"], "other");
}

#[test]
fn tunnels() {
    let fixture = setup();
    let document = fixture.golden(&["tunnels", "--json"], 0, "tunnels.json");
    let states: Vec<&str> = document["tunnels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tunnel| tunnel["status"]["state"].as_str().unwrap())
        .collect();
    assert_eq!(
        states,
        ["port-in-use", "stopped", "failed", "stopped", "failed"]
    );
    let temporary: Vec<(&str, bool)> = document["tunnels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tunnel| {
            (
                tunnel["reaches"].as_str().unwrap(),
                tunnel["temporary"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        temporary,
        [
            ("prod", false),
            ("prod", false),
            ("prod", false),
            ("prod", false),
            ("web on prod", true)
        ]
    );
}

/// A port nobody uses: bound, read, released.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn forward_to_a_host_prints_the_started_forward() {
    let fixture = setup();
    support::install_tunnel_ssh(&fixture.env);
    let local = free_port();
    let output = fixture.run(&[
        "forward",
        "--on",
        "prod",
        "--port",
        "5432",
        "--local",
        &local.to_string(),
        "--json",
    ]);
    let stopped = fixture.run(&["tunnel", "stop", "--all"]);
    assert!(stopped.status.success());
    // The local port is shown as the busy port, whatever port the test really bound.
    let document = check_golden(&output, 0, "forward.json", fixture.env.home(), Some(local));
    assert_eq!(document["tunnel"]["name"], "_prod-5432");
    assert_eq!(document["tunnel"]["temporary"], true);
    assert_eq!(document["tunnel"]["status"]["state"], "running");
}

#[test]
fn forward_on_this_machine_prints_where_the_ports_are_reachable() {
    let fixture = setup();
    let document = fixture.golden(&["forward", "web", "--json"], 0, "forward-here.json");
    assert_eq!(document["reachable"][0]["address"], "127.0.0.1:8080");
}

#[test]
fn forward_without_a_port_is_a_usage_error_with_the_suggestions() {
    let fixture = setup();
    let output = fixture.run(&["forward", "--on", "prod", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["error"]["kind"], "usage");
    assert_eq!(
        document["error"]["message"],
        "--port is required; no port was suggested"
    );
}

#[test]
fn forward_refuses_a_local_port_of_zero() {
    let fixture = setup();
    let output = fixture.run(&[
        "forward", "--on", "prod", "--port", "80", "--local", "0", "--json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        document["error"]["message"],
        "local port 0 is not a port; give one from 1 through 65535"
    );
}

#[test]
fn tunnel_save_keeps_a_temporary_forward() {
    let fixture = setup();
    let document = fixture.golden(
        &["tunnel", "save", "_web-80", "--json"],
        0,
        "tunnel-save.json",
    );
    assert_eq!(document["tunnel"]["name"], "web-80");
    assert_eq!(document["tunnel"]["temporary"], false);
    let output = fixture.run(&["tunnel", "save", "db", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        document["error"]["message"],
        "db is not a temporary forward"
    );
}

#[test]
fn tunnel_show_of_a_failed_tunnel() {
    let fixture = setup();
    let document = fixture.golden(
        &["tunnel", "show", "preview", "--json"],
        0,
        "tunnel-show.json",
    );
    assert_eq!(
        document["log"][1],
        "Error: remote port forwarding failed for listen port 8080"
    );
}

#[test]
fn tunnel_start_reports_each_name() {
    let fixture = setup();
    let document = fixture.golden(
        &["tunnel", "start", "preview", "nope", "--json"],
        1,
        "tunnel-start.json",
    );
    let results = &document["results"];
    assert_eq!(results[0]["outcome"], "failed");
    assert_eq!(results[0]["tunnel"]["status"]["state"], "failed");
    assert_eq!(results[1]["error"]["kind"], "unknown-tunnel");
    assert_eq!(results[1]["tunnel"], Value::Null);
}

#[test]
fn tunnel_stop_all_with_nothing_running() {
    let fixture = setup();
    fixture.golden(
        &["tunnel", "stop", "--all", "--json"],
        0,
        "tunnel-stop-all.json",
    );
}

/// The desktop panel (`desktop/quickshell/PorticoModel.qml`) lists containers, and starts and
/// stops tunnels, with exactly these forms.
#[test]
fn the_desktop_panel_forms_print_documents() {
    let fixture = setup();
    let output = fixture.run(&["containers", "--json", "--on=prod"]);
    let document = check_golden(
        &output,
        0,
        "containers-on-host.json",
        fixture.env.home(),
        None,
    );
    assert_eq!(document["target"]["alias"], "prod");

    // An alias that starts with "-" reaches Portico as an alias, not as a flag: the
    // answer is an error document (exit status 1), never clap's usage error (2).
    let output = fixture.run(&["containers", "--json", "--on=-dash"]);
    assert_eq!(output.status.code(), Some(1));
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["error"]["kind"], "unknown-host");

    let output = fixture.run(&["tunnel", "start", "--json", "--", "preview"]);
    assert_eq!(output.status.code(), Some(1));
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["results"][0]["name"], "preview");
    assert_eq!(document["results"][0]["outcome"], "failed");

    let output = fixture.run(&["tunnel", "stop", "--json", "--", "db"]);
    assert!(output.status.success());
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["results"][0]["name"], "db");
    assert_eq!(document["results"][0]["outcome"], "not-running");
}

#[test]
fn settings() {
    let fixture = setup();
    let output = fixture.run(&["settings", "--json"]);
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(document["exists"], false);
    assert_eq!(document["users"], serde_json::json!([]));
    assert_eq!(
        document["tabs"],
        serde_json::json!({ "docker": true, "podman": true, "proxmox": true, "tunnels": true })
    );
    let set = fixture.run(&["settings", "set", "tabs.podman", "off"]);
    assert_eq!(set.status.code(), Some(0));
    let path = fixture.env.home().join(".config/portico/settings.toml");
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("[users.local.docker]\n\"shop/api\" = \"deploy\"\n");
    fs::write(&path, text).unwrap();
    fixture.golden(&["settings", "--json"], 0, "settings.json");
}

#[test]
fn an_invalid_settings_file_is_an_error_document() {
    let fixture = setup();
    let path = fixture.env.home().join(".config/portico/settings.toml");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "[tabs\n").unwrap();
    let document = fixture.golden(&["settings", "--json"], 1, "settings-invalid.json");
    assert_eq!(document["error"]["kind"], "settings");
    assert!(
        document["error"]["message"]
            .as_str()
            .unwrap()
            .contains("line 1")
    );
}

#[test]
fn a_missing_name_is_a_usage_error() {
    let fixture = setup();
    fixture.golden(&["show", "--json"], 1, "error-usage.json");
}

#[test]
fn an_unknown_host_is_an_error_document() {
    let fixture = setup();
    fixture.golden(&["show", "nope", "--json"], 1, "error-unknown-host.json");
}

/// `tunnels` lists tunnels without the inventory, so it is not among them.
#[test]
fn an_unreadable_config_is_an_error_document_on_stdout() {
    let fixture = setup();
    let config = fixture.env.ssh_config();
    support::deny_read(&config);
    for args in [
        &["hosts", "--json"][..],
        &["show", "prod", "--json"],
        &["identities", "--json"],
        &["containers", "--on", "prod", "--json"],
    ] {
        let output = fixture.run(args);
        support::allow_read(&config);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(output.stderr.is_empty(), "{args:?}");
        let document: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["error"]["kind"], "other", "{args:?}");
        assert!(
            document["error"]["message"]
                .as_str()
                .unwrap()
                .contains(&support::shown(".ssh/config")),
            "{args:?}: {document}"
        );
        support::deny_read(&config);
    }
    support::allow_read(&config);
}

#[test]
fn json_is_refused_where_it_is_not_offered() {
    let fixture = setup();
    let output = fixture.run(&["add", "x", "-H", "x", "--json"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn an_unset_home_is_an_error_document() {
    // Windows' home is `USERPROFILE`.
    let variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_portico"))
        .args(["hosts", "--json"])
        .env_remove(variable)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        document,
        serde_json::json!({
            "error": { "kind": "other", "message": format!("{variable} is not set"), "next": [] },
            "schema": 1,
        })
    );
    // Without --json it stays text on stderr.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_portico"))
        .arg("hosts")
        .env_remove(variable)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!("error: {variable} is not set\n")
    );
}

#[test]
fn start_and_stop_with_an_unreadable_tunnels_file_report_per_name() {
    let fixture = setup();
    let tunnels = fixture.env.home().join(".ssh/portico/tunnels");
    support::deny_read(&tunnels);
    let start = fixture.run(&["tunnel", "start", "db", "--json"]);
    let stop = fixture.run(&["tunnel", "stop", "db", "--json"]);
    let stop_all = fixture.run(&["tunnel", "stop", "--all", "--json"]);
    support::allow_read(&tunnels);

    for (output, name) in [(&start, "start"), (&stop, "stop")] {
        assert_eq!(output.status.code(), Some(1), "{name}");
        assert!(output.stderr.is_empty(), "{name}");
        let document: Value = serde_json::from_slice(&output.stdout).unwrap();
        let result = &document["results"][0];
        assert_eq!(result["name"], "db", "{name}");
        assert_eq!(result["outcome"], "failed", "{name}");
        assert_eq!(result["error"]["kind"], "other", "{name}");
        assert!(
            result["error"]["message"]
                .as_str()
                .unwrap()
                .contains("tunnels"),
            "{name}: {document}"
        );
        assert_eq!(result["tunnel"], Value::Null, "{name}");
    }
    // Recorded tunnels can still be stopped; with none running there is nothing to report.
    assert_eq!(stop_all.status.code(), Some(0));
    assert!(stop_all.stderr.is_empty());
    let document: Value = serde_json::from_slice(&stop_all.stdout).unwrap();
    assert_eq!(document, serde_json::json!({ "results": [], "schema": 1 }));
}
