//! Background jobs of the administration API (integrity checks, retention): run periodically
//! and on request, one at a time per job, keeping the report of the last run.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

pub struct Job<R> {
    name: &'static str,
    state: Mutex<JobState<R>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobState<R> {
    pub running: bool,
    /// Report of the last completed run, since the server started.
    pub last: Option<R>,
}

impl<R: Clone + Send + 'static> Job<R> {
    pub fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            state: Mutex::new(JobState {
                running: false,
                last: None,
            }),
        })
    }

    pub fn state(&self) -> JobState<R> {
        self.lock().clone()
    }

    /// Run `run` in the background, unless a run is in progress; returns whether it started.
    pub fn start(self: &Arc<Self>, run: impl Future<Output = R> + Send + 'static) -> bool {
        let Some(run) = self.begin(run) else {
            return false;
        };
        tokio::spawn(run);
        true
    }

    /// Run `make()` every `interval`, the first time one interval after the start, until
    /// `shutdown`.
    pub async fn run_periodically<F, Fut>(
        self: Arc<Self>,
        interval: Duration,
        shutdown: CancellationToken,
        mut make: F,
    ) where
        F: FnMut() -> Fut,
        Fut: Future<Output = R> + Send + 'static,
    {
        let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticks.tick() => match self.begin(make()) {
                    Some(run) => tokio::select! {
                        () = run => {}
                        () = shutdown.cancelled() => return,
                    },
                    None => tracing::info!(job = self.name, "already running, skipped"),
                },
                () = shutdown.cancelled() => return,
            }
        }
    }

    /// Mark the job as running, and return the run, which records its report.
    fn begin<Fut>(
        self: &Arc<Self>,
        run: Fut,
    ) -> Option<impl Future<Output = ()> + Send + use<R, Fut>>
    where
        Fut: Future<Output = R> + Send + 'static,
    {
        {
            let mut state = self.lock();
            if state.running {
                return None;
            }
            state.running = true;
        }
        let job = self.clone();
        // Cleared even if the run is dropped (cancelled at shutdown) or panics.
        let guard = Running(self.clone());
        Some(async move {
            let report = run.await;
            job.lock().last = Some(report);
            drop(guard);
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, JobState<R>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

struct Running<R>(Arc<Job<R>>);

impl<R> Drop for Running<R> {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .running = false;
    }
}

/// `hours` as an interval; `None` for 0.
pub fn hours(hours: u64) -> Option<Duration> {
    (hours > 0).then(|| Duration::from_secs(hours.saturating_mul(3600)))
}
