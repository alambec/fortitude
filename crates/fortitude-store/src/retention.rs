//! Retention: deletion of the studies whose most recently stored series is older than a
//! maximum age.
//!
//! A study is kept whole: it expires only once **every** series of it was stored before the
//! cutoff, so a study that keeps receiving series (priors sent again, late series) is not
//! deleted piece by piece. Expiry is checked again by the index writer, in the transaction
//! that deletes the study: a series stored while retention runs saves its study.
//!
//! Each deleted study is logged. Files that cannot be removed are reported, and left to the
//! [integrity check](crate::integrity) as orphans.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::Store;
use crate::index::{IndexError, Value};

/// Studies listed in a report; the counts include the others.
pub const MAX_LISTED_STUDIES: usize = 1000;
/// Errors listed in a report.
const MAX_LISTED_ERRORS: usize = 100;
/// Expired studies looked up per index query.
const PAGE: i64 = 100;

#[derive(Debug, Clone, Copy)]
pub struct RetentionOptions {
    /// Studies whose series were all stored more than this long ago expire.
    pub max_age: Duration,
    /// Only list the expired studies.
    pub dry_run: bool,
}

/// Result of a retention run.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RetentionReport {
    /// Start, in seconds since the Unix epoch.
    pub started_at: u64,
    pub duration_seconds: f64,
    pub dry_run: bool,
    pub max_age_days: f64,
    /// Studies whose last series was stored before this (seconds since the Unix epoch)
    /// expire.
    pub cutoff: u64,
    /// Expired studies, and their instances: deleted, or to be deleted in a dry run.
    pub studies: u64,
    pub instances: u64,
    /// The first [`MAX_LISTED_STUDIES`] expired studies.
    pub expired: Vec<ExpiredStudy>,
    /// Studies that could not be deleted, and files that could not be removed.
    pub errors: Vec<String>,
}

/// A study past its retention period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExpiredStudy {
    pub study_uid: String,
    pub patient_id: Option<String>,
    pub study_date: Option<String>,
    pub accession_number: Option<String>,
    pub instances: u64,
    /// When its most recent series was stored (seconds since the Unix epoch).
    pub last_stored_at: u64,
}

impl RetentionReport {
    fn push_error(&mut self, error: String) {
        tracing::error!(error, "retention");
        if self.errors.len() < MAX_LISTED_ERRORS {
            self.errors.push(error);
        }
    }
}

impl Store {
    /// Delete the expired studies (or only list them); see the [module](self) documentation.
    pub async fn apply_retention(self: &Arc<Self>, options: RetentionOptions) -> RetentionReport {
        let started = Instant::now();
        let now = SystemTime::now();
        let cutoff = now.checked_sub(options.max_age).unwrap_or(UNIX_EPOCH);
        let millis = |t: SystemTime| {
            t.duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64)
        };
        let cutoff_ms = millis(cutoff);
        let mut report = RetentionReport {
            started_at: (millis(now) / 1000) as u64,
            dry_run: options.dry_run,
            max_age_days: options.max_age.as_secs_f64() / 86_400.0,
            cutoff: (cutoff_ms / 1000) as u64,
            ..RetentionReport::default()
        };
        tracing::info!(
            dry_run = options.dry_run,
            max_age_days = report.max_age_days,
            "retention started"
        );

        let mut after = (i64::MIN, 0);
        loop {
            let page = match self.expired_studies(cutoff_ms, after).await {
                Ok(page) => page,
                Err(e) => {
                    report.push_error(format!("cannot look up expired studies: {e}"));
                    break;
                }
            };
            let Some((last, _)) = page.last() else {
                break;
            };
            after = *last;
            for (_, study) in page {
                let deleted = if options.dry_run {
                    Some(study.instances)
                } else {
                    self.delete_expired(&study, cutoff_ms, &mut report).await
                };
                if let Some(instances) = deleted {
                    report.studies += 1;
                    report.instances += instances;
                    if report.expired.len() < MAX_LISTED_STUDIES {
                        report.expired.push(study);
                    }
                }
            }
        }

        report.duration_seconds = started.elapsed().as_secs_f64();
        tracing::info!(
            dry_run = report.dry_run,
            studies = report.studies,
            instances = report.instances,
            errors = report.errors.len(),
            seconds = report.duration_seconds,
            "retention finished"
        );
        if !options.dry_run {
            metrics::counter!("fortitude_retention_deleted_studies_total")
                .increment(report.studies);
            metrics::counter!("fortitude_retention_deleted_instances_total")
                .increment(report.instances);
            metrics::gauge!("fortitude_retention_last_run_timestamp_seconds")
                .set(report.started_at as f64);
        }
        report
    }

    /// Delete an expired study and its files; the number of its instances that were deleted,
    /// or `None` when it was kept (a series was stored since the lookup) or not deleted.
    async fn delete_expired(
        self: &Arc<Self>,
        study: &ExpiredStudy,
        cutoff_ms: i64,
        report: &mut RetentionReport,
    ) -> Option<u64> {
        let deleted = self
            .index()
            .delete_study_unchanged_since(&study.study_uid, cutoff_ms)
            .await;
        self.locations
            .invalidate(&crate::retrieve::Scope::study(&study.study_uid));
        let paths = match deleted {
            Ok(paths) if paths.is_empty() => return None,
            Ok(paths) => paths,
            Err(e) => {
                report.push_error(format!("cannot delete study {}: {e}", study.study_uid));
                return None;
            }
        };
        let count = paths.len() as u64;
        tracing::info!(
            study_uid = study.study_uid,
            patient_id = study.patient_id,
            study_date = study.study_date,
            accession_number = study.accession_number,
            instances = count,
            last_stored_at = study.last_stored_at,
            "retention: study deleted"
        );
        for e in self.files().remove_all(paths).await {
            report.push_error(format!(
                "cannot remove a file of study {}: {e}",
                study.study_uid
            ));
        }
        Some(count)
    }

    /// The next expired studies after `after`, in the order of their creation, with their
    /// creation time and primary key (the position to continue from). Only studies created
    /// before the cutoff are read, through the `studies_created_at` index.
    async fn expired_studies(
        &self,
        cutoff_ms: i64,
        after: (i64, i64),
    ) -> Result<Vec<((i64, i64), ExpiredStudy)>, IndexError> {
        self.index()
            .readers()
            .run(move |conn| {
                // A study is created with its first series: one created after the cutoff
                // cannot have expired.
                conn.query(
                    "SELECT st.created_at, st.pk, st.study_uid, st.patient_id, st.study_date,
                        st.accession_number, st.num_instances,
                        (SELECT max(s.created_at) FROM series s WHERE s.study_pk = st.pk)
                     FROM studies st
                     WHERE st.created_at < ?1 AND (st.created_at, st.pk) > (?2, ?3)
                       AND NOT EXISTS (
                           SELECT 1 FROM series s WHERE s.study_pk = st.pk AND s.created_at >= ?1
                       )
                     ORDER BY st.created_at, st.pk
                     LIMIT ?4",
                    &[
                        Value::Integer(cutoff_ms),
                        Value::Integer(after.0),
                        Value::Integer(after.1),
                        Value::Integer(PAGE),
                    ],
                    |r| {
                        let text = |i| r.opt_str(i).map(|v| v.map(str::to_owned));
                        Ok((
                            (r.i64(0)?, r.i64(1)?),
                            ExpiredStudy {
                                study_uid: r.str(2)?.to_owned(),
                                patient_id: text(3)?,
                                study_date: text(4)?,
                                accession_number: text(5)?,
                                instances: r.i64(6)? as u64,
                                last_stored_at: (r.opt_i64(7)?.unwrap_or(0) / 1000) as u64,
                            },
                        ))
                    },
                )
            })
            .await
    }
}
