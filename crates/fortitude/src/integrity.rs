//! Integrity checks of the stored files against the index: periodic, and on request through
//! the administration API. One runs at a time; the report of the last one is kept.

use std::sync::Arc;
use std::time::Duration;

use fortitude_store::Store;
use fortitude_store::integrity::{IntegrityOptions, IntegrityReport};
use tokio_util::sync::CancellationToken;

use crate::config::IntegrityConfig;
use crate::jobs::{self, Job, JobState};

/// Shortest grace period for orphan files, whatever the configuration: a file is written
/// before its instance is indexed, so a younger file may belong to an ingest in progress.
pub const MIN_ORPHAN_GRACE: Duration = Duration::from_secs(5 * 60);

pub struct Checker {
    store: Arc<Store>,
    config: IntegrityConfig,
    job: Arc<Job<IntegrityReport>>,
}

impl Checker {
    pub fn new(store: Arc<Store>, config: IntegrityConfig) -> Arc<Self> {
        Arc::new(Self {
            store,
            config,
            job: Job::new("integrity check"),
        })
    }

    pub fn state(&self) -> JobState<IntegrityReport> {
        self.job.state()
    }

    /// Start a check in the background, unless one is running; `remove_orphans` and
    /// `verify_checksums` override the configuration. Returns whether it started.
    pub fn start(&self, remove_orphans: Option<bool>, verify_checksums: Option<bool>) -> bool {
        self.job.start(self.check(remove_orphans, verify_checksums))
    }

    /// Check every `interval_hours`, the first time one interval after the start, until
    /// `shutdown`.
    pub async fn run_periodically(self: Arc<Self>, shutdown: CancellationToken) {
        if let Some(interval) = jobs::hours(self.config.interval_hours) {
            let job = self.job.clone();
            job.run_periodically(interval, shutdown, || self.check(None, None))
                .await;
        }
    }

    fn check(
        &self,
        remove_orphans: Option<bool>,
        verify_checksums: Option<bool>,
    ) -> impl Future<Output = IntegrityReport> + use<> {
        let store = self.store.clone();
        let options = IntegrityOptions {
            remove_orphans: remove_orphans.unwrap_or(self.config.remove_orphans),
            orphan_grace: Duration::from_secs(self.config.orphan_grace_minutes.saturating_mul(60))
                .max(MIN_ORPHAN_GRACE),
            verify_checksums: verify_checksums.unwrap_or(self.config.verify_checksums),
        };
        async move { store.check_integrity(options).await }
    }
}
