mod support;

use support::TestEnv;

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
    assert!(log.contains("--\nssh\n--\ndev"));
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
    std::fs::write(
        env.bin().join("xdg-terminal-exec"),
        "#!/bin/sh\nprintf 'launcher-noise\\n'\nprintf 'launcher-noise\\n' >&2\nsleep 2\n",
    )
    .unwrap();
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
    std::fs::write(env.bin().join("xdg-terminal-exec"), "#!/bin/sh\nexit 23\n").unwrap();

    env.run(["connect", "dev", "-n"]).assert_exit_code(23);
}
