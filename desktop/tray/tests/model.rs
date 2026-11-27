use portico_tray::model::*;
use serde_json::{Value, json};

const HOSTS: &str = include_str!("../../../tests/fixtures/json/hosts.json");
const TUNNELS: &str = include_str!("../../../tests/fixtures/json/tunnels.json");
const CONTAINERS: &str = include_str!("../../../tests/fixtures/json/containers.json");
const PODMAN: &str = include_str!("../../../tests/fixtures/json/containers-podman.json");
const PROXMOX: &str = include_str!("../../../tests/fixtures/json/containers-proxmox.json");
const DOCKER_DOWN: &str = include_str!("../../../tests/fixtures/json/containers-docker-down.json");
const PROXMOX_DENIED: &str =
    include_str!("../../../tests/fixtures/json/containers-proxmox-denied.json");
const SETTINGS: &str = include_str!("../../../tests/fixtures/json/settings.json");
const UNKNOWN_HOST: &str = include_str!("../../../tests/fixtures/json/error-unknown-host.json");

fn doc(stdout: &str) -> Value {
    match parse(stdout, Some(0)) {
        Parsed::Doc(doc) => doc,
        Parsed::Problem(p) => panic!("not a schema 1 document: {p}"),
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

#[test]
fn parses_hosts_golden() {
    let (hosts, warnings) = hosts(&doc(HOSTS));
    let rows: Vec<_> = hosts
        .iter()
        .map(|h| (h.alias.as_str(), h.external))
        .collect();
    assert_eq!(rows, [("bastion", true), ("legacy", true), ("prod", false)]);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].starts_with("duplicate alias bastion"));
}

#[test]
fn duplicate_alias_shown_once() {
    let d = json!({"schema": 1, "hosts": [
        {"alias": "a", "ownership": "managed"},
        {"alias": "a", "ownership": "external"},
    ]});
    let (hosts, warnings) = hosts(&d);
    assert_eq!(
        hosts,
        [Host {
            alias: "a".into(),
            external: false
        }]
    );
    assert!(warnings.is_empty());
}

#[test]
fn parses_every_tunnel_state() {
    let listed = tunnels(&doc(TUNNELS));
    let by_name = |name: &str| listed.iter().find(|t| t.name == name).unwrap();

    let busy = by_name("busy");
    assert_eq!(
        busy.state,
        TunnelState::PortInUse {
            address: "127.0.0.1:5433".into(),
            by: None
        }
    );
    assert_eq!(busy.action(), Action::Start);

    let db = by_name("db");
    assert_eq!(db.state, TunnelState::Stopped);
    assert_eq!(db.action(), Action::Start);
    assert_eq!(db.label, "db  127.0.0.1:1 → localhost:5432");
    assert_eq!(db.warnings.len(), 1);

    let preview = by_name("preview");
    assert_eq!(
        preview.state,
        TunnelState::Failed("Error: remote port forwarding failed for listen port 8080".into())
    );
    assert_eq!(preview.action(), Action::Start);

    let mut running = doc(TUNNELS);
    running["tunnels"][1]["status"] = json!({"state": "running"});
    let running = &tunnels(&running)[1];
    assert_eq!(running.state, TunnelState::Running);
    assert_eq!(running.action(), Action::Stop);
}

#[test]
fn unknown_tunnel_state_has_no_action() {
    let mut d = doc(TUNNELS);
    d["tunnels"][1]["status"] = json!({"state": "teleporting"});
    let tunnel = &tunnels(&d)[1];
    assert_eq!(tunnel.state, TunnelState::Unknown("teleporting".into()));
    assert_eq!(tunnel.action(), Action::None);
}

#[test]
fn parses_containers_per_backend() {
    let docker = containers(&doc(CONTAINERS)).unwrap();
    assert_eq!(docker[0].id, "4f1c2a9b7d3e");
    assert_eq!(docker[0].backend, "docker");
    assert_eq!(docker[0].label, "Docker: web · nginx:1.27 · Up 3 hours");

    let podman = containers(&doc(PODMAN)).unwrap();
    assert_eq!(podman[1].backend, "podman");
    assert_eq!(
        podman[1].label,
        "Podman: toolbox · registry.fedoraproject.org/fedora:41 · Up 5 minutes"
    );

    // Proxmox has no image.
    let proxmox = containers(&doc(PROXMOX)).unwrap();
    assert_eq!(proxmox[0].id, "110");
    assert_eq!(proxmox[0].label, "Proxmox: mail · running");
}

#[test]
fn container_errors_have_message_and_next() {
    let (message, next) = containers(&doc(DOCKER_DOWN)).unwrap_err();
    assert_eq!(message, "The Docker daemon is not running on this machine.");
    assert_eq!(next.len(), 2);
    assert_eq!(next[0], "Start it with: sudo systemctl start docker");

    let (message, next) = containers(&doc(PROXMOX_DENIED)).unwrap_err();
    assert!(message.starts_with("pct needs root"));
    assert_eq!(next.len(), 2);
}

#[test]
fn settings_tabs_and_backends() {
    let t = tabs(&doc(SETTINGS));
    assert!(t.tunnels && t.docker && t.proxmox && !t.podman);
    assert_eq!(enabled_backends(&t), ["docker", "proxmox"]);
}

#[test]
fn missing_tabs_default_on() {
    for d in [
        json!({}),
        json!({"schema": 1, "tabs": {}}),
        doc(UNKNOWN_HOST),
    ] {
        let t = tabs(&d);
        assert!(t.tunnels && t.docker && t.podman && t.proxmox);
        assert_eq!(enabled_backends(&t), ["docker", "podman", "proxmox"]);
    }
}

#[test]
fn schema_problems() {
    let old = "This Portico does not print the JSON this app reads; it needs a Portico release with --json.";
    let problem = |stdout: &str, exit| match parse(stdout, exit) {
        Parsed::Problem(p) => p,
        Parsed::Doc(_) => panic!("expected a problem for {stdout:?}"),
    };
    assert_eq!(problem("portico 0.7.0", Some(0)), old);
    assert_eq!(problem("[1]", Some(0)), old);
    assert_eq!(problem("{}", Some(0)), old);
    assert_eq!(problem("", Some(2)), old);
    assert_eq!(
        problem(r#"{"schema":2}"#, Some(0)),
        "This Portico prints schema 2; this app reads schema 1. Update the tray app."
    );
    assert_eq!(
        problem(r#"{"schema":0}"#, Some(0)),
        "This Portico prints schema 0; this app reads schema 1. Update Portico."
    );
    assert!(matches!(parse(r#"{"schema":1}"#, Some(0)), Parsed::Doc(_)));
}

#[test]
fn error_document_is_scoped() {
    let d = doc(UNKNOWN_HOST);
    assert_eq!(
        error_of(&d),
        Some(("unknown host: nope".to_string(), vec![]))
    );
    assert_eq!(error_of(&doc(HOSTS)), None);
}

#[test]
fn argument_lists() {
    assert_eq!(
        connect_args("-dash"),
        strings(&["connect", "--new-terminal", "--", "-dash"])
    );
    assert_eq!(
        shell_args(Some("api"), "podman", "abc"),
        strings(&[
            "shell",
            "--new-terminal",
            "--on=api",
            "--backend=podman",
            "--",
            "abc"
        ])
    );
    assert_eq!(
        shell_args(None, "docker", "abc"),
        strings(&["shell", "--new-terminal", "--backend=docker", "--", "abc"])
    );
    assert_eq!(
        containers_args(Some("api"), &["docker", "proxmox"]),
        Some(strings(&[
            "containers",
            "--json",
            "--on=api",
            "--backend=docker",
            "--backend=proxmox"
        ]))
    );
    assert_eq!(
        containers_args(None, &["docker", "podman", "proxmox"]),
        Some(strings(&["containers", "--json"]))
    );
    assert_eq!(containers_args(Some("api"), &[]), None);
    assert_eq!(
        tunnel_args(Action::Stop, "db"),
        strings(&["tunnel", "stop", "--json", "--", "db"])
    );
    assert_eq!(
        tunnel_args(Action::Start, "-x"),
        strings(&["tunnel", "start", "--json", "--", "-x"])
    );
}

#[test]
fn launch_failure_text() {
    assert_eq!(
        launch_failure(Some(1), "warming up\n\x1b[31mno terminal found\x1b[0m\n\n"),
        "no terminal found"
    );
    assert_eq!(
        launch_failure(Some(3), " \n"),
        "Portico could not open a terminal (exit status 3)"
    );
}

#[test]
fn version_parsing() {
    assert_eq!(version_of("portico 0.7.0\n"), Some("0.7.0".to_string()));
    assert_eq!(version_of("something else"), None);
    assert_eq!(version_of(""), None);
}

#[test]
fn title_truncates() {
    assert_eq!(title("short"), "short");
    let exact = "a".repeat(60);
    assert_eq!(title(&exact), exact);
    let cut = title(&"b".repeat(61));
    assert_eq!(cut.chars().count(), 60);
    assert!(cut.ends_with('…'));
}
