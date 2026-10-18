//! Runs connection tests on a worker thread, so the interface keeps handling keys meanwhile.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

use crate::app::{App, HostSummary, TestOutcome, TestTarget};

/// Where a test was started, which decides where its result is shown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Origin {
    /// A saved host in the list.
    Host(HostSummary),
    /// The values typed into the open add or edit form.
    Form,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Job {
    Test(TestTarget),
}

#[derive(Debug)]
pub(super) struct Finished {
    pub origin: Origin,
    pub target: TestTarget,
    pub outcome: Result<TestOutcome, String>,
}

struct Request {
    generation: u64,
    origin: Origin,
    job: Job,
    cancelled: Arc<AtomicBool>,
}

struct Delivered {
    generation: u64,
    finished: Finished,
}

struct Running {
    origin: Origin,
    alias: String,
    cancelled: Arc<AtomicBool>,
}

/// Runs one test at a time on a worker thread, with the same scheme as the `ssh -G` resolver.
///
/// Every job carries a generation and a cancel flag. Starting a job cancels the running one (its
/// `ssh` is killed) and bumps the generation, so a result already on its way back is discarded
/// rather than mistaken for the current job's. A cancelled job sends nothing.
pub(super) struct Tester {
    requests: mpsc::Sender<Request>,
    results: mpsc::Receiver<Delivered>,
    generation: u64,
    running: Option<Running>,
}

impl Tester {
    pub(super) fn spawn<'scope>(
        scope: &'scope thread::Scope<'scope, '_>,
        app: &'scope App,
    ) -> Self {
        let (requests, receiver) = mpsc::channel();
        let (sender, results) = mpsc::channel();
        scope.spawn(move || test_worker(app, &receiver, &sender));
        Self {
            requests,
            results,
            generation: 0,
            running: None,
        }
    }

    /// Starts `job`, cancelling any test still running.
    pub(super) fn start(&mut self, origin: Origin, job: Job) {
        self.cancel();
        self.generation += 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        let alias = match &job {
            Job::Test(target) => target.alias().to_owned(),
        };
        let request = Request {
            generation: self.generation,
            origin: origin.clone(),
            job,
            cancelled: Arc::clone(&cancelled),
        };
        if self.requests.send(request).is_ok() {
            self.running = Some(Running {
                origin,
                alias,
                cancelled,
            });
        }
    }

    /// Abandons the running test, killing its `ssh`.
    pub(super) fn cancel(&mut self) {
        if let Some(running) = self.running.take() {
            running.cancelled.store(true, Ordering::Relaxed);
        }
    }

    /// Cancels the running test when `stale` says its result would no longer apply.
    pub(super) fn cancel_if(&mut self, stale: impl FnOnce(&Origin, &str) -> bool) {
        if self
            .running
            .as_ref()
            .is_some_and(|running| stale(&running.origin, &running.alias))
        {
            self.cancel();
        }
    }

    /// The alias under test, while a test runs.
    pub(super) fn running(&self) -> Option<&str> {
        self.running.as_ref().map(|running| running.alias.as_str())
    }

    /// Returns the current test's result, if it has arrived.
    pub(super) fn poll(&mut self) -> Option<Finished> {
        let mut current = None;
        while let Ok(delivered) = self.results.try_recv() {
            if self.running.is_some() && delivered.generation == self.generation {
                self.running = None;
                current = Some(delivered.finished);
            }
        }
        current
    }
}

impl Drop for Tester {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn test_worker(app: &App, requests: &mpsc::Receiver<Request>, results: &mpsc::Sender<Delivered>) {
    while let Ok(request) = requests.recv() {
        if request.cancelled.load(Ordering::Relaxed) {
            continue;
        }
        let Job::Test(target) = request.job;
        let outcome = app
            .test_cancellable(&target, &request.cancelled)
            .map_err(|error| error.to_string());
        if request.cancelled.load(Ordering::Relaxed) {
            continue;
        }
        let delivered = Delivered {
            generation: request.generation,
            finished: Finished {
                origin: request.origin,
                target,
                outcome,
            },
        };
        if results.send(delivered).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };

    use crate::{
        managed::{ConfigPaths, ManagedStore},
        openssh::OpenSsh,
        probe::ProbeResult,
        test_executable::write_executable,
    };

    use super::*;

    /// An app with hosts `a` and `b`. Its fake `ssh` resolves any alias; a probe of `a` starts
    /// a long `sleep` (recording its pid in `pid`) and waits for it, any other probe succeeds.
    fn slow_app(dir: &Path) -> (App, PathBuf) {
        let paths = ConfigPaths::from_home(dir);
        let store = ManagedStore::new(paths.clone());
        store.apply_init(true, |_, _| Ok(())).unwrap();
        fs::write(&paths.entry, "Include ~/.ssh/portico/config\nHost a b\n").unwrap();
        let pid = dir.join("pid");
        let authenticated =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ssh/authenticated.txt");
        let ssh = dir.join("ssh");
        write_executable(
            &ssh,
            &format!(
                "#!/bin/sh\nfor alias do :; done\ncase \"$1\" in\n  -G) printf 'hostname %s.example\\nuser deploy\\nport 22\\n' \"$alias\" ;;\n  *)\n    if [ \"$alias\" = a ]; then\n      sleep 30 &\n      printf '%s' \"$!\" > '{pid}.tmp'\n      mv '{pid}.tmp' '{pid}'\n      wait\n    fi\n    cat '{fixture}' >&2 ;;\nesac\n",
                pid = pid.display(),
                fixture = authenticated.display(),
            ),
        );
        let app = App::new(store, OpenSsh::new(ssh, "/usr/bin/false".into()));
        (app, pid)
    }

    fn wait_for_pid(path: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(pid) = fs::read_to_string(path) {
                return pid;
            }
            assert!(Instant::now() < deadline, "ssh never started");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn assert_dies(pid: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit(')')
                        .next()
                        .map(|rest| rest.trim_start().to_owned())
                });
            if state.is_none_or(|state| state.starts_with('Z')) {
                return;
            }
            assert!(Instant::now() < deadline, "process {pid} survived");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_result(tester: &mut Tester) -> Finished {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(finished) = tester.poll() {
                return finished;
            }
            assert!(Instant::now() < deadline, "no result arrived");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn saved(alias: &str) -> Job {
        Job::Test(TestTarget::Saved(alias.into()))
    }

    #[test]
    fn a_second_job_supersedes_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let (app, pid) = slow_app(dir.path());
        thread::scope(|scope| {
            let mut tester = Tester::spawn(scope, &app);
            tester.start(Origin::Form, saved("a"));
            assert_eq!(tester.running(), Some("a"));
            let first = wait_for_pid(&pid);

            tester.start(Origin::Form, saved("b"));
            assert_eq!(tester.running(), Some("b"));
            let finished = wait_for_result(&mut tester);

            assert_eq!(finished.target, TestTarget::Saved("b".into()));
            let Ok(TestOutcome::Done(report)) = finished.outcome else {
                panic!("{:?}", finished.outcome);
            };
            assert!(matches!(report.result, ProbeResult::Authenticated(_)));
            assert_eq!(tester.running(), None);
            assert_dies(&first);
            assert!(tester.poll().is_none(), "the superseded test reported");
        });
    }

    #[test]
    fn cancelling_drops_the_result_and_stops_the_ssh() {
        let dir = tempfile::tempdir().unwrap();
        let (app, pid) = slow_app(dir.path());
        thread::scope(|scope| {
            let mut tester = Tester::spawn(scope, &app);
            tester.start(Origin::Form, saved("a"));
            let sleeping = wait_for_pid(&pid);

            tester.cancel();

            assert_eq!(tester.running(), None);
            assert_dies(&sleeping);
            assert!(tester.poll().is_none());
        });
    }

    #[test]
    fn cancel_if_only_cancels_a_matching_test() {
        let dir = tempfile::tempdir().unwrap();
        let (app, pid) = slow_app(dir.path());
        thread::scope(|scope| {
            let mut tester = Tester::spawn(scope, &app);
            tester.start(Origin::Form, saved("a"));
            wait_for_pid(&pid);

            tester.cancel_if(|origin, _| matches!(origin, Origin::Host(_)));
            assert_eq!(tester.running(), Some("a"));
            tester.cancel_if(|_, alias| alias == "a");
            assert_eq!(tester.running(), None);
        });
    }

    #[test]
    fn dropping_the_tester_kills_the_running_ssh() {
        let dir = tempfile::tempdir().unwrap();
        let (app, pid) = slow_app(dir.path());
        let started = Instant::now();
        let mut sleeping = String::new();
        thread::scope(|scope| {
            let mut tester = Tester::spawn(scope, &app);
            tester.start(Origin::Form, saved("a"));
            sleeping = wait_for_pid(&pid);
            drop(tester);
        });
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_dies(&sleeping);
    }
}
