//! Retention: deletion of the studies whose series were all stored more than
//! `retention.max_age_days` ago, periodically and on request through the administration API.
//! One run at a time; the report of the last one is kept.

use std::sync::Arc;
use std::time::Duration;

use fortitude_store::Store;
use fortitude_store::retention::{RetentionOptions, RetentionReport};
use tokio_util::sync::CancellationToken;

use crate::config::RetentionConfig;
use crate::jobs::{self, Job, JobState};

pub struct Retention {
    store: Arc<Store>,
    config: RetentionConfig,
    job: Arc<Job<RetentionReport>>,
}

impl Retention {
    pub fn new(store: Arc<Store>, config: RetentionConfig) -> Arc<Self> {
        Arc::new(Self {
            store,
            config,
            job: Job::new("retention"),
        })
    }

    pub fn state(&self) -> JobState<RetentionReport> {
        self.job.state()
    }

    /// Start a run in the background, unless one is running (`Ok(false)`). `max_age_days`
    /// overrides the configuration; without either, the error message.
    pub fn start(&self, dry_run: bool, max_age_days: Option<u64>) -> Result<bool, &'static str> {
        let days = match max_age_days.or(self.configured()) {
            Some(0) => return Err("max_age_days must be positive"),
            Some(days) => days,
            None => return Err("retention is not configured: set retention.max_age_days"),
        };
        Ok(self.job.start(self.run(days, dry_run)))
    }

    /// Run every `interval_hours` (when `max_age_days` is set), the first time one interval
    /// after the start, until `shutdown`.
    pub async fn run_periodically(self: Arc<Self>, shutdown: CancellationToken) {
        let (Some(days), Some(interval)) =
            (self.configured(), jobs::hours(self.config.interval_hours))
        else {
            return;
        };
        tracing::info!(max_age_days = days, "retention enabled");
        let job = self.job.clone();
        job.run_periodically(interval, shutdown, || self.run(days, false))
            .await;
    }

    fn configured(&self) -> Option<u64> {
        (self.config.max_age_days > 0).then_some(self.config.max_age_days)
    }

    fn run(&self, days: u64, dry_run: bool) -> impl Future<Output = RetentionReport> + use<> {
        let store = self.store.clone();
        let options = RetentionOptions {
            max_age: Duration::from_secs(days.saturating_mul(86_400)),
            dry_run,
        };
        async move { store.apply_retention(options).await }
    }
}
