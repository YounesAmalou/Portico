//! Builds an adoption or export plan on a worker thread: the plan runs `ssh -G` twice per
//! eligible host, so the interface keeps handling keys meanwhile and Esc can stop it.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

use crate::{
    adopt::{Direction, MigrationPlan},
    app::{App, AppError},
};

type Planned = Result<MigrationPlan, AppError>;

struct Request {
    generation: u64,
    direction: Direction,
    cancelled: Arc<AtomicBool>,
}

/// Builds one plan at a time: starting a plan cancels the running one, whose result is dropped.
pub(super) struct Planner {
    requests: mpsc::Sender<Request>,
    results: mpsc::Receiver<(u64, Planned)>,
    generation: u64,
    /// The running plan's cancellation flag and the alias its screen preselects.
    running: Option<(Arc<AtomicBool>, String)>,
}

impl Planner {
    pub(super) fn spawn<'scope>(
        scope: &'scope thread::Scope<'scope, '_>,
        app: &'scope App,
    ) -> Self {
        let (requests, receiver) = mpsc::channel();
        let (sender, results) = mpsc::channel();
        scope.spawn(move || plan_worker(app, &receiver, &sender));
        Self {
            requests,
            results,
            generation: 0,
            running: None,
        }
    }

    /// Starts the plan of `direction`, cancelling any plan still running; `false` when the
    /// worker stopped.
    pub(super) fn start(&mut self, direction: Direction, preselect: String) -> bool {
        self.cancel();
        self.generation += 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = Request {
            generation: self.generation,
            direction,
            cancelled: Arc::clone(&cancelled),
        };
        if self.requests.send(request).is_err() {
            return false;
        }
        self.running = Some((cancelled, preselect));
        true
    }

    /// Abandons the running plan, killing its `ssh`.
    pub(super) fn cancel(&mut self) {
        if let Some((cancelled, _)) = self.running.take() {
            cancelled.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The current plan with the alias to preselect, if it has arrived.
    pub(super) fn try_finished(&mut self) -> Option<(Planned, String)> {
        let mut current = None;
        while let Ok((generation, planned)) = self.results.try_recv() {
            if generation == self.generation
                && let Some((_, preselect)) = self.running.take()
            {
                current = Some((planned, preselect));
            }
        }
        current
    }
}

impl Drop for Planner {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn plan_worker(
    app: &App,
    requests: &mpsc::Receiver<Request>,
    results: &mpsc::Sender<(u64, Planned)>,
) {
    while let Ok(request) = requests.recv() {
        if request.cancelled.load(Ordering::Relaxed) {
            continue;
        }
        let planned = app.migration_plan(request.direction, Some(&request.cancelled));
        if request.cancelled.load(Ordering::Relaxed) {
            continue;
        }
        if results.send((request.generation, planned)).is_err() {
            return;
        }
    }
}
