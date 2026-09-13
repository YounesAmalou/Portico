mod support;

use std::os::unix::fs::PermissionsExt;

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

    let managed = std::fs::read_to_string(env.managed_config()).unwrap();
    for index in 0..8 {
        assert!(managed.contains(&format!("Host host-{index}\n")));
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
        .assert_failure();

    assert_eq!(std::fs::read(env.managed_config()).unwrap(), active_before);
    assert_eq!(std::fs::read(&backup_path).ok(), backup_before);
}

#[test]
fn approved_v1_flow_works_in_an_isolated_home() {
    let env = TestEnv::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        env.run(["init", "-y"]).assert_success();
        env.run(["add", "dev", "-H", "dev.example", "-u", "deploy"])
            .assert_success();
        env.run(["hosts"]).assert_stdout_contains("dev  managed");
        env.run(["show", "dev"])
            .assert_stdout_contains("Declared HostName: dev.example");
        env.run(["edit", "dev", "-p", "2222"]).assert_success();
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

        let entry = std::fs::read_to_string(env.ssh_config()).unwrap();
        assert_eq!(entry, "Include ~/.ssh/portico/config\n");
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
            .arg("ordinary.example")
            .env("HOME", env.home())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "ssh -G failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
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
        .arg("ordinary.example")
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
