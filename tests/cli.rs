mod support;

use support::TestEnv;

#[test]
fn review_raw_comments_survive_init_and_host_operations() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.ssh_config().parent().unwrap()).unwrap();
    let original = b"# caf\xe9\r\nHost legacy # ann\xff\n HostName legacy.example\n";
    std::fs::write(env.ssh_config(), original).unwrap();
    env.run(["init", "-y"]).assert_success();
    env.run(["hosts"])
        .assert_stdout_contains("legacy  external");
    env.run(["add", "dev", "-H", "dev.example"])
        .assert_success();
    env.run(["show", "dev"])
        .assert_stdout_contains("dev.example");
    env.run(["edit", "dev", "-u", "deploy"]).assert_success();
    env.run(["remove", "dev", "-f"]).assert_success();
    assert_eq!(
        std::fs::read(env.ssh_config())
            .unwrap()
            .strip_prefix(b"Include ~/.ssh/portico/config\n")
            .unwrap(),
        original
    );
    let undecodable =
        b"Include ~/.ssh/portico/config\nHost undecodable\xff\n HostName legacy.example\n";
    std::fs::write(env.ssh_config(), undecodable).unwrap();
    env.run(["add", "other", "-H", "other.example"])
        .assert_failure_stderr_contains("host inventory is incomplete");
    assert_eq!(std::fs::read(env.ssh_config()).unwrap(), undecodable);
}

#[test]
fn commands_before_initialization_explain_how_to_continue() {
    for command in [
        vec!["hosts"],
        vec!["show", "dev"],
        vec!["add", "dev", "-H", "dev.example"],
        vec!["edit", "dev", "-u", "deploy"],
        vec!["remove", "dev"],
        vec!["connect", "dev"],
    ] {
        for entry in [
            None,
            Some("Host legacy\n"),
            Some("# comment\nInclude ~/.ssh/portico/config\n"),
            Some("Include ~/.ssh/portico/config\nInclude portico/config\n"),
            Some("Host *\n Include ~/.ssh/portico/config\n"),
            Some("Include ~/.ssh/portico/config other.conf\n"),
            Some("Include portico/config\n"),
            Some("Include ~/.ssh/portico/config\n"),
        ] {
            let env = TestEnv::new();
            std::fs::create_dir_all(env.managed_config().parent().unwrap()).unwrap();
            if let Some(contents) = entry {
                std::fs::write(env.ssh_config(), contents).unwrap();
            }
            if entry != Some("Include ~/.ssh/portico/config\n") {
                std::fs::write(env.managed_config(), "Host dev\n HostName dev.example\n").unwrap();
            }
            env.run(&command).assert_failure_stderr_contains(
                "setup requires ~/.ssh/config, ~/.ssh/portico/config, and one exact top-level `Include ~/.ssh/portico/config` on the first line; run `portico init`",
            );
            assert!(!env.connection_log().exists());
        }
    }
}

#[test]
fn add_show_edit_and_force_remove_flow() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "dev", "-H", "dev.example", "-u", "deploy"])
        .assert_success();
    env.run(["show", "dev"])
        .assert_stdout_contains("dev.example");
    env.run(["edit", "dev", "-p", "2222", "-U", "user"])
        .assert_success();
    env.run(["edit", "dev", "-a", "prod"])
        .assert_stdout_contains("Updated prod.");
    env.run(["remove", "prod"])
        .assert_failure_stderr_contains("--force");
    env.run(["remove", "prod", "-f"]).assert_success();
    env.run(["show", "dev"]).assert_failure();
}

#[test]
fn show_preserves_declared_details_when_resolution_fails() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "dev", "-H", "dev.example"])
        .assert_success();
    env.reject_alias("dev");

    env.run(["show", "dev"])
        .assert_stdout_contains("Declared HostName: dev.example");
    env.run(["show", "dev"])
        .assert_stdout_contains("Resolution error:");
    env.run(["show", "dev"])
        .assert_stdout_contains("rejected alias: dev");
}

#[test]
fn lists_ownership_and_rejects_external_mutation() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    std::fs::write(
        env.ssh_config(),
        "Include ~/.ssh/portico/config\nHost legacy\n  HostName legacy.example\n",
    )
    .unwrap();
    env.run(["hosts"])
        .assert_stdout_contains("legacy  external");
    env.run(["remove", "legacy", "-f"])
        .assert_failure_stderr_contains("read-only");
}

#[test]
fn hosts_reports_discovery_warnings_even_without_rows() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    std::fs::write(
        env.ssh_config(),
        "Include ~/.ssh/portico/config\nInclude missing.conf\n",
    )
    .unwrap();

    env.run(["hosts"]).assert_stdout_contains("Warning:");
    env.run(["hosts"]).assert_stdout_contains("missing.conf");
}

#[test]
fn expanded_include_alias_blocks_collision() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    std::fs::write(
        env.home().join(".ssh/external.conf"),
        "Host expanded\n  HostName expanded.example\n",
    )
    .unwrap();
    std::fs::write(
        env.ssh_config(),
        "Include ~/.ssh/portico/config\nInclude ${HOME}/.ssh/external.conf\n",
    )
    .unwrap();

    env.run(["add", "expanded", "-H", "other.example"])
        .assert_failure_stderr_contains("alias already exists");
}

#[test]
fn escaped_expanded_include_makes_collision_discovery_incomplete() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    let external = env.home().join(".ssh/external.conf");
    std::fs::write(&external, "Host escaped\n  HostName escaped.example\n").unwrap();
    std::fs::write(
        env.ssh_config(),
        "Include ~/.ssh/portico/config\nInclude ${PORTICO_ESCAPED_INCLUDE}\n",
    )
    .unwrap();
    let escaped = external.to_string_lossy().replace(".conf", "\\.conf");

    env.run_with_env(
        ["add", "escaped", "-H", "other.example"],
        "PORTICO_ESCAPED_INCLUDE",
        escaped.as_ref(),
    )
    .assert_failure_stderr_contains("host inventory is incomplete");
}

#[test]
fn connection_modes_propagate_status_and_arguments() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "dev", "-H", "dev.example"])
        .assert_success();
    env.run(["connect", "dev"]).assert_success();
    env.run(["connect", "dev", "-n"]).assert_success();
    let log = std::fs::read_to_string(env.connection_log()).unwrap();
    assert!(log.contains("dev"));
    assert!(log.contains("ssh\n--\ndev"));
}

#[test]
fn non_interactive_changes_require_explicit_approval() {
    let env = TestEnv::new();
    env.run(["init"]).assert_failure_stderr_contains("--yes");
    assert!(!env.managed_config().exists());
}

#[test]
fn connection_exit_code_is_propagated() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "fail", "-H", "fail.example"])
        .assert_success();
    env.run(["connect", "fail"]).assert_exit_code(23);
}

#[test]
fn new_terminal_detaches_launcher_streams() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "dev", "-H", "dev.example"])
        .assert_success();
    env.install_program(
        "xdg-terminal-exec",
        "#!/bin/sh\nprintf 'launcher-noise\\n'\nprintf 'launcher-noise\\n' >&2\nsleep 2\n",
    );
    let started = std::time::Instant::now();

    let result = env.run(["connect", "dev", "-n"]);

    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "captured call waited {:?}",
        started.elapsed()
    );
    result.assert_success_without_output("launcher-noise");
}

#[test]
fn prompt_terminal_launcher_failure_is_propagated() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "dev", "-H", "dev.example"])
        .assert_success();
    env.install_program("xdg-terminal-exec", "#!/bin/sh\nexit 23\n");

    env.run(["connect", "dev", "-n"]).assert_exit_code(23);
}

#[test]
fn edit_rejects_setting_and_unsetting_the_same_field() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "dev", "-H", "dev.example", "-u", "deploy"])
        .assert_success();
    for (flag, field, value) in [
        ("-u", "user", "other"),
        ("-p", "port", "2222"),
        ("-i", "identity", "~/.ssh/id_dev"),
        ("-J", "jump", "bastion"),
    ] {
        env.run(["edit", "dev", flag, value, "-U", field])
            .assert_failure_stderr_contains(&format!("--unset {field} conflicts with --{field}"));
    }
    env.run(["show", "dev"])
        .assert_stdout_contains("Declared User: deploy");
}

#[test]
fn remove_validates_the_alias_before_prompting() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    std::fs::write(
        env.ssh_config(),
        "Include ~/.ssh/portico/config\nHost legacy\n  HostName legacy.example\n",
    )
    .unwrap();

    env.run(["remove", "missing"])
        .assert_failure_stderr_contains("unknown host: missing");
    env.run(["remove", "legacy"])
        .assert_failure_stderr_contains("read-only");
}

#[test]
fn blank_or_padded_jump_and_identity_values_are_never_written() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    for (flag, value) in [
        ("-J", ""),
        ("-J", " "),
        ("-J", " bastion"),
        ("-J", "bastion "),
        ("-J", "one, two"),
        ("-i", " "),
        ("-i", " ~/.ssh/id_dev"),
        ("-i", "~/.ssh/id_dev "),
    ] {
        env.run(["add", "dev", "-H", "dev.example", flag, value])
            .assert_failure();
        env.run(["show", "dev"]).assert_failure();
    }
    env.run(["add", "dev", "-H", "dev.example"])
        .assert_success();
    for (flag, value) in [("-J", "bastion "), ("-i", " ")] {
        env.run(["edit", "dev", flag, value]).assert_failure();
    }
    env.run(["show", "dev"])
        .assert_stdout_contains("dev.example");
}

fn fixture(name: &str) -> String {
    format!(
        "{}/tests/fixtures/ssh/{name}.txt",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// A fake `ssh`: `-G` resolves `dev` to deploy@h; a key fetch records a key in
/// the file it is given; a probe prints `trusted` once `~/.ssh/known_hosts`
/// exists, else `untrusted`.
fn install_fake_ssh(env: &TestEnv, trusted: &str, untrusted: &str) {
    env.install_program(
        "ssh",
        &format!(
            "#!/bin/sh\nif [ \"$1\" = '-G' ]; then printf 'hostname h\\nuser deploy\\nport 22\\nuserknownhostsfile ~/.ssh/known_hosts\\n'; exit 0; fi\nkh=; prev=\nfor a do\n  case \"$prev$a\" in -oUserKnownHostsFile=*) kh=${{a#UserKnownHostsFile=}} ;; esac\n  prev=; [ \"$a\" = -o ] && prev=-o\ndone\nif [ -n \"$kh\" ]; then echo 'h ssh-ed25519 AAAAkey' >> \"$kh\"; exit 255; fi\nif [ -f \"$HOME/.ssh/known_hosts\" ]; then\n{trusted}\nelse\n{untrusted}\nfi\n"
        ),
    );
}

fn install_fake_keygen(env: &TestEnv) {
    env.install_program(
        "ssh-keygen",
        "#!/bin/sh\nprintf '256 SHA256:abc h (ED25519)\\n'\n",
    );
}

fn env_with_dev() -> TestEnv {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "dev", "-H", "h.example"]).assert_success();
    env
}

fn known_hosts(env: &TestEnv) -> std::path::PathBuf {
    env.home().join(".ssh/known_hosts")
}

fn unknown_key_env() -> TestEnv {
    let env = env_with_dev();
    install_fake_keygen(&env);
    install_fake_ssh(
        &env,
        "printf 'Authenticated to 10.0.0.5 ([10.0.0.5]:22) using \"publickey\".\\n' >&2",
        &format!("cat '{}' >&2; exit 255", fixture("host-key-unknown")),
    );
    env
}

#[test]
fn test_reports_success_and_exits_zero() {
    let env = env_with_dev();
    install_fake_ssh(
        &env,
        "printf 'Authenticated to 10.0.0.5 ([10.0.0.5]:22) using \"publickey\".\\n' >&2",
        "true",
    );
    std::fs::write(known_hosts(&env), "").unwrap();
    env.run(["test", "dev"])
        .assert_stdout_contains("dev: authenticated as deploy@10.0.0.5:22 (publickey)");
}

#[test]
fn test_reports_each_failure_as_an_explanation_and_exits_one() {
    let env = env_with_dev();
    let refused = format!("cat '{}' >&2; exit 255", fixture("auth-refused"));
    install_fake_ssh(&env, &refused, &refused);
    env.run(["test", "dev"]).assert_exit_code_and_stdout_contains(
        1,
        "dev: failed \u{2014} the server refused the login\n  user:    deploy@h:22\n  offered: /tmp/portico-fixture/wrong (file)\n  next:    check User and IdentityFile for dev, and that the public key is in the\n           server's authorized_keys\n",
    );
}

#[test]
fn test_raw_prints_what_ssh_said() {
    let env = env_with_dev();
    let refused = "printf 'user@h: Permission denied (publickey).\\n' >&2; exit 255";
    install_fake_ssh(&env, refused, refused);
    let result = env.run(["test", "x", "--raw"]);
    result.assert_failure_stderr_contains("unknown host: x");
    let result = env.run(["test", "dev", "--raw"]);
    result.assert_exit_code_and_stdout_contains(
        1,
        "  ssh said:\n    user@h: Permission denied (publickey).\n",
    );
}

#[test]
fn test_of_an_unknown_host_is_an_error() {
    let env = env_with_dev();
    env.run(["test", "nope"])
        .assert_failure_stderr_contains("error: unknown host: nope");
}

#[test]
fn non_interactive_unknown_key_prints_the_fingerprint_and_never_writes() {
    let env = unknown_key_env();
    let result = env.run(["test", "dev"]);
    result.assert_exit_code_and_stdout_contains(1, "SHA256:abc");
    env.run(["test", "dev"])
        .assert_exit_code_and_stdout_contains(
            1,
            "hint: pass --accept-host-key SHA256:abc to trust it",
        );
    env.run(["test", "dev"]).assert_exit_code_and_stdout_contains(
        1,
        "dev: host key not trusted\nh presents an ED25519 key with fingerprint SHA256:abc\nCompare it with the fingerprint from the server's administrator or console.\n",
    );
    assert!(!known_hosts(&env).exists());
}

#[test]
fn a_matching_accept_host_key_trusts_and_retests() {
    let env = unknown_key_env();
    env.run(["test", "dev", "--accept-host-key", "SHA256:abc"])
        .assert_stdout_contains("dev: authenticated as deploy@10.0.0.5:22 (publickey)");
    assert!(
        std::fs::read_to_string(known_hosts(&env))
            .unwrap()
            .contains("h ssh-ed25519 AAAAkey")
    );
}

#[test]
fn a_mismatching_accept_host_key_writes_nothing() {
    let env = unknown_key_env();
    env.run(["test", "dev", "--accept-host-key", "SHA256:other"])
        .assert_exit_code_and_stdout_contains(
            1,
            "dev: failed \u{2014} the host key fingerprint does not match --accept-host-key",
        );
    assert!(!known_hosts(&env).exists());
}

#[test]
fn external_hosts_can_be_tested() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    let mut config = std::fs::read_to_string(env.ssh_config()).unwrap();
    config.push_str("Host legacy\n HostName legacy.example\n");
    std::fs::write(env.ssh_config(), config).unwrap();
    install_fake_ssh(
        &env,
        "printf 'Authenticated to 10.0.0.5 ([10.0.0.5]:22) using \"publickey\".\\n' >&2",
        "true",
    );
    std::fs::write(known_hosts(&env), "").unwrap();
    env.run(["test", "legacy"])
        .assert_stdout_contains("legacy: authenticated as deploy@10.0.0.5:22 (publickey)");
}

fn alive(pid: libc::pid_t) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit(')')
                .next()
                .map(|rest| rest.trim_start().to_owned())
        })
        .is_some_and(|state| !state.starts_with('Z'))
}

#[test]
fn interrupting_a_test_stops_ssh_and_exits_130() {
    use std::{
        io::Read,
        thread,
        time::{Duration, Instant},
    };

    let env = env_with_dev();
    let pid_file = env.home().join("ssh.pid");
    env.install_program(
        "ssh",
        &format!(
            "#!/bin/sh\nif [ \"$1\" = '-G' ]; then printf 'hostname h\\nuser deploy\\nport 22\\n'; exit 0; fi\nprintf '%s' \"$$\" > '{0}.tmp'\nmv '{0}.tmp' '{0}'\nexec sleep 30\n",
            pid_file.display()
        ),
    );
    let mut portico = env.spawn(["test", "dev"]);
    let started = Instant::now();
    let ssh: libc::pid_t = loop {
        if let Ok(pid) = std::fs::read_to_string(&pid_file) {
            break pid.parse().unwrap();
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "ssh never started"
        );
        thread::sleep(Duration::from_millis(10));
    };

    // SAFETY: plain kill(2) on a child this test started.
    assert_eq!(
        unsafe { libc::kill(portico.id() as libc::pid_t, libc::SIGINT) },
        0
    );

    let status = loop {
        if let Some(status) = portico.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(10) {
            let _ = portico.kill();
            panic!("portico ignored SIGINT");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let mut stdout = String::new();
    portico
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    let ssh_gone = Instant::now();
    while alive(ssh) {
        if ssh_gone.elapsed() > Duration::from_secs(5) {
            // SAFETY: as above; never leave the fake behind.
            unsafe { libc::kill(ssh, libc::SIGKILL) };
            panic!("ssh {ssh} outlived the interrupted test");
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(status.code(), Some(130), "{status:?}\n{stdout}");
    assert_eq!(stdout, "dev: cancelled\n");
}

#[test]
fn a_save_rejected_by_the_users_own_config_names_that_file() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.ssh_config().parent().unwrap()).unwrap();
    std::fs::write(
        env.ssh_config(),
        "Include ~/.ssh/portico/config\n# mine\nBogus yes\n",
    )
    .unwrap();
    env.run(["init", "-y"]).assert_success();
    env.install_program(
        "ssh",
        "#!/bin/sh\nprintf '%s/.ssh/config: line 3: Bad configuration option: bogus\\n' \"$HOME\" >&2\nexit 255\n",
    );

    env.run(["add", "x", "-H", "h"])
        .assert_exit_code_and_stderr_contains(
            1,
            "line 3 of ~/.ssh/config: Bad configuration option: bogus; nothing was saved; fix that line, then save again",
        );
}

/// Replaces `ssh`: `-G` runs `resolve`, anything else runs `connect`.
fn install_connecting_ssh(env: &TestEnv, resolve: &str, connect: &str) {
    env.install_program(
        "ssh",
        &format!("#!/bin/sh\nif [ \"$1\" = '-G' ]; then\n{resolve}\nfi\n{connect}\n"),
    );
}

const RESOLVES: &str = "printf 'hostname h\\nuser user\\nport 22\\n'; exit 0";

fn env_with_x() -> TestEnv {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "x", "-H", "h"]).assert_success();
    env
}

#[test]
fn connect_explains_a_refused_login() {
    let env = env_with_x();
    install_connecting_ssh(
        &env,
        RESOLVES,
        "printf 'user@h: Permission denied (publickey).\\n' >&2; exit 255",
    );

    env.run(["connect", "x"]).assert_exit_code_and_stderr_contains(
        255,
        "user@h: Permission denied (publickey).\n\nx: failed \u{2014} the server refused the login\n",
    );
    // The explanation names the host as the inventory spells it.
    env.run(["connect", "X"])
        .assert_exit_code_and_stderr_contains(
            255,
            "\n\nx: failed \u{2014} the server refused the login\n",
        );
}

#[test]
fn connect_adds_nothing_for_other_exit_codes_or_unknown_output() {
    let env = env_with_x();
    install_connecting_ssh(
        &env,
        RESOLVES,
        "printf 'user@h: Permission denied (publickey).\\n' >&2; exit 1",
    );
    env.run(["connect", "x"])
        .assert_exit_code_and_stderr(1, "user@h: Permission denied (publickey).\n");

    install_connecting_ssh(&env, RESOLVES, "printf 'weird thing\\n' >&2; exit 255");
    env.run(["connect", "x"])
        .assert_exit_code_and_stderr(255, "weird thing\n");
}

#[test]
fn connect_keeps_the_exit_code_when_resolving_fails() {
    let env = env_with_x();
    install_connecting_ssh(
        &env,
        "printf 'oops\\n' >&2; exit 1",
        "printf 'Permission denied (publickey).\\n' >&2; exit 255",
    );

    env.run(["connect", "x"])
        .assert_exit_code_and_stderr(255, "Permission denied (publickey).\n");
}

#[test]
fn connect_explains_a_rejected_configuration() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.ssh_config().parent().unwrap()).unwrap();
    std::fs::write(
        env.ssh_config(),
        "Include ~/.ssh/portico/config\n# mine\nBogus yes\n",
    )
    .unwrap();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "x", "-H", "h"]).assert_success();
    let rejected = "printf '%s/.ssh/config: line 3: Bad configuration option: bogus\\n' \"$HOME\" >&2; exit 255";
    install_connecting_ssh(&env, rejected, rejected);

    env.run(["connect", "x"]).assert_exit_code_and_stderr_contains(
        255,
        "\n\nx: failed \u{2014} OpenSSH rejected the configuration\n  file: ~/.ssh/config\n  line: 3 (Bad configuration option: bogus)\n",
    );
}

#[test]
fn a_lost_session_says_so() {
    let env = env_with_x();
    install_connecting_ssh(
        &env,
        RESOLVES,
        "printf 'Connection to h closed by remote host.\\n' >&2; exit 255",
    );

    env.run(["connect", "x"]).assert_exit_code_and_stderr_contains(
        255,
        "Connection to h closed by remote host.\n\nx: failed \u{2014} the connection was lost\n",
    );
}

#[test]
fn a_test_rejected_by_the_users_own_config_names_that_file() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.ssh_config().parent().unwrap()).unwrap();
    std::fs::write(
        env.ssh_config(),
        "Include ~/.ssh/portico/config\n# mine\nBogus yes\n",
    )
    .unwrap();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "x", "-H", "h"]).assert_success();
    env.install_program(
        "ssh",
        "#!/bin/sh\nprintf '%s/.ssh/config: line 3: Bad configuration option: bogus\\n' \"$HOME\" >&2\nexit 255\n",
    );

    env.run(["test", "x"]).assert_exit_code_and_stdout_contains(
        1,
        "x: failed \u{2014} OpenSSH rejected the configuration\n  file: ~/.ssh/config\n  line: 3 (Bad configuration option: bogus)\n  next: fix that line, then try again\n",
    );
}
