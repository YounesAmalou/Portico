mod support;

use support::TestEnv;

#[test]
fn commands_before_initialization_explain_how_to_continue() {
    let env = TestEnv::new();

    env.run(["hosts"])
        .assert_failure_stderr_contains("Portico is not initialized; run `portico init` first");
    env.run(["add", "dev", "-H", "dev.example"])
        .assert_failure_stderr_contains("Portico is not initialized; run `portico init` first");
    env.run(["remove", "dev"])
        .assert_failure_stderr_contains("Portico is not initialized; run `portico init` first");
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
