//! Lists containers on a worker thread, so the interface keeps handling keys meanwhile.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

use super::containers::Panel;
use crate::app::{App, ContainerError, ContainerList, Target};

type Listed = (Target, Result<ContainerList, ContainerError>);

struct Request {
    generation: u64,
    target: Target,
    cancelled: Arc<AtomicBool>,
}

/// Runs one listing at a time with the scheme of the tester: starting a listing cancels the
/// running one (its `ssh` is killed) and bumps the generation, so a stale result is dropped.
pub(super) struct Lister {
    requests: mpsc::Sender<Request>,
    results: mpsc::Receiver<(u64, Listed)>,
    generation: u64,
    running: Option<Running>,
}

struct Running {
    target: Target,
    cancelled: Arc<AtomicBool>,
}

impl Lister {
    pub(super) fn spawn<'scope>(
        scope: &'scope thread::Scope<'scope, '_>,
        app: &'scope App,
    ) -> Self {
        let (requests, receiver) = mpsc::channel();
        let (sender, results) = mpsc::channel();
        scope.spawn(move || list_worker(app, &receiver, &sender));
        Self {
            requests,
            results,
            generation: 0,
            running: None,
        }
    }

    /// Lists `target`, cancelling any listing still running, and returns the target of the
    /// listing it cancelled.
    pub(super) fn start(&mut self, target: Target) -> Option<Target> {
        let cancelled_target = self.cancel();
        self.generation += 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = Request {
            generation: self.generation,
            target: target.clone(),
            cancelled: Arc::clone(&cancelled),
        };
        if self.requests.send(request).is_ok() {
            self.running = Some(Running { target, cancelled });
        }
        cancelled_target
    }

    /// Starts listing `target` and shows it in `panel`, which forgets a listing this cancels.
    pub(super) fn list(&mut self, panel: &mut Panel, target: Target) {
        if let Some(cancelled) = self.start(target.clone()) {
            panel.abandon(cancelled);
        }
        if self.is_running() {
            panel.set_listing(target);
        } else {
            panel.fail(target, "the container listing worker stopped");
        }
    }

    /// Abandons the running listing, killing its `ssh`, and returns its target.
    pub(super) fn cancel(&mut self) -> Option<Target> {
        let running = self.running.take()?;
        running.cancelled.store(true, Ordering::Relaxed);
        Some(running.target)
    }

    pub(super) fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The current listing's result, if it has arrived.
    pub(super) fn try_finished(&mut self) -> Option<Listed> {
        let mut current = None;
        while let Ok((generation, listed)) = self.results.try_recv() {
            if self.running.is_some() && generation == self.generation {
                self.running = None;
                current = Some(listed);
            }
        }
        current
    }
}

impl Drop for Lister {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn list_worker(
    app: &App,
    requests: &mpsc::Receiver<Request>,
    results: &mpsc::Sender<(u64, Listed)>,
) {
    while let Ok(request) = requests.recv() {
        if request.cancelled.load(Ordering::Relaxed) {
            continue;
        }
        let result = app.containers(&request.target, true, Some(&request.cancelled));
        if request.cancelled.load(Ordering::Relaxed) {
            continue;
        }
        if results
            .send((request.generation, (request.target, result)))
            .is_err()
        {
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
        app::ContainerPrograms,
        managed::{ConfigPaths, ManagedStore},
        openssh::OpenSsh,
        test_executable::write_executable,
    };

    use super::*;

    /// An app whose local `docker` lists one container, or, while `slow` exists, starts a long
    /// `sleep` (recording its pid in `pid`) and waits for it.
    fn local_app(dir: &Path) -> (App, PathBuf, PathBuf) {
        let (slow, pid) = (dir.join("slow"), dir.join("pid"));
        let docker = dir.join("docker");
        write_executable(
            &docker,
            &format!(
                "#!/bin/sh\nif [ -f '{slow}' ]; then\n  rm '{slow}'\n  sleep 30 &\n  printf '%s' \"$!\" > '{pid}.tmp'\n  mv '{pid}.tmp' '{pid}'\n  wait\nfi\nprintf '%s\\n' '{{\"id\":\"abc\",\"name\":\"web\"}}'\n",
                slow = slow.display(),
                pid = pid.display(),
            ),
        );
        let store = ManagedStore::new(ConfigPaths::from_home(dir));
        let app = App::new(
            store,
            OpenSsh::new("/usr/bin/false".into(), "/usr/bin/false".into()),
        )
        .with_container_programs(ContainerPrograms {
            docker,
            ..ContainerPrograms::default()
        });
        (app, slow, pid)
    }

    fn wait_for_pid(path: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(pid) = fs::read_to_string(path) {
                return pid;
            }
            assert!(Instant::now() < deadline, "docker never started");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn assert_dies(pid: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            !stat
                .rsplit(')')
                .next()
                .unwrap_or("")
                .trim_start()
                .starts_with('Z')
        }) {
            assert!(Instant::now() < deadline, "process {pid} survived");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_result(lister: &mut Lister) -> Listed {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(listed) = lister.try_finished() {
                return listed;
            }
            assert!(Instant::now() < deadline, "no result arrived");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn restarting_supersedes_the_running_listing() {
        let dir = tempfile::tempdir().unwrap();
        let (app, slow, pid) = local_app(dir.path());
        fs::write(&slow, "").unwrap();
        thread::scope(|scope| {
            let mut lister = Lister::spawn(scope, &app);
            lister.start(Target::Local);
            let first = wait_for_pid(&pid);

            assert_eq!(lister.start(Target::Local), Some(Target::Local));
            let (target, result) = wait_for_result(&mut lister);

            assert_eq!(target, Target::Local);
            let list = result.unwrap();
            assert_eq!(list.listing.containers[0].id, "abc");
            assert!(!lister.is_running());
            assert_dies(&first);
            assert!(
                lister.try_finished().is_none(),
                "the superseded listing reported"
            );
        });
    }

    #[test]
    fn cancelling_drops_the_result_and_stops_docker() {
        let dir = tempfile::tempdir().unwrap();
        let (app, slow, pid) = local_app(dir.path());
        fs::write(&slow, "").unwrap();
        thread::scope(|scope| {
            let mut lister = Lister::spawn(scope, &app);
            lister.start(Target::Local);
            let sleeping = wait_for_pid(&pid);

            assert_eq!(lister.cancel(), Some(Target::Local));

            assert!(!lister.is_running());
            assert_eq!(lister.cancel(), None);
            assert_dies(&sleeping);
            thread::sleep(Duration::from_millis(50));
            assert!(lister.try_finished().is_none());
        });
    }

    #[test]
    fn a_stopped_worker_fails_the_listing() {
        let (requests, receiver) = mpsc::channel();
        let (_sender, results) = mpsc::channel();
        drop(receiver);
        let mut lister = Lister {
            requests,
            results,
            generation: 0,
            running: None,
        };
        let mut panel = Panel::default();
        panel.follow(Some("dev"));

        lister.list(&mut panel, Target::Host("dev".into()));

        assert!(!lister.is_running());
        let text: Vec<String> = panel
            .lines(false, 10)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(text, ["the container listing worker stopped"]);
    }
}
