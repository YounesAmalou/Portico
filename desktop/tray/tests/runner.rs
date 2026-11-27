#![cfg(unix)]

use portico_tray::instance::acquire;
use portico_tray::model::Action;
use portico_tray::portico::{Request, Worker, locate, run, start_update, update_failure};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

fn fake(dir: &Path, body: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("portico");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn locate_prefers_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let recorded = fake(&tmp.path().join("rec"), "");
    let on_path = fake(&tmp.path().join("bin"), "");
    let found = locate(
        Some(&recorded),
        Some(on_path.parent().unwrap().as_os_str()),
        tmp.path(),
    );
    assert_eq!(found, Some(recorded));
}

#[test]
fn locate_falls_back_when_recorded_path_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let recorded = fake(&tmp.path().join("rec"), "");
    fs::remove_file(&recorded).unwrap();
    let on_path = fake(&tmp.path().join("bin"), "");
    let found = locate(
        Some(&recorded),
        Some(on_path.parent().unwrap().as_os_str()),
        tmp.path(),
    );
    assert_eq!(found, Some(on_path));
}

#[test]
fn locate_well_known_after_path() {
    let tmp = tempfile::tempdir().unwrap();
    let cargo = fake(&tmp.path().join(".cargo/bin"), "");
    let found = locate(None, Some("".as_ref()), tmp.path());
    assert_eq!(found, Some(cargo));
    assert_eq!(locate(None, None, &tmp.path().join("nothing")), None);
}

#[test]
fn run_reports_exit_and_streams() {
    let tmp = tempfile::tempdir().unwrap();
    let program = fake(tmp.path(), "echo \"out $1\"; echo err >&2; exit 3");
    let out = run(&program, &["a".to_string()]).unwrap();
    assert_eq!(out.stdout, "out a\n");
    assert_eq!(out.stderr, "err\n");
    assert_eq!(out.exit, Some(3));
}

#[test]
fn worker_answers_requests_in_background() {
    let tmp = tempfile::tempdir().unwrap();
    let program = fake(tmp.path(), "echo \"$@\"");
    let (tx, rx) = mpsc::channel();
    let worker = Worker::spawn(program, tx);
    worker.send(Request::Hosts);
    let (request, result) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(request, Request::Hosts);
    assert_eq!(result.unwrap().stdout, "hosts --json\n");

    worker.send(Request::Tunnel(Action::None, "db".into()));
    let (_, result) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);

    worker.send(Request::Containers(None, vec![]));
    let (_, result) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);

    worker.send(Request::Tunnel(Action::Stop, "db".into()));
    let (_, result) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(result.unwrap().stdout, "tunnel stop --json -- db\n");
}

#[test]
fn update_runs_detached_and_logs() {
    let tmp = tempfile::tempdir().unwrap();
    let program = fake(tmp.path(), "echo \"$@\"; echo 'error: boom' >&2");
    let log = tmp.path().join("run/tray-update.log");
    start_update(&program, &log).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let text = fs::read_to_string(&log).unwrap_or_default();
        if text.contains("error: boom") {
            assert!(text.contains("desktop install --yes --tray-only"));
            break;
        }
        assert!(std::time::Instant::now() < deadline, "log: {text:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn update_failure_reads_last_error_line() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("u.log");
    let past = SystemTime::now() - Duration::from_secs(3600);
    let future = SystemTime::now() + Duration::from_secs(3600);

    fs::write(&log, "working\nerror: download failed\n\n").unwrap();
    assert_eq!(
        update_failure(&log, past),
        Some("error: download failed".to_string())
    );
    assert_eq!(update_failure(&log, future), None);

    fs::write(&log, "error: old\ninstalled Portico Tray\n").unwrap();
    assert_eq!(update_failure(&log, past), None);
    assert_eq!(update_failure(&tmp.path().join("missing"), past), None);
}

#[test]
fn second_lock_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let first = acquire(tmp.path()).unwrap();
    assert!(first.is_some());
    assert!(acquire(tmp.path()).unwrap().is_none());
    drop(first);
    assert!(acquire(tmp.path()).unwrap().is_some());
}

#[test]
fn same_kind_requests_run_in_send_order() {
    let tmp = tempfile::tempdir().unwrap();
    let record = tmp.path().join("order");
    let program = fake(
        tmp.path(),
        &format!("sleep 0.05; echo \"$2\" >> {}; echo ok", record.display()),
    );
    let (tx, rx) = mpsc::channel();
    let worker = Worker::spawn(program, tx);
    let actions = [
        Action::Start,
        Action::Stop,
        Action::Start,
        Action::Stop,
        Action::Start,
    ];
    for action in actions {
        worker.send(Request::Tunnel(action, "db".into()));
    }
    for _ in actions {
        rx.recv_timeout(Duration::from_secs(10)).unwrap().1.unwrap();
    }
    let order = fs::read_to_string(&record).unwrap();
    assert_eq!(order, "start\nstop\nstart\nstop\nstart\n");
}
