use portico_tray::layout::*;
use portico_tray::model::Action;
use portico_tray::portico::{Output, Request};
use std::io;

const HOSTS: &str = include_str!("../../../tests/fixtures/json/hosts.json");
const TUNNELS: &str = include_str!("../../../tests/fixtures/json/tunnels.json");
const CONTAINERS: &str = include_str!("../../../tests/fixtures/json/containers.json");
const DOCKER_DOWN: &str = include_str!("../../../tests/fixtures/json/containers-docker-down.json");
const TUNNEL_START: &str = include_str!("../../../tests/fixtures/json/tunnel-start.json");

fn out(stdout: &str, exit: i32) -> io::Result<Output> {
    Ok(Output {
        stdout: stdout.to_string(),
        stderr: String::new(),
        exit: Some(exit),
    })
}

fn not_run() -> io::Result<Output> {
    Err(io::Error::from(io::ErrorKind::NotFound))
}

/// A state after start, with every answer of the four first requests.
fn loaded(version: &str) -> State {
    let mut state = State::new(version, Some("/usr/bin/portico".into()));
    let requests = state.refresh();
    assert_eq!(
        requests,
        [
            Request::Version,
            Request::Settings,
            Request::Hosts,
            Request::Tunnels
        ]
    );
    state.apply(Request::Version, out("portico 0.7.0\n", 0));
    state.apply(
        Request::Settings,
        out(r#"{"schema":1,"tabs":{"tunnels":true}}"#, 0),
    );
    state.apply(Request::Hosts, out(HOSTS, 0));
    state.apply(Request::Tunnels, out(TUNNELS, 0));
    state
}

/// The menu as indented text lines: `-` separator, `[x]`/`[ ]` check items,
/// `*` clickable items, `>` submenus, plain text for disabled lines.
fn render(entries: &[Entry], depth: usize, lines: &mut Vec<String>) {
    let pad = "  ".repeat(depth);
    for entry in entries {
        match entry {
            Entry::Line(text) => lines.push(format!("{pad}{text}")),
            Entry::Separator => lines.push(format!("{pad}-")),
            Entry::Item { text, check, .. } => {
                let mark = match check {
                    Some(true) => "[x] ",
                    Some(false) => "[ ] ",
                    None => "* ",
                };
                lines.push(format!("{pad}{mark}{text}"));
            }
            Entry::Submenu(text, inner) => {
                lines.push(format!("{pad}> {text}"));
                render(inner, depth + 1, lines);
            }
        }
    }
}

fn shown(state: &State) -> Vec<String> {
    let mut lines = Vec::new();
    render(&describe(state), 0, &mut lines);
    lines
}

fn clicks(entries: &[Entry], found: &mut Vec<(String, Click)>) {
    for entry in entries {
        match entry {
            Entry::Item { text, click, .. } => found.push((text.clone(), click.clone())),
            Entry::Submenu(_, inner) => clicks(inner, found),
            _ => {}
        }
    }
}

fn click_of(state: &State, text: &str) -> Click {
    let mut found = Vec::new();
    clicks(&describe(state), &mut found);
    found
        .into_iter()
        .find(|(t, _)| t.starts_with(text))
        .unwrap_or_else(|| panic!("no item {text} in {:#?}", shown(state)))
        .1
}

#[test]
fn not_found_shows_only_that() {
    let mut state = State::new("0.7.0", None);
    assert!(state.refresh().is_empty());
    assert_eq!(
        shown(&state),
        [
            "Portico was not found",
            "Run portico desktop install again, then Refresh.",
            "-",
            "* Refresh",
            "* Quit"
        ]
    );
    assert_eq!(state.click(&Click::Connect("prod".into())), None);
}

#[test]
fn a_run_that_fails_to_start_is_not_found_at_its_path() {
    let mut state = State::new("0.7.0", Some("/opt/portico".into()));
    state.refresh();
    state.apply(Request::Hosts, not_run());
    assert_eq!(shown(&state)[0], "Portico was not found at /opt/portico");
}

#[test]
fn loaded_menu_has_every_section() {
    let lines = shown(&loaded("0.7.0"));
    assert_eq!(lines[0], "Tunnels");
    assert!(lines.contains(&"Hosts".to_string()), "{lines:#?}");
    assert!(lines.contains(&"> prod".to_string()), "{lines:#?}");
    assert!(
        lines.contains(&"> bastion (external)".to_string()),
        "{lines:#?}"
    );
    assert!(lines.contains(&"  * Open shell".to_string()), "{lines:#?}");
    assert!(
        lines.contains(&"  * Show containers".to_string()),
        "{lines:#?}"
    );
    assert!(lines.contains(&"> This machine".to_string()), "{lines:#?}");
    assert_eq!(lines[lines.len() - 2..], ["* Refresh", "* Quit"]);
    // Same versions: no banner.
    assert!(!lines.iter().any(|l| l.contains("Update")), "{lines:#?}");
}

#[test]
fn tunnels_show_state_and_checkmark() {
    let mut state = loaded("0.7.0");
    let running = r#"{"schema":1,"tunnels":[
        {"name":"db","forward":{"description":"127.0.0.1:5432 → localhost:5432"},"status":{"state":"running"},"warnings":[]},
        {"name":"web","forward":{"description":""},"status":{"state":"stopped"},"warnings":[]},
        {"name":"odd","forward":{"description":""},"status":{"state":"paused"},"warnings":[]}]}"#;
    state.menu_opened();
    state.apply(Request::Tunnels, out(running, 0));
    let lines = shown(&state);
    assert_eq!(
        lines[..4],
        [
            "Tunnels",
            "[x] db  127.0.0.1:5432 → localhost:5432  Running",
            "[ ] web  Stopped",
            // An unknown state has no action.
            "odd  paused",
        ]
    );
    assert_eq!(
        click_of(&state, "db"),
        Click::Tunnel(Action::Stop, "db".into())
    );
    assert_eq!(
        click_of(&state, "web"),
        Click::Tunnel(Action::Start, "web".into())
    );
}

#[test]
fn clicking_a_tunnel_makes_it_busy_until_its_answer() {
    let mut state = loaded("0.7.0");
    let click = click_of(&state, "preview");
    let Click::Tunnel(action, name) = click.clone() else {
        panic!("{click:?}")
    };
    assert_eq!(
        state.click(&click),
        Some(Request::Tunnel(action, name.clone()))
    );
    // A second click while it runs does nothing.
    assert_eq!(state.click(&click), None);
    let doing = if action == Action::Stop {
        "Stopping…"
    } else {
        "Starting…"
    };
    assert!(
        shown(&state)
            .iter()
            .any(|l| l.starts_with(&name) && l.ends_with(doing)),
        "{:#?}",
        shown(&state)
    );
    state.apply(Request::Tunnel(action, name.clone()), out("{}", 1));
    assert!(state.busy.is_empty());
    // `{}` has no schema: the row shows why.
    assert!(state.row_errors.contains_key(&Row::Tunnel(name)));
}

#[test]
fn a_failed_start_replaces_the_row_and_shows_the_error() {
    let mut state = loaded("0.7.0");
    state.click(&Click::Tunnel(Action::Start, "preview".into()));
    state.apply(
        Request::Tunnel(Action::Start, "preview".into()),
        out(TUNNEL_START, 1),
    );
    let lines = shown(&state);
    let at = lines
        .iter()
        .position(|l| l.starts_with("[ ] preview") && l.ends_with("Failed"))
        .unwrap_or_else(|| panic!("{lines:#?}"));
    assert!(
        lines[at + 1..]
            .iter()
            .any(|l| l.contains("the server refused the remote forward")),
        "{lines:#?}"
    );
}

#[test]
fn a_listing_sent_before_an_action_finished_is_dropped() {
    let mut state = loaded("0.7.0");
    let before = shown(&state);
    let opened = state.menu_opened();
    assert_eq!(opened, [Request::Tunnels]);
    state.click(&Click::Tunnel(Action::Start, "preview".into()));
    state.apply(
        Request::Tunnel(Action::Start, "preview".into()),
        out(TUNNEL_START, 1),
    );
    let after_action = shown(&state);
    assert_ne!(before, after_action);
    // The listing sent before the action carries the old state.
    state.apply(Request::Tunnels, out(TUNNELS, 0));
    assert_eq!(shown(&state), after_action);
}

#[test]
fn version_banner_and_development_build() {
    let lines = shown(&loaded("dev"));
    assert!(lines.contains(&"Development build".to_string()));
    assert!(!lines.iter().any(|l| l.contains("Update")));

    let state = loaded("0.6.0");
    assert_eq!(
        click_of(&state, "Portico is").clone(),
        Click::Update,
        "{:#?}",
        shown(&state)
    );
    assert!(shown(&state).contains(&"* Portico is 0.7.0, this app is 0.6.0: Update".to_string()));
}

#[test]
fn update_failure_is_shown() {
    let mut state = loaded("0.7.0");
    state.update_failure = Some("the download did not match its digest".into());
    assert!(
        shown(&state)
            .iter()
            .any(|l| l.starts_with("Update failed: the download")),
        "{:#?}",
        shown(&state)
    );
}

#[test]
fn settings_turn_sections_off() {
    let mut state = loaded("0.7.0");
    state.apply(
        Request::Settings,
        out(
            r#"{"schema":1,"tabs":{"tunnels":false,"docker":false,"podman":false,"proxmox":false}}"#,
            0,
        ),
    );
    let lines = shown(&state);
    assert_eq!(lines[0], "Hosts");
    assert!(
        !lines.iter().any(|l| l.contains("containers")),
        "{lines:#?}"
    );
    assert!(!lines.iter().any(|l| l.contains("This machine")));
    assert!(state.menu_opened().is_empty());
    assert_eq!(state.click(&Click::ShowContainers(None)), None);
}

#[test]
fn an_old_portico_without_settings_keeps_every_tab() {
    let mut state = loaded("0.7.0");
    state.apply(Request::Settings, out("error: unknown command", 2));
    assert_eq!(shown(&state)[0], "Tunnels");
}

#[test]
fn containers_list_on_request() {
    let mut state = loaded("0.7.0");
    let target = Some("prod".to_string());
    let request = state.click(&Click::ShowContainers(target.clone())).unwrap();
    let all = ["docker", "podman", "proxmox"].map(String::from).to_vec();
    assert_eq!(request, Request::Containers(target.clone(), all));
    assert!(shown(&state).contains(&"  Listing containers…".to_string()));
    state.apply(request.clone(), out(CONTAINERS, 0));
    let lines = shown(&state);
    assert!(
        lines.contains(&"  * Refresh containers".to_string()),
        "{lines:#?}"
    );
    let shell = click_of(&state, "Docker: ");
    let Click::Shell(on, backend, _) = &shell else {
        panic!("{shell:?}")
    };
    assert_eq!((on, backend.as_str()), (&target, "docker"));

    // A failing shell shows its stderr under the container.
    let launch = state.click(&shell).unwrap();
    state.apply(
        launch,
        Ok(Output {
            stdout: String::new(),
            stderr: "error: no terminal found\n".into(),
            exit: Some(1),
        }),
    );
    assert!(
        shown(&state).contains(&"      error: no terminal found".to_string()),
        "{:#?}",
        shown(&state)
    );

    // Refresh drops listings and errors.
    state.refresh();
    assert!(shown(&state).contains(&"  * Show containers".to_string()));
    assert!(state.row_errors.is_empty());
    // An answer to a listing dropped by Refresh is ignored.
    state.apply(request, out(CONTAINERS, 0));
    assert!(state.containers.is_empty());
}

#[test]
fn a_failed_listing_shows_message_and_next_step() {
    let mut state = loaded("0.7.0");
    let request = state.click(&Click::ShowContainers(None)).unwrap();
    state.apply(request, out(DOCKER_DOWN, 1));
    let lines = shown(&state);
    let at = lines.iter().position(|l| l == "> This machine").unwrap();
    assert_eq!(lines[at + 1], "  * Refresh containers");
    assert!(lines.len() > at + 3, "{lines:#?}");
    assert!(!lines[at + 2].starts_with("  *"), "{lines:#?}");
}

#[test]
fn a_document_with_another_schema_is_shown_alone() {
    let mut state = loaded("0.7.0");
    state.apply(Request::Hosts, out(r#"{"schema":2}"#, 0));
    let lines = shown(&state);
    assert!(
        lines[0].starts_with("This Portico prints schema 2"),
        "{lines:#?}"
    );
    assert!(!lines.iter().any(|l| l == "Hosts"));
    assert_eq!(lines[lines.len() - 2..], ["* Refresh", "* Quit"]);
    assert!(lines.iter().all(|l| l.chars().count() <= 60));
    assert!(state.menu_opened().is_empty());
    state.refresh();
    assert!(state.problem.is_none());
}

#[test]
fn a_failed_connect_shows_under_open_shell() {
    let mut state = loaded("0.7.0");
    let request = state.click(&Click::Connect("prod".into())).unwrap();
    assert_eq!(request, Request::Connect("prod".into()));
    state.apply(
        request,
        Ok(Output {
            stdout: String::new(),
            stderr: String::new(),
            exit: Some(3),
        }),
    );
    let lines = shown(&state);
    let at = lines.iter().position(|l| l == "> prod").unwrap();
    assert_eq!(lines[at + 1], "  * Open shell");
    assert!(
        lines[at + 2].contains("could not open a terminal"),
        "{lines:#?}"
    );
    // Clicking again clears the error until the answer.
    state.click(&Click::Connect("prod".into()));
    assert!(state.row_errors.is_empty());
}

#[test]
fn every_title_is_short() {
    let mut state = loaded("0.7.0");
    state.problem = None;
    state.update_failure = Some("x".repeat(500));
    for line in shown(&state) {
        let text = line.trim_start_matches([' ', '*', '>', '[', ']', 'x']);
        assert!(text.chars().count() <= 60, "{line}");
    }
}
