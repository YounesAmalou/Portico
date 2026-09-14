mod support;

use std::os::unix::fs::PermissionsExt;

use portico::managed::ManagedConfig;
use support::TestEnv;

#[test]
fn ordinary_openssh_resolves_a_portico_host() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run([
        "add",
        "dev",
        "-H",
        "203.0.113.8",
        "-u",
        "deploy",
        "-p",
        "2222",
    ])
    .assert_success();
    let entry = std::fs::read_to_string(env.ssh_config()).unwrap();
    let validation = env.ssh_config().with_file_name("acceptance-config");
    std::fs::write(
        &validation,
        entry.replace(
            "~/.ssh/portico/config",
            env.managed_config().to_str().unwrap(),
        ),
    )
    .unwrap();

    let output = std::process::Command::new("ssh")
        .args(["-G", "-F"])
        .arg(validation)
        .arg("dev")
        .env("HOME", env.home())
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "ssh -G failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("hostname 203.0.113.8"), "{stdout}");
    assert!(stdout.contains("user deploy"), "{stdout}");
    assert!(stdout.contains("port 2222"), "{stdout}");
}

#[test]
fn managed_strings_round_trip_through_openssh() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    for identity in [
        r#"/tmp/a"b"c"#,
        r#""key""#,
        "/tmp/a'b'c",
        r"/tmp/a\b",
        "/tmp/#key",
        "/tmp/a b",
    ] {
        env.run([
            "add",
            "literal",
            "-H",
            "203.0.113.8",
            "-u",
            r#"a"b"c"#,
            "-i",
            identity,
            "-J",
            "a'b'@jump.example",
        ])
        .assert_success();
        let output = std::process::Command::new("ssh")
            .args(["-G", "-F"])
            .arg(env.managed_config())
            .args(["--", "literal"])
            .env("HOME", env.home())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout
                .lines()
                .any(|line| line == format!("identityfile {identity}")),
            "{stdout}"
        );
        assert!(
            stdout.lines().any(|line| line == "user a\"b\"c"),
            "{stdout}"
        );
        assert!(
            stdout
                .lines()
                .any(|line| line == "proxyjump a'b'@jump.example"),
            "{stdout}"
        );
        let managed =
            ManagedConfig::parse(&std::fs::read_to_string(env.managed_config()).unwrap()).unwrap();
        assert_eq!(
            managed.find("literal").unwrap().identity_file(),
            Some(identity)
        );
        env.run(["remove", "literal", "-f"]).assert_success();
    }
}

#[test]
fn symlinked_entry_discovery_agrees_with_openssh() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.home().join(".ssh")).unwrap();
    let target = env.home().join("dotfiles/config");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "Include relative.conf ~/.ssh/tilde.conf\n").unwrap();
    std::fs::write(
        env.home().join(".ssh/relative.conf"),
        "Host 'relative'\n HostName 203.0.113.9\n",
    )
    .unwrap();
    std::fs::write(
        env.home().join(".ssh/tilde.conf"),
        "Host 'tilde'\n HostName 203.0.113.10\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&target, env.ssh_config()).unwrap();
    let inventory = portico::discovery::discover(&env.ssh_config(), &env.managed_config()).unwrap();
    assert!(inventory.aliases_complete, "{:?}", inventory.warnings);
    assert_eq!(
        inventory
            .hosts
            .iter()
            .map(|host| host.alias.as_str())
            .collect::<Vec<_>>(),
        ["relative", "tilde"]
    );
    for (alias, expected) in [
        ("relative", "hostname 203.0.113.9"),
        ("tilde", "hostname 203.0.113.10"),
    ] {
        let output = std::process::Command::new("ssh")
            .args(["-G", "-F"])
            .arg(env.ssh_config())
            .arg(alias)
            .env("HOME", env.home())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line == expected),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn literal_hash_alias_discovery_agrees_with_openssh() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.home().join(".ssh")).unwrap();
    for header in ["Host ignored# prod", "Host ''# prod", "Host \"\"# prod"] {
        std::fs::write(
            env.ssh_config(),
            format!("{header}\n HostName 203.0.113.11\n"),
        )
        .unwrap();
        let output = std::process::Command::new("ssh")
            .args(["-G", "-F"])
            .arg(env.ssh_config())
            .arg("prod")
            .env("HOME", env.home())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line == "hostname 203.0.113.11")
        );

        let discovery =
            portico::discovery::discover(&env.ssh_config(), &env.managed_config()).unwrap();

        assert!(
            discovery.hosts.iter().any(|host| host.alias == "prod"),
            "{header}: {discovery:?}"
        );
    }
}

#[test]
fn tui_paste_and_terminal_lifecycle_work_on_a_real_pty() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "fail", "-H", "fail.example"])
        .assert_success();
    std::fs::write(env.bin().join("ssh"), "#!/bin/sh\nif [ \"$1\" = '-G' ]; then\n  if [ \"$2\" != '-F' ] && [ -f \"$HOME/block-resolver\" ]; then\n    : > \"$HOME/resolver-started\"\n    sleep 4\n  fi\n  printf 'hostname fail.example\\nport 22\\n'\n  exit 0\nfi\nstty -g > \"$HOME/child-terminal-state\"\nexit 23\n").unwrap();

    let output = std::process::Command::new("python3")
        .arg("-I")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/tui_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_portico"))
        .arg(env.home())
        .arg(env.bin())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "PTY harness failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("{}", String::from_utf8_lossy(&output.stdout));
}

#[test]
fn concurrent_adds_do_not_lose_completed_updates() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|index| {
                let barrier = &barrier;
                let env = &env;
                scope.spawn(move || {
                    let alias = format!("host-{index}");
                    let hostname = format!("host-{index}.example");
                    barrier.wait();
                    env.run(["add", alias.as_str(), "-H", hostname.as_str()])
                        .assert_success();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    });

    let managed =
        ManagedConfig::parse(&std::fs::read_to_string(env.managed_config()).unwrap()).unwrap();
    assert_eq!(managed.hosts().len(), 8);
    for (alias, hostname) in [
        ("host-0", "host-0.example"),
        ("host-1", "host-1.example"),
        ("host-2", "host-2.example"),
        ("host-3", "host-3.example"),
        ("host-4", "host-4.example"),
        ("host-5", "host-5.example"),
        ("host-6", "host-6.example"),
        ("host-7", "host-7.example"),
    ] {
        assert_eq!(
            managed.find(alias).map(|host| host.hostname()),
            Some(hostname)
        );
    }
}

#[test]
fn rejected_candidate_preserves_active_and_backup_bytes() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "good", "-H", "good.example"])
        .assert_success();
    let active_before = std::fs::read(env.managed_config()).unwrap();
    let backup_path = env.managed_config().with_file_name("config.bak");
    let backup_before = std::fs::read(&backup_path).ok();
    env.reject_alias("rejected");

    env.run(["add", "rejected", "-H", "rejected.example"])
        .assert_failure_stderr_contains("rejected alias: rejected");

    assert_eq!(std::fs::read(env.managed_config()).unwrap(), active_before);
    assert_eq!(std::fs::read(&backup_path).ok(), backup_before);
}

#[test]
fn approved_v1_flow_works_in_an_isolated_home() {
    let env = TestEnv::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let external = env.home().join(".ssh/external.conf");
        let external_before = b"# User-owned host.\nHost legacy\n  HostName 198.51.100.20\n  User operator\n  Port 2200\n";
        let entry_before = b"# Existing SSH configuration.\nInclude external.conf\n";
        let expected_entry = b"# Existing SSH configuration.\nInclude ~/.ssh/portico/config\nInclude external.conf\n";
        std::fs::create_dir_all(external.parent().unwrap()).unwrap();
        std::fs::set_permissions(
            external.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::write(env.ssh_config(), entry_before).unwrap();
        std::fs::write(&external, external_before).unwrap();

        env.run(["init", "-y"]).assert_success();
        assert_eq!(std::fs::read(env.ssh_config()).unwrap(), expected_entry);
        assert_eq!(std::fs::read(&external).unwrap(), external_before);
        env.run(["add", "dev", "-H", "dev.example", "-u", "deploy"])
            .assert_success();
        env.run(["hosts"]).assert_stdout_contains("dev  managed");
        env.run(["show", "dev"])
            .assert_stdout_contains("Declared HostName: dev.example");
        env.run(["edit", "dev", "-p", "2222"]).assert_success();
        env.run(["show", "dev"])
            .assert_stdout_contains("Declared Port: 2222");
        env.run(["connect", "dev", "-n"]).assert_success();
        assert!(
            std::fs::read_to_string(env.connection_log())
                .unwrap()
                .contains("--\nssh\n--\ndev")
        );
        assert_eq!(file_mode(env.home().join(".ssh")), Some(0o700));
        assert_eq!(
            file_mode(env.managed_config().parent().unwrap()),
            Some(0o700)
        );
        assert_eq!(file_mode(env.managed_config()), Some(0o600));
        assert_eq!(
            file_mode(env.managed_config().with_file_name("config.bak")),
            Some(0o600)
        );
        env.run(["remove", "dev", "-f"]).assert_success();
        let managed =
            ManagedConfig::parse(&std::fs::read_to_string(env.managed_config()).unwrap()).unwrap();
        assert!(managed.hosts().is_empty());

        let entry = std::fs::read_to_string(env.ssh_config()).unwrap();
        assert_eq!(entry.as_bytes(), expected_entry);
        assert_eq!(std::fs::read(&external).unwrap(), external_before);
        let validation = env.ssh_config().with_file_name("acceptance-config");
        std::fs::write(
            &validation,
            entry.replace(
                "~/.ssh/portico/config",
                env.managed_config().to_str().unwrap(),
            ),
        )
        .unwrap();
        let output = std::process::Command::new("ssh")
            .args(["-G", "-F"])
            .arg(validation)
            .arg("legacy")
            .env("HOME", env.home())
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            output.status.success(),
            "ssh -G failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("hostname 198.51.100.20"), "{stdout}");
        assert!(stdout.contains("user operator"), "{stdout}");
        assert!(stdout.contains("port 2200"), "{stdout}");
    }));

    if let Err(payload) = result {
        let backup = env.managed_config().with_file_name("config.bak");
        eprintln!("temporary home: {}", env.home().display());
        eprintln!("entry config:\n{}", read_or_error(env.ssh_config()));
        eprintln!("managed config:\n{}", read_or_error(env.managed_config()));
        eprintln!("ssh mode: {:?}", file_mode(env.home().join(".ssh")));
        eprintln!(
            "portico mode: {:?}",
            file_mode(env.managed_config().parent().unwrap())
        );
        eprintln!("managed mode: {:?}", file_mode(env.managed_config()));
        eprintln!("backup:\n{}", read_or_error(backup));
        eprintln!("ssh -G -F:\n{}", openssh_diagnostics(&env));
        std::panic::resume_unwind(payload);
    }
}

fn read_or_error(path: impl AsRef<std::path::Path>) -> String {
    std::fs::read_to_string(path.as_ref()).unwrap_or_else(|error| format!("<{error}>"))
}

fn file_mode(path: impl AsRef<std::path::Path>) -> Option<u32> {
    std::fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o777)
}

fn openssh_diagnostics(env: &TestEnv) -> String {
    let entry = match std::fs::read_to_string(env.ssh_config()) {
        Ok(entry) => entry,
        Err(error) => return format!("could not read entry config: {error}"),
    };
    let validation = env.ssh_config().with_file_name("acceptance-config");
    if let Err(error) = std::fs::write(
        &validation,
        entry.replace(
            "~/.ssh/portico/config",
            env.managed_config().to_str().unwrap(),
        ),
    ) {
        return format!("could not write validation config: {error}");
    }
    match std::process::Command::new("ssh")
        .args(["-G", "-F"])
        .arg(validation)
        .arg("legacy")
        .env("HOME", env.home())
        .output()
    {
        Ok(output) => format!(
            "status: {}\nstdout:\n{}stderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => format!("could not run ssh: {error}"),
    }
}
