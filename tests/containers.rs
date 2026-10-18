mod support;

use std::{ffi::OsStr, fs, path::PathBuf};

use support::TestEnv;

const DENIED: &str = "permission denied while trying to connect to the Docker daemon socket at unix:///var/run/docker.sock: dial unix /var/run/docker.sock: connect: permission denied";
const DAEMON_DOWN: &str = "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?";
const SOCKET_HOST: &str = "unix:///var/run/docker.sock";

/// One line of `docker ps` output for a container, in a Compose project when `compose` is
/// `Some((project, service, number))`.
fn ps_line(
    id: &str,
    name: &str,
    image: &str,
    status: &str,
    compose: Option<(&str, &str, &str)>,
) -> String {
    let (project, service, number) = compose.unwrap_or(("", "", ""));
    format!(
        r#"{{"id":"{id}","name":"{name}","image":"{image}","command":"","running_for":"","status":"{status}","ports":"","networks":"","mounts":"","project":"{project}","service":"{service}","number":"{number}","working_dir":""}}"#
    )
}

/// Two `shop` containers and one outside any project.
fn shop_listing() -> String {
    [
        ps_line(
            "aaaa1111",
            "shop-web-1",
            "nginx",
            "Up 2 hours",
            Some(("shop", "web", "1")),
        ),
        ps_line("cccc3333", "cache", "redis", "Up 5 minutes", None),
        ps_line(
            "bbbb2222",
            "shop-db-1",
            "postgres",
            "Up 2 hours",
            Some(("shop", "db", "1")),
        ),
    ]
    .join("\n")
}

/// Where the fakes of `env` write their logs.
fn log(env: &TestEnv, name: &str) -> PathBuf {
    env.bin().parent().unwrap().join(format!("{name}.calls"))
}

fn read_log(env: &TestEnv, name: &str) -> String {
    fs::read_to_string(log(env, name)).unwrap_or_default()
}

/// A `docker` that prints `listing` for `ps` and logs every call's arguments, one per line.
fn install_docker(env: &TestEnv, listing: &str) {
    env.install_program(
        "docker",
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\nif [ \"$1\" = ps ]; then\ncat <<'EOF'\n{listing}\nEOF\nfi\nexit 0\n",
            log(env, "docker").display()
        ),
    );
}

/// A `docker` that always fails with `message`.
fn install_failing_docker(env: &TestEnv, message: &str) {
    env.install_program(
        "docker",
        &format!("#!/bin/sh\necho '{message}' >&2\nexit 1\n"),
    );
}

/// Fakes the probe's view of the group database: `me` is in the process groups `me`, the
/// database groups `database`, and the socket belongs to `docker`.
fn install_probe(env: &TestEnv, database: &str) {
    env.install_program(
        "id",
        &format!(
            "#!/bin/sh\ncase \"$*\" in\n  -un) echo me ;;\n  -Gn) echo me ;;\n  '-Gn me') echo '{database}' ;;\nesac\n"
        ),
    );
    env.install_program("getent", "#!/bin/sh\nexit 0\n");
    env.install_program("stat", "#!/bin/sh\necho docker\n");
}

/// An initialised environment with the host `dev`, whose ssh logs its arguments and prints
/// `listing` for any remote command.
fn env_with_dev(listing: &str) -> TestEnv {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "dev", "-H", "dev.example"])
        .assert_success();
    env.install_program(
        "ssh",
        &format!(
            "#!/bin/sh\nif [ \"$1\" = '-G' ]; then printf 'hostname h\\nuser u\\nport 22\\n'; exit 0; fi\nprintf '%s\\n' \"$@\" >> '{}'\ncat <<'EOF'\n{listing}\nEOF\n",
            log(&env, "ssh").display()
        ),
    );
    env
}

fn run_with_socket<I, S>(env: &TestEnv, args: I) -> support::RunResult
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    env.run_with_env(args, "DOCKER_HOST", OsStr::new(SOCKET_HOST))
}

#[test]
fn lists_local_containers_grouped() {
    let env = TestEnv::new();
    install_docker(&env, &shop_listing());

    let result = env.run(["containers"]);
    let stdout = String::from_utf8_lossy(&result.0.stdout).into_owned();
    result.assert_success();
    assert_eq!(
        stdout,
        "shop\n  db   shop-db-1   postgres  Up 2 hours\n  web  shop-web-1  nginx     Up 2 hours\nOther\n  cache  cache  redis  Up 5 minutes\n"
    );
    let position = |text: &str| stdout.find(text).unwrap();
    assert!(position("shop") < position("  db"));
    assert!(position("  db") < position("  web"));
    assert!(position("  web") < position("Other"));
}

#[test]
fn lists_remote_containers_over_ssh() {
    let env = env_with_dev(&shop_listing());

    env.run(["containers", "--on", "dev"])
        .assert_stdout_contains("shop\n  db ");
    let arguments = read_log(&env, "ssh");
    assert!(
        arguments.starts_with(
            "-o\nRemoteCommand=none\n--\ndev\ndocker ps --no-trunc --format '{\"id\":"
        ),
        "{arguments}"
    );
    assert!(!arguments.contains("BatchMode"), "{arguments}");
}

#[test]
fn alias_case_uses_inventory_spelling() {
    let env = env_with_dev(&shop_listing());

    env.run(["containers", "--on", "DEV"]).assert_success();
    assert!(read_log(&env, "ssh").contains("--\ndev\n"));
}

#[test]
fn no_containers_message() {
    let env = TestEnv::new();
    install_docker(&env, "");

    env.run(["containers"])
        .assert_stdout_contains("No running containers on this machine.\n");
}

#[test]
fn docker_missing_is_explained() {
    let env = TestEnv::new();

    env.run_with_env(["containers"], "PATH", env.bin().as_os_str())
        .assert_exit_code_and_stderr_contains(
            1,
            "Docker is not installed on this machine, or it is not on the PATH of a non-interactive shell there.\n  Install Docker from the distribution's packages, then list again.\n",
        );
}

#[test]
fn daemon_down_offers_start_and_declines_non_interactively() {
    let env = TestEnv::new();
    install_failing_docker(&env, DAEMON_DOWN);
    env.install_program("systemctl", "#!/bin/sh\nexit 0\n");

    let result = run_with_socket(&env, ["containers"]);
    assert!(result.0.stdout.is_empty(), "prompted non-interactively");
    result.assert_exit_code_and_stderr_contains(
        1,
        "The Docker daemon is not running on this machine.\n  Start it with: sudo systemctl start docker\n",
    );
}

#[test]
fn permission_denied_rerun_through_sg() {
    let env = TestEnv::new();
    env.install_program(
        "docker",
        &format!(
            "#!/bin/sh\nif [ -n \"$IN_DOCKER_GROUP\" ]; then\ncat <<'EOF'\n{}\nEOF\nexit 0\nfi\necho '{DENIED}' >&2\nexit 1\n",
            shop_listing()
        ),
    );
    env.install_program(
        "sg",
        "#!/bin/sh\n[ \"$1\" = docker ] && [ \"$2\" = -c ] || exit 2\nIN_DOCKER_GROUP=1 exec sh -c \"$3\"\n",
    );
    install_probe(&env, "me docker");

    let result = run_with_socket(&env, ["containers"]);
    let stdout = String::from_utf8_lossy(&result.0.stdout).into_owned();
    result.assert_success();
    assert!(stdout.starts_with("shop\n  db "), "{stdout}");
    assert!(
        stdout.ends_with(
            "Ran docker through `sg docker`: other shells need `newgrp docker` or a new login.\n"
        ),
        "{stdout}"
    );
}

#[test]
fn join_offer_declined_lists_options() {
    let env = TestEnv::new();
    install_failing_docker(&env, DENIED);
    install_probe(&env, "me");

    let result = run_with_socket(&env, ["containers"]);
    assert!(result.0.stdout.is_empty(), "prompted non-interactively");
    result.assert_exit_code_and_stderr_contains(
        1,
        "me cannot use Docker on this machine: permission denied on /var/run/docker.sock.\n  Add me to the docker group (root-equivalent access): sudo sh -c 'groupadd -f docker && usermod -aG docker \"$1\" && chgrp docker \"$2\"' sh me /var/run/docker.sock\n  Or use rootless Docker: https://docs.docker.com/engine/security/rootless/\n  Or run sudo docker yourself\n",
    );
}

#[test]
fn listing_ssh_failure_is_explained() {
    let env = env_with_dev("");
    env.install_program(
        "ssh",
        "#!/bin/sh\nif [ \"$1\" = '-G' ]; then printf 'hostname h\\nuser user\\nport 22\\n'; exit 0; fi\nprintf 'user@h: Permission denied (publickey).\\n' >&2\nexit 255\n",
    );

    env.run(["containers", "--on", "dev"])
        .assert_exit_code_and_stderr_contains(
            1,
            "user@h: Permission denied (publickey).\n\ndev: failed \u{2014} the server refused the login\n",
        );
}
