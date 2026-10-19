//! `--json`: the documents of `docs/json-output.md`, pinned by the golden files in
//! `tests/fixtures/json/`. `PORTICO_BLESS=1 cargo test --test json` rewrites them.

mod support;

use std::{
    ffi::OsStr,
    fs,
    io::Write,
    net::TcpListener,
    os::unix::{fs::PermissionsExt, net::UnixListener},
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
    /// The agent's socket.
    _agent: UnixListener,
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
if [ "$1" = -f ]; then
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

/// `docker ps` lines: a Compose service and a container outside any project, whose command
/// carries a secret.
fn listing() -> String {
    [
                r#"{"id":"4f1c2a9b7d3e","name":"shop-web-1","image":"nginx:1.27","command":"nginx -g daemon","running_for":"3 hours ago","status":"Up 3 hours","ports":"80/tcp","networks":"shop_default","mounts":"/srv/shop","project":"shop","service":"web","number":"1","working_dir":"/srv/shop"}"#
        .to_owned(),
        format!(
            r#"{{"id":"9a8b7c6d5e4f","name":"cache","image":"redis:7","command":"redis-server --requirepass={SECRET}","running_for":"5 minutes ago","status":"Up 5 minutes","ports":"","networks":"bridge","mounts":"","project":"","service":"","number":"","working_dir":""}}"#
        ),
    ]
    .join("\n")
}

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
    fs::set_permissions(ssh.join("id_ed25519"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        ssh.join("id_ed25519.pub"),
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIprod deploy@prod\n",
    )
    .unwrap();
    fs::create_dir_all(env.home().join(".1password")).unwrap();
    let agent = UnixListener::bind(env.home().join(".1password/agent.sock")).unwrap();
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
        &format!("#!/bin/sh\ncat <<'EOF'\n{}\nEOF\n", listing()),
    );
    Fixture {
        env,
        _busy: busy,
        busy_port,
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
fn normalize(text: &str, home: &Path, busy_port: Option<u16>) -> Value {
    let mut text = text.replace(&home.display().to_string(), HOME);
    if let Some(port) = busy_port {
        text = text.replace(&port.to_string(), BUSY_PORT);
    }
    let mut document: Value = serde_json::from_str(&text).unwrap();
    fn walk(value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (key, value) in map.iter_mut() {
                    match (key.as_str(), &value) {
                        ("pid", Value::Number(_)) => *value = 41237.into(),
                        ("started", Value::Number(_)) => *value = 1781860921u64.into(),
                        ("finished", Value::Number(_)) => *value = 1781861043u64.into(),
                        _ => walk(value),
                    }
                }
            }
            Value::Array(values) => values.iter_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut document);
    document
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
    assert_eq!(states, ["port-in-use", "stopped", "failed", "stopped"]);
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
    fs::set_permissions(&config, fs::Permissions::from_mode(0o000)).unwrap();
    for args in [
        &["hosts", "--json"][..],
        &["show", "prod", "--json"],
        &["identities", "--json"],
        &["containers", "--on", "prod", "--json"],
    ] {
        let output = fixture.run(args);
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(output.stderr.is_empty(), "{args:?}");
        let document: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["error"]["kind"], "other", "{args:?}");
        assert!(
            document["error"]["message"]
                .as_str()
                .unwrap()
                .contains(".ssh/config"),
            "{args:?}: {document}"
        );
        fs::set_permissions(&config, fs::Permissions::from_mode(0o000)).unwrap();
    }
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn json_is_refused_where_it_is_not_offered() {
    let fixture = setup();
    let output = fixture.run(&["add", "x", "-H", "x", "--json"]);
    assert_eq!(output.status.code(), Some(2));
}
