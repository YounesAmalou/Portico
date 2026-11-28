mod support;

use support::{TestEnv, shown};

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

/// Unix-only: there a backslash in an expanded Include escapes the next character; on Windows
/// it is the path separator.
#[cfg(unix)]
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
    assert!(log.contains("connect\n--hold\n--\ndev"));
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

/// The desktop panel (`desktop/quickshell/PorticoModel.qml`) connects with exactly this form.
#[test]
fn the_desktop_panel_connect_form_opens_a_new_terminal() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "dev", "-H", "dev.example"])
        .assert_success();

    env.run(["connect", "--new-terminal", "--", "dev"])
        .assert_success();
    assert_eq!(
        std::fs::read_to_string(env.connection_log()).unwrap(),
        format!(
            "{}\nconnect\n--hold\n--\ndev\n",
            env!("CARGO_BIN_EXE_portico")
        )
    );
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
        &format!(
            "dev: failed \u{2014} the server refused the login\n  user:    deploy@h:22\n  offered: {} (file)\n  next:    check User and IdentityFile for dev, and that the public key is in the\n           server's authorized_keys\n",
            support::shown("/tmp/portico-fixture/wrong")
        ),
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

#[cfg(unix)]
fn alive(pid: libc::pid_t) -> bool {
    u32::try_from(pid)
        .ok()
        .and_then(|pid| portico::platform::process::Proc::system().stat(pid))
        .is_some_and(|stat| stat.state != 'Z')
}

/// Unix-only: Windows sends no signals.
#[cfg(unix)]
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
        // Like ssh, name the file given with -F: Portico's staged copy of the entry config.
        "#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = '-F' ]; then config=\"$2\"; fi\n  shift\ndone\nprintf '%s: line 3: Bad configuration option: bogus\\n' \"$config\" >&2\nexit 255\n",
    );

    env.run(["add", "x", "-H", "h"])
        .assert_exit_code_and_stderr_contains(
            1,
            &format!(
                "line 3 of {}: Bad configuration option: bogus; nothing was saved; fix that line, then save again",
                shown("~/.ssh/config")
            ),
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

/// Unix-only: Windows sends no signals.
#[cfg(unix)]
#[test]
fn connect_exits_as_a_shell_would_when_a_signal_kills_ssh() {
    let env = env_with_x();
    install_connecting_ssh(&env, RESOLVES, "kill -TERM $$");
    env.run(["connect", "x"]).assert_exit_code(128 + 15);
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
        &format!(
            "\n\nx: failed \u{2014} OpenSSH rejected the configuration\n  file: {}\n  line: 3 (Bad configuration option: bogus)\n",
            shown("~/.ssh/config")
        ),
    );
}

#[test]
fn hold_waits_for_enter_only_after_an_explained_failure() {
    let env = env_with_x();
    install_connecting_ssh(
        &env,
        RESOLVES,
        "printf 'user@h: Permission denied (publickey).\\n' >&2; exit 255",
    );
    let mut child = env.spawn_piped(["connect", "x", "--hold"]);
    let mut stdin = child.stdin.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let reader = {
        let seen = seen.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 256];
            while let Ok(read) = std::io::Read::read(&mut stderr, &mut chunk) {
                if read == 0 {
                    break;
                }
                seen.lock().unwrap().extend_from_slice(&chunk[..read]);
            }
        })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !String::from_utf8_lossy(&seen.lock().unwrap()).ends_with("Press Enter to close\n") {
        assert!(std::time::Instant::now() < deadline, "no prompt shown");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Still waiting: stdin is open and nothing has been sent.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(child.try_wait().unwrap().is_none(), "exited without Enter");
    std::io::Write::write_all(&mut stdin, b"\n").unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(255));
    reader.join().unwrap();
    // End of input also ends the wait.
    env.run_with_stdin(["connect", "x", "--hold"], "")
        .assert_exit_code_and_stderr_contains(255, "Press Enter to close");

    install_connecting_ssh(&env, RESOLVES, "exit 0");
    env.run_with_stdin(["connect", "x", "--hold"], "")
        .assert_exit_code_and_stderr(0, "");

    install_connecting_ssh(&env, RESOLVES, "printf 'weird thing\\n' >&2; exit 255");
    env.run_with_stdin(["connect", "x", "--hold"], "\n")
        .assert_exit_code_and_stderr(255, "weird thing\n");
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
        &format!(
            "x: failed \u{2014} OpenSSH rejected the configuration\n  file: {}\n  line: 3 (Bad configuration option: bogus)\n  next: fix that line, then try again\n",
            shown("~/.ssh/config")
        ),
    );
}

const INCLUDE: &str = "Include ~/.ssh/portico/config\n";

/// An initialized home whose entry config declares `hosts` below the Portico include.
fn env_with_external(hosts: &str) -> (TestEnv, String) {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    let entry = format!("{INCLUDE}{hosts}");
    std::fs::write(env.ssh_config(), &entry).unwrap();
    (env, entry)
}

#[test]
fn adopt_requires_yes_without_a_terminal() {
    let (env, entry) = env_with_external("Host dev\n  HostName dev.example\n");
    let managed = std::fs::read(env.managed_config()).unwrap();
    env.run(["adopt", "dev"])
        .assert_failure_stderr_contains("adopt requires --yes in non-interactive mode");
    env.run(["adopt"]).assert_failure_stderr_contains(
        "adopt requires host aliases or --all in non-interactive mode",
    );
    assert_eq!(std::fs::read_to_string(env.ssh_config()).unwrap(), entry);
    assert_eq!(std::fs::read(env.managed_config()).unwrap(), managed);
}

#[test]
fn adopt_named_hosts_moves_only_those() {
    let (env, _) =
        env_with_external("Host one\n  HostName one.example\n\nHost two\n  HostName two.example\n");
    env.run(["adopt", "two", "-y"])
        .assert_stdout_contains("Adopted: two");
    let entry = std::fs::read_to_string(env.ssh_config()).unwrap();
    assert!(entry.contains("Host one") && !entry.contains("Host two"));
    env.run(["hosts"]).assert_stdout_contains("two  managed");
    env.run(["hosts"]).assert_stdout_contains("one  external");
}

#[test]
fn adopt_all_skips_ineligible_hosts_with_reasons() {
    let (env, _) = env_with_external(
        "Host dev\n  HostName dev.example\n\nHost multi other\n  HostName m.example\n",
    );
    env.run(["adopt", "--all", "--yes"])
        .assert_stdout_contains("Skipped multi: Host line declares 2 aliases");
    env.run(["hosts"]).assert_stdout_contains("dev  managed");
    env.run(["hosts"]).assert_stdout_contains("multi  external");
}

#[test]
fn adopt_named_ineligible_host_fails_without_changes() {
    let (env, entry) = env_with_external(
        "Host dev\n  HostName dev.example\n\nHost multi other\n  HostName m.example\n",
    );
    let managed = std::fs::read(env.managed_config()).unwrap();
    env.run(["adopt", "dev", "multi", "-y"])
        .assert_failure_stderr_contains("cannot adopt multi: Host line declares 2 aliases");
    env.run(["adopt", "nope", "-y"])
        .assert_failure_stderr_contains("unknown host: nope");
    assert_eq!(std::fs::read_to_string(env.ssh_config()).unwrap(), entry);
    assert_eq!(std::fs::read(env.managed_config()).unwrap(), managed);
}

#[test]
fn adopt_all_conflicts_with_aliases() {
    let (env, _) = env_with_external("Host dev\n  HostName dev.example\n");
    env.run(["adopt", "dev", "--all", "-y"])
        .assert_failure_stderr_contains("cannot be used with");
}

#[test]
fn export_moves_hosts_back_below_the_include() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "dev", "-H", "dev.example"])
        .assert_success();
    env.run(["export", "dev"])
        .assert_failure_stderr_contains("export requires --yes in non-interactive mode");
    env.run(["export", "dev", "-y"])
        .assert_stdout_contains("Exported: dev");
    let entry = std::fs::read_to_string(env.ssh_config()).unwrap();
    assert!(entry.starts_with(INCLUDE));
    assert!(entry.contains("Host dev\n"));
    env.run(["hosts"]).assert_stdout_contains("dev  external");
}

#[test]
fn adopt_refuses_a_block_whose_effective_configuration_would_change() {
    if !std::path::Path::new("/usr/bin/ssh").exists() {
        eprintln!("skipped: /usr/bin/ssh is not installed");
        return;
    }
    let env = TestEnv::new();
    env.install_program("ssh", "#!/bin/sh\nexec /usr/bin/ssh \"$@\"\n");
    env.run(["init", "-y"]).assert_success();
    let entry = format!("{INCLUDE}Host *\n  User root\n\nHost dev\n  HostName d\n  User alice\n");
    std::fs::write(env.ssh_config(), &entry).unwrap();
    let managed = std::fs::read(env.managed_config()).unwrap();
    env.run(["adopt", "dev", "-y"])
        .assert_failure_stderr_contains("effective configuration would change");
    assert_eq!(std::fs::read_to_string(env.ssh_config()).unwrap(), entry);
    assert_eq!(std::fs::read(env.managed_config()).unwrap(), managed);
}

/// `ssh -G` for `alias` through a copy of the entry config whose Portico include names an
/// absolute path, because OpenSSH expands `~` from the passwd home rather than `$HOME`.
fn ssh_g(env: &TestEnv, alias: &str) -> String {
    let entry = std::fs::read_to_string(env.ssh_config()).unwrap();
    let staged = entry.replace(
        "Include ~/.ssh/portico/config",
        &format!("Include {}", env.managed_config().display()),
    );
    let path = env.home().join("staged-ssh-config");
    std::fs::write(&path, staged).unwrap();
    let output = std::process::Command::new("/usr/bin/ssh")
        .args(["-G", "-F"])
        .arg(&path)
        .arg(alias)
        .env("HOME", env.home())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn adopt_and_export_keep_ssh_g_identical_with_real_openssh() {
    if !std::path::Path::new("/usr/bin/ssh").exists() {
        eprintln!("skipped: /usr/bin/ssh is not installed");
        return;
    }
    let env = TestEnv::new();
    env.install_program("ssh", "#!/bin/sh\nexec /usr/bin/ssh \"$@\"\n");
    env.run(["init", "-y"]).assert_success();
    let entry = format!(
        "{INCLUDE}# the dev box\nHost dev\n  HostName dev.example\n  # deploys only\n  User alice\n  Port 2222\n"
    );
    std::fs::write(env.ssh_config(), &entry).unwrap();
    let before = ssh_g(&env, "dev");
    assert!(before.contains("hostname dev.example\n"), "{before}");

    env.run(["adopt", "dev", "-y"])
        .assert_stdout_contains("Adopted: dev");
    assert_eq!(ssh_g(&env, "dev"), before);
    let managed = std::fs::read_to_string(env.managed_config()).unwrap();
    assert!(managed.contains("# the dev box"), "{managed}");
    assert!(managed.contains("# deploys only"), "{managed}");
    assert!(
        !std::fs::read_to_string(env.ssh_config())
            .unwrap()
            .contains("Host dev")
    );

    env.run(["export", "dev", "-y"])
        .assert_stdout_contains("Exported: dev");
    assert_eq!(ssh_g(&env, "dev"), before);
    let exported = std::fs::read_to_string(env.ssh_config()).unwrap();
    assert!(exported.contains("# the dev box"), "{exported}");
    assert!(exported.contains("# deploys only"), "{exported}");
}

fn settings_file(env: &TestEnv) -> std::path::PathBuf {
    env.home().join(".config/portico/settings.toml")
}

#[test]
fn settings_lists_every_key_and_the_path() {
    let env = TestEnv::new();
    let output = env.run(["settings"]);
    let stdout = String::from_utf8_lossy(&output.0.stdout).into_owned();
    for expected in [
        "tabs.tunnels  on".to_owned(),
        "tabs.podman   on".to_owned(),
        format!(
            "{} (not created yet)",
            shown("~/.config/portico/settings.toml")
        ),
    ] {
        assert!(stdout.contains(&expected), "{stdout}");
    }
    assert!(!stdout.contains("Remembered"), "{stdout}");
    output.assert_success();
}

const REMEMBERED: &str = "[users.local.docker]\n\"shop/api\" = \"deploy\"\nweb = \"www-data\"\n\n[users.host.prod.podman]\n\"shop/api\" = \"app\"\n";

fn write_settings(env: &TestEnv, text: &str) {
    std::fs::create_dir_all(settings_file(env).parent().unwrap()).unwrap();
    std::fs::write(settings_file(env), text).unwrap();
}

#[test]
fn settings_lists_remembered_users() {
    let env = TestEnv::new();
    write_settings(&env, REMEMBERED);
    env.run(["settings"]).assert_stdout_contains(
        "Remembered container users:\n  this machine  docker  shop/api  deploy\n  this machine  docker  web       www-data\n  prod          podman  shop/api  app\n",
    );
}

#[test]
fn settings_forget_removes_an_entry() {
    let env = TestEnv::new();
    write_settings(&env, REMEMBERED);
    let forget = [
        "settings",
        "forget",
        "--on",
        "prod",
        "--backend",
        "podman",
        "shop/api",
    ];
    env.run(forget)
        .assert_stdout_contains("forgot the user of shop/api (podman on prod)");
    assert!(
        !std::fs::read_to_string(settings_file(&env))
            .unwrap()
            .contains("app")
    );
    env.run(forget)
        .assert_stdout_contains("nothing remembered for shop/api (podman on prod)");
    env.run(["settings", "forget", "web"])
        .assert_stdout_contains("forgot the user of web (docker on this machine)");
}

#[test]
fn settings_forget_refuses_proxmox() {
    let env = TestEnv::new();
    env.run(["settings", "forget", "--backend", "proxmox", "101"])
        .assert_exit_code_and_stderr_contains(
            2,
            "a Proxmox container's shell runs as root, so no user is remembered",
        );
}

#[test]
fn settings_set_writes_and_reports() {
    let env = TestEnv::new();
    env.run(["settings", "set", "tabs.podman", "off"])
        .assert_stdout_contains("tabs.podman: off");
    assert!(
        std::fs::read_to_string(settings_file(&env))
            .unwrap()
            .contains("podman = false")
    );
    env.run(["settings"])
        .assert_stdout_contains("tabs.podman   off  (default on)");
    env.run(["settings", "set", "tabs.podman", "off"])
        .assert_stdout_contains("tabs.podman: already off");
}

#[test]
fn settings_set_rejects_unknown_key_and_value() {
    let env = TestEnv::new();
    env.run(["settings", "set", "tabs.kubernetes", "off"])
        .assert_exit_code_and_stderr_contains(
            2,
            "tabs.tunnels, tabs.docker, tabs.podman, tabs.proxmox",
        );
    env.run(["settings", "set", "tabs.docker", "maybe"])
        .assert_exit_code_and_stderr_contains(2, "on or off");
    env.run(["settings", "--json", "set", "tabs.docker", "off"])
        .assert_exit_code(2);
}

#[test]
fn settings_reports_an_invalid_file() {
    let env = TestEnv::new();
    std::fs::create_dir_all(settings_file(&env).parent().unwrap()).unwrap();
    std::fs::write(settings_file(&env), "[tabs\n").unwrap();
    env.run(["settings"])
        .assert_exit_code_and_stderr_contains(1, "line 1");
}

/// A port nobody uses: bound, read, released.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// An initialised home with the host `prod`, whose ssh starts tunnels as
/// `support::install_tunnel_ssh` does and answers the listening-ports command with 80 and 443.
fn forward_env() -> TestEnv {
    let env = TestEnv::new();
    support::install_tunnel_ssh(&env);
    std::fs::write(
        env.home().parent().unwrap().join("remote-calls"),
        "printf 'LISTEN 0 511 0.0.0.0:80 0.0.0.0:*\\nLISTEN 0 511 [::]:443 [::]:*\\nLISTEN 0 128 0.0.0.0:22 0.0.0.0:*\\n'\n",
    )
    .unwrap();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "prod", "-H", "prod.example"])
        .assert_success();
    env
}

/// Stops every tunnel when dropped, so no stand-in outlives a failed test.
struct StopAll<'a>(&'a TestEnv);

impl Drop for StopAll<'_> {
    fn drop(&mut self) {
        let _ = self.0.run(["tunnel", "stop", "--all"]);
    }
}

fn stdout_of(result: support::RunResult) -> String {
    let output = result.0;
    assert!(
        output.status.success(),
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn forward_requires_a_port_when_not_interactive() {
    let env = forward_env();
    env.run(["forward", "--on", "prod"])
        .assert_exit_code_and_stderr(2, "error: --port is required; suggested: 80, 443\n");
    assert!(env.run(["tunnels"]).0.stdout.starts_with(b"No tunnels."));
}

#[test]
fn forward_refuses_a_local_port_of_zero() {
    let env = forward_env();
    let output = env.run(["forward", "--on", "prod", "--port", "5432", "--local", "0"]);
    output.assert_exit_code_and_stderr(
        1,
        "error: local port 0 is not a port; give one from 1 through 65535\n",
    );
    assert!(env.run(["tunnels"]).0.stdout.starts_with(b"No tunnels."));
}

#[test]
fn forward_prints_the_address_and_how_to_keep_it() {
    let env = forward_env();
    let _stop = StopAll(&env);
    let local = free_port();
    let output = stdout_of(env.run([
        "forward",
        "--on",
        "prod",
        "--port",
        "5432",
        "--local",
        &local.to_string(),
    ]));
    assert_eq!(
        output,
        format!(
            "Forwarding 127.0.0.1:{local} \u{2192} prod:5432 (temporary forward _prod-5432).\n\
             Stop it with `portico tunnel stop _prod-5432`, keep it with `portico tunnel save _prod-5432`.\n"
        )
    );
    let list = stdout_of(env.run(["tunnels"]));
    assert!(list.contains("_prod-5432 (temporary)  prod  "), "{list}");
    assert!(list.contains("running  pid "), "{list}");
    env.run(["tunnel", "stop", "_prod-5432"])
        .assert_stdout_contains("Stopped _prod-5432.");
    assert!(stdout_of(env.run(["tunnels"])).starts_with("No tunnels."));
}

#[test]
fn forward_to_a_local_container_prints_where_it_is_reachable() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.install_program(
        "docker",
        r#"#!/bin/sh
if [ "$1" = inspect ]; then
  echo '{"bridge":{"IPAddress":"172.18.0.4"}}|{"80/tcp":{}}|default'
  exit 0
fi
cat <<'EOF'
{"id":"aaaa1111","name":"web","image":"nginx","command":"","running_for":"","status":"Up 2 hours","ports":"0.0.0.0:8080->80/tcp","networks":"bridge","mounts":"","project":"","service":"","number":"","working_dir":""}
EOF
"#,
    );
    let output = stdout_of(env.run(["forward", "web"]));
    assert_eq!(output, "Port 80 of web is reachable at 127.0.0.1:8080.\n");
    // Nothing was started.
    assert!(stdout_of(env.run(["tunnels"])).starts_with("No tunnels."));
}

#[test]
fn tunnel_save_keeps_a_temporary_forward() {
    let env = forward_env();
    let _stop = StopAll(&env);
    let local = free_port().to_string();
    stdout_of(env.run([
        "forward", "--on", "prod", "--port", "5432", "--local", &local,
    ]));
    let output = stdout_of(env.run(["tunnel", "save", "_prod-5432", "--name", "pg"]));
    assert_eq!(
        output,
        "Saved pg.\nRestarted it as pg; connections through it were interrupted.\n"
    );
    let list = stdout_of(env.run(["tunnels"]));
    assert!(list.contains("pg  prod  "), "{list}");
    assert!(!list.contains("_prod-5432"), "{list}");
    assert!(list.contains("running  pid "), "{list}");
    let file = std::fs::read_to_string(env.home().join(".ssh/portico/tunnels")).unwrap();
    assert!(file.contains("pg"), "{file}");
    // Only a temporary forward is saved.
    env.run(["tunnel", "save", "pg"])
        .assert_failure_stderr_contains("pg is not a temporary forward");
}

/// `docker ps` of `prod`: the Compose service `shop/api`, on `shop_default`.
const SHOP_API: &str = r#"{"id":"bbbb2222","name":"shop-api-1","image":"api","command":"","running_for":"","status":"Up 2 hours","ports":"","networks":"shop_default","mounts":"","project":"shop","service":"api","number":"1","working_dir":""}"#;

/// `forward_env` whose `prod` lists `listing` with Docker, at 172.18.0.4 for `docker inspect`,
/// and has neither Podman nor Proxmox; with `refused`, ssh's login is refused instead.
fn container_env(listing: &str, refused: bool) -> TestEnv {
    let env = forward_env();
    let refusal = if refused {
        "printf 'user@h: Permission denied (publickey).\\n' >&2\nexit 255\n"
    } else {
        ""
    };
    std::fs::write(
        env.home().parent().unwrap().join("remote-calls"),
        format!(
            "{refusal}for a; do last=$a; done\ncase $last in\n  *'docker inspect'*) echo '{{\"shop_default\":{{\"IPAddress\":\"172.18.0.4\"}}}}|{{\"80/tcp\":{{}}}}|default'; exit 0 ;;\n  *'docker ps'*)\ncat <<'EOF'\n{listing}\nEOF\n  exit 0 ;;\n  *'podman ps'*) echo 'sh: 2: exec: podman: not found' >&2; exit 127 ;;\n  *'pct list'*) echo 'portico: no pct' >&2; exit 127 ;;\nesac\n"
        ),
    )
    .unwrap();
    env
}

fn tunnels_file(env: &TestEnv) -> String {
    std::fs::read_to_string(env.home().join(".ssh/portico/tunnels")).unwrap_or_default()
}

#[test]
fn tunnel_add_container_saves_the_backend_that_lists_it() {
    let env = container_env(SHOP_API, false);
    let local = free_port();
    let output = stdout_of(env.run([
        "tunnel",
        "add",
        "api",
        "--on",
        "prod",
        "--container",
        "shop/api",
        "--port",
        "80",
        "--local",
        &local.to_string(),
    ]));
    assert_eq!(
        output,
        format!("Added api (127.0.0.1:{local} \u{2192} shop/api:80 through prod).\n")
    );
    let file = tunnels_file(&env);
    assert!(
        file.contains(&format!(
            "ContainerForward 127.0.0.1:{local} docker shop/api 80"
        )),
        "{file}"
    );
    let show = stdout_of(env.run(["tunnel", "show", "api"]));
    assert!(
        show.contains(&format!(
            "Forward: local 127.0.0.1:{local} \u{2192} shop/api:80 (docker)"
        )),
        "{show}"
    );
    // The address is looked up at each start, so the command shows the reference in its place.
    assert!(
        show.contains(&format!("-L 127.0.0.1:{local}:<shop/api>:80 ")),
        "{show}"
    );
    let list = stdout_of(env.run(["tunnels"]));
    assert!(list.contains("shop/api on prod"), "{list}");
    assert!(list.contains("shop/api:80 (docker)"), "{list}");
}

#[test]
fn tunnel_add_container_needs_its_port() {
    let env = container_env(SHOP_API, false);
    env.run([
        "tunnel",
        "add",
        "api",
        "--on",
        "prod",
        "--container",
        "shop/api",
    ])
    .assert_exit_code_and_stderr_contains(2, "--port");
    assert_eq!(tunnels_file(&env).matches("Tunnel ").count(), 0);
}

#[test]
fn tunnel_add_of_a_container_not_running_needs_its_backend() {
    let env = container_env("", false);
    let local = free_port().to_string();
    let add = |backend: Option<&str>| {
        let mut args = vec![
            "tunnel",
            "add",
            "api",
            "--on",
            "prod",
            "--container",
            "shop/api",
            "--port",
            "80",
            "--local",
            &local,
        ];
        if let Some(backend) = backend {
            args.extend(["--backend", backend]);
        }
        env.run(args)
    };
    add(None).assert_exit_code_and_stderr(
        1,
        "error: shop/api is not running on prod; give --backend to save it anyway\n",
    );
    assert!(!tunnels_file(&env).contains("Tunnel api"));
    stdout_of(add(Some("podman")));
    assert!(
        tunnels_file(&env).contains(&format!(
            "ContainerForward 127.0.0.1:{local} podman shop/api 80"
        )),
        "{}",
        tunnels_file(&env)
    );
}

#[test]
fn tunnel_add_container_explains_a_refused_login() {
    let env = container_env(SHOP_API, true);
    env.run([
        "tunnel",
        "add",
        "api",
        "--on",
        "prod",
        "--container",
        "shop/api",
        "--port",
        "80",
    ])
    .assert_exit_code_and_stderr_contains(
        1,
        "user@h: Permission denied (publickey).\n\nprod: failed \u{2014} the server refused the login\n",
    );
}

/// `container_env` with the container tunnel `api` to `shop/api`'s port 80 through `prod`;
/// returns its local port.
fn with_container_tunnel(env: &TestEnv) -> u16 {
    let local = free_port();
    stdout_of(env.run([
        "tunnel",
        "add",
        "api",
        "--on",
        "prod",
        "--container",
        "shop/api",
        "--backend",
        "docker",
        "--port",
        "80",
        "--local",
        &local.to_string(),
    ]));
    local
}

#[test]
fn tunnel_edit_refuses_a_forward_for_a_container_tunnel() {
    let env = container_env(SHOP_API, false);
    with_container_tunnel(&env);
    let before = tunnels_file(&env);
    env.run(["tunnel", "edit", "api", "-L", "127.0.0.1:9000:db:5432"])
        .assert_exit_code_and_stderr_contains(1, "tunnel api forwards to a container");
    assert_eq!(tunnels_file(&env), before);
    // `-L` and `--container` exclude each other.
    env.run([
        "tunnel",
        "edit",
        "api",
        "-L",
        "127.0.0.1:9000:db:5432",
        "--container",
        "web",
    ])
    .assert_exit_code(2);
    assert_eq!(tunnels_file(&env), before);
}

#[test]
fn tunnel_edit_refuses_a_backend_or_port_without_a_container() {
    let env = container_env(SHOP_API, false);
    stdout_of(env.run([
        "tunnel",
        "add",
        "db",
        "--on",
        "prod",
        "-L",
        "127.0.0.1:1:localhost:5432",
    ]));
    let before = tunnels_file(&env);
    for flag in [["--backend", "docker"], ["--port", "80"]] {
        env.run(["tunnel", "edit", "db", flag[0], flag[1]])
            .assert_exit_code_and_stderr_contains(1, "tunnel db reaches no container");
    }
    env.run(["tunnel", "edit", "db", "--address", "db"])
        .assert_exit_code_and_stderr_contains(1, "tunnel db reaches no container");
    assert_eq!(tunnels_file(&env), before);
}

#[test]
fn tunnel_edit_changes_a_container_tunnel_and_turns_it_into_an_address_one() {
    let env = container_env(SHOP_API, false);
    let local = with_container_tunnel(&env);
    stdout_of(env.run([
        "tunnel",
        "edit",
        "api",
        "--port",
        "81",
        "--backend",
        "podman",
    ]));
    assert!(
        tunnels_file(&env).contains(&format!(
            "ContainerForward 127.0.0.1:{local} podman shop/api 81"
        )),
        "{}",
        tunnels_file(&env)
    );
    // A rename keeps the container.
    stdout_of(env.run(["tunnel", "edit", "api", "--name", "app"]));
    assert!(
        tunnels_file(&env).contains(&format!(
            "ContainerForward 127.0.0.1:{local} podman shop/api 81"
        )),
        "{}",
        tunnels_file(&env)
    );
    stdout_of(env.run(["tunnel", "edit", "app", "--address", "db"]));
    let file = tunnels_file(&env);
    assert!(
        file.contains(&format!("LocalForward 127.0.0.1:{local} db:81")),
        "{file}"
    );
    assert!(!file.contains("ContainerForward"), "{file}");
    // Back to a container, found by its listing, keeping the port.
    stdout_of(env.run(["tunnel", "edit", "app", "--container", "shop/api"]));
    assert!(
        tunnels_file(&env).contains(&format!(
            "ContainerForward 127.0.0.1:{local} docker shop/api 81"
        )),
        "{}",
        tunnels_file(&env)
    );
}

#[test]
fn tunnel_start_prints_that_nothing_answers_on_its_own_line() {
    let env = container_env(SHOP_API, false);
    let _stop = StopAll(&env);
    let local = with_container_tunnel(&env);
    std::fs::write(
        env.home().parent().unwrap().join("start-log"),
        "channel 2: open failed: connect failed: Connection refused\n",
    )
    .unwrap();
    let output = stdout_of(env.run(["tunnel", "start", "api"]));
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines.len(), 2, "{output}");
    assert!(
        lines[0].starts_with(&format!(
            "Started api (127.0.0.1:{local} \u{2192} shop/api:80 through prod), pid "
        )),
        "{output}"
    );
    assert_eq!(
        lines[1],
        "Warning: started, but nothing answers on port 80 inside shop/api; an app that listens only on 127.0.0.1 inside its container cannot be reached"
    );
    let starts = std::fs::read_to_string(env.home().parent().unwrap().join("ssh-calls")).unwrap();
    assert!(
        starts.contains(&format!("127.0.0.1:{local}:172.18.0.4:80")),
        "{starts}"
    );
}

#[test]
fn tunnel_start_of_a_container_explains_a_refused_login() {
    let env = container_env(SHOP_API, false);
    with_container_tunnel(&env);
    std::fs::write(
        env.home().parent().unwrap().join("remote-calls"),
        "printf 'user@h: Permission denied (publickey).\\n' >&2\nexit 255\n",
    )
    .unwrap();
    env.run(["tunnel", "start", "api"])
        .assert_exit_code_and_stderr_contains(
            1,
            "user@h: Permission denied (publickey).\n\nprod: failed \u{2014} the server refused the login\n",
        );
}

#[test]
fn a_multi_tunnel_start_names_the_tunnel_whose_container_failure_it_explains() {
    let env = container_env(SHOP_API, false);
    with_container_tunnel(&env);
    let other = free_port();
    stdout_of(env.run([
        "tunnel",
        "add",
        "web",
        "--on",
        "prod",
        "--container",
        "shop/api",
        "--backend",
        "docker",
        "--port",
        "80",
        "--local",
        &other.to_string(),
    ]));
    std::fs::write(
        env.home().parent().unwrap().join("remote-calls"),
        "printf 'user@h: Permission denied (publickey).\\n' >&2\nexit 255\n",
    )
    .unwrap();
    env.run(["tunnel", "start", "api", "web"])
        .assert_exit_code_and_stderr_contains(1, "api: could not start\n");
}

#[test]
fn tunnel_add_container_defaults_its_local_port() {
    let env = container_env(SHOP_API, false);
    stdout_of(env.run([
        "tunnel",
        "add",
        "api",
        "--on",
        "prod",
        "--container",
        "shop/api",
        "--port",
        "80",
    ]));
    let file = tunnels_file(&env);
    let line = file
        .lines()
        .find_map(|line| line.trim().strip_prefix("ContainerForward 127.0.0.1:"))
        .unwrap_or_else(|| panic!("{file}"));
    let (local, rest) = line.split_once(' ').unwrap();
    assert_eq!(rest, "docker shop/api 80");
    // The port inside, or when it cannot be bound the next free one above it (from 8080 where 80
    // is privileged; on Windows it is not, but something may already listen there).
    let local: u16 = local.parse().unwrap();
    assert!(local >= 80, "{local}");
}

#[test]
fn tunnel_edit_looks_a_new_container_up_and_saves_it_as_add_does() {
    let env = container_env(SHOP_API, false);
    let local = with_container_tunnel(&env);
    stdout_of(env.run(["tunnel", "edit", "api", "--backend", "podman"]));
    // Without --backend the container is listed: found by Docker, saved as its service.
    stdout_of(env.run(["tunnel", "edit", "api", "--container", "shop-api-1"]));
    let file = tunnels_file(&env);
    assert!(
        file.contains(&format!(
            "ContainerForward 127.0.0.1:{local} docker shop/api 80"
        )),
        "{file}"
    );
    // A container no backend has is refused, leaving the file as it was.
    env.run(["tunnel", "edit", "api", "--container", "gone"])
        .assert_exit_code_and_stderr(
            1,
            "error: gone is not running on prod; give --backend to save it anyway\n",
        );
    assert_eq!(tunnels_file(&env), file);
}

#[test]
fn tunnel_edit_turns_a_container_tunnel_into_an_address_and_port() {
    let env = container_env(SHOP_API, false);
    let local = with_container_tunnel(&env);
    stdout_of(env.run(["tunnel", "edit", "api", "--address", "db", "--port", "5432"]));
    let file = tunnels_file(&env);
    assert!(
        file.contains(&format!("LocalForward 127.0.0.1:{local} db:5432")),
        "{file}"
    );
    assert!(!file.contains("ContainerForward"), "{file}");
}

#[test]
fn tunnel_edit_of_a_running_tunnel_lists_no_container() {
    let env = container_env(SHOP_API, false);
    let _stop = StopAll(&env);
    with_container_tunnel(&env);
    stdout_of(env.run(["tunnel", "start", "api"]));
    let marker = env.home().parent().unwrap().join("listed");
    std::fs::write(
        env.home().parent().unwrap().join("remote-calls"),
        format!("touch '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    env.run(["tunnel", "edit", "api", "--container", "shop-api-1"])
        .assert_exit_code_and_stderr(1, "error: tunnel api is running; stop it first\n");
    assert!(!marker.exists());
}
