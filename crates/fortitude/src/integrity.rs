//! Integrity checks of the stored files against the index: periodic, and on request through
//! the administration API. One runs at a time; the report of the last one is kept.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use fortitude_store::Store;
use fortitude_store::integrity::{IntegrityOptions, IntegrityReport};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::config::IntegrityConfig;

/// Shortest grace period for orphan files, whatever the configuration: a file is written
/// before its instance is indexed, so a younger file may belong to an ingest in progress.
pub const MIN_ORPHAN_GRACE: Duration = Duration::from_secs(5 * 60);

pub struct Checker {
    store: Arc<Store>,
    config: IntegrityConfig,
    state: Mutex<State>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct State {
    pub running: bool,
    /// Report of the last completed check, since the server started.
    pub last: Option<IntegrityReport>,
}

impl Checker {
    pub fn new(store: Arc<Store>, config: IntegrityConfig) -> Arc<Self> {
        Arc::new(Self {
            store,
            config,
            state: Mutex::default(),
        })
    }

    pub fn state(&self) -> State {
        self.lock().clone()
    }

    /// Start a check in the background, unless one is running; `remove_orphans` overrides the
    /// configuration. Returns whether it started.
    pub fn start(self: &Arc<Self>, remove_orphans: Option<bool>) -> bool {
        let Some(run) = self.begin(remove_orphans) else {
            return false;
        };
        tokio::spawn(run);
        true
    }

    /// Check every `interval_hours`, the first time one interval after the start, until
    /// `shutdown`.
    pub async fn run_periodically(self: Arc<Self>, shutdown: CancellationToken) {
        let Some(interval) = self.config.interval() else {
            return;
        };
        let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticks.tick() => match self.begin(None) {
                    Some(run) => tokio::select! {
                        () = run => {}
                        () = shutdown.cancelled() => return,
                    },
                    None => tracing::info!("integrity check already running, skipped"),
                },
                () = shutdown.cancelled() => return,
            }
        }
    }

    /// Mark a check as running, and return it.
    fn begin(
        self: &Arc<Self>,
        remove_orphans: Option<bool>,
    ) -> Option<impl Future<Output = ()> + Send + use<>> {
        {
            let mut state = self.lock();
            if state.running {
                return None;
            }
            state.running = true;
        }
        let options = IntegrityOptions {
            remove_orphans: remove_orphans.unwrap_or(self.config.remove_orphans),
            orphan_grace: self.config.orphan_grace(),
        };
        let checker = self.clone();
        // Cleared even if the check is dropped (cancelled at shutdown) or panics.
        let guard = Running(self.clone());
        Some(async move {
            let report = checker.store.check_integrity(options).await;
            checker.lock().last = Some(report);
            drop(guard);
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

struct Running(Arc<Checker>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.lock().running = false;
    }
}

impl IntegrityConfig {
    fn interval(&self) -> Option<Duration> {
        (self.interval_hours > 0).then(|| Duration::from_secs(self.interval_hours * 3600))
    }

    fn orphan_grace(&self) -> Duration {
        Duration::from_secs(self.orphan_grace_minutes.saturating_mul(60)).max(MIN_ORPHAN_GRACE)
    }
}
