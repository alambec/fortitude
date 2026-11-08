//! Retention: deletion of whole studies by the age of their most recent series.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use fortitude_store::retention::RetentionOptions;
use fortitude_store::{Store, StoreOptions, Stored};

fn open() -> (tempfile::TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(StoreOptions {
        path: dir.path().to_owned(),
        fsync: false,
        files: fortitude_testkit::file_backend(),
        index: fortitude_testkit::postgres_index().map_or(
            fortitude_store::IndexBackend::Sqlite,
            fortitude_store::IndexBackend::Postgres,
        ),
        index_readers: 2,
        location_cache_bytes: 1 << 20,
        location_cache_ttl: std::time::Duration::from_secs(60),
        inline_binary_max: 64,
    })
    .unwrap();
    (dir, store)
}

async fn store_series(store: &Arc<Store>, study_uid: &str, series_uid: &str, count: u32) {
    for instance in fortitude_testkit::series(study_uid, series_uid, count) {
        let result = store.store(Bytes::from(instance.to_bytes()), None).await;
        assert_eq!(result.outcome.unwrap(), Stored::New);
    }
}

/// Retention whose cutoff is `mark`.
fn since(mark: Instant, dry_run: bool) -> RetentionOptions {
    RetentionOptions {
        max_age: mark.elapsed(),
        dry_run,
    }
}

/// Lets stored and later-stored data fall on both sides of a millisecond mark.
async fn tick() -> Instant {
    tokio::time::sleep(Duration::from_millis(20)).await;
    let mark = Instant::now();
    tokio::time::sleep(Duration::from_millis(20)).await;
    mark
}

#[tokio::test]
async fn expired_studies_are_deleted_whole() {
    let (_dir, s) = open();
    store_series(&s, "1.1", "1.1.1", 2).await;
    store_series(&s, "1.1", "1.1.2", 1).await;
    store_series(&s, "2.1", "2.1.1", 2).await;
    let mark = tick().await;
    // A new series keeps its whole study.
    store_series(&s, "2.1", "2.1.2", 1).await;
    store_series(&s, "3.1", "3.1.1", 1).await;

    let nothing = RetentionOptions {
        max_age: Duration::from_secs(86_400),
        dry_run: false,
    };
    let report = s.apply_retention(nothing).await;
    assert_eq!((report.studies, report.instances), (0, 0), "{report:?}");

    let report = s.apply_retention(since(mark, true)).await;
    assert!(report.dry_run);
    assert_eq!((report.studies, report.instances), (1, 3), "{report:?}");
    assert_eq!(report.expired[0].study_uid, "1.1");
    assert_eq!(report.expired[0].instances, 3);
    assert_eq!(s.statistics().await.unwrap().instances, 7);

    let report = s.apply_retention(since(mark, false)).await;
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!((report.studies, report.instances), (1, 3), "{report:?}");
    let left = s.statistics().await.unwrap();
    assert_eq!((left.studies, left.series, left.instances), (2, 3, 4));
    assert_eq!(s.files().keys().await.unwrap().len(), 4);
    assert!(s.integrity_is_clean().await);

    let report = s.apply_retention(since(mark, false)).await;
    assert_eq!(report.studies, 0, "{report:?}");
}

#[tokio::test]
async fn a_series_stored_before_deletion_saves_its_study() {
    let (_dir, s) = open();
    store_series(&s, "1.1", "1.1.1", 1).await;
    // As if retention had looked the study up before this series arrived.
    let paths = s
        .index()
        .delete_study_unchanged_since("1.1", 0)
        .await
        .unwrap();
    assert!(paths.is_empty());
    assert_eq!(s.statistics().await.unwrap().instances, 1);

    let mark = tick().await;
    let report = s.apply_retention(since(mark, false)).await;
    assert_eq!(report.studies, 1, "{report:?}");
    assert_eq!(s.statistics().await.unwrap().instances, 0);
    assert!(s.files().keys().await.unwrap().is_empty());
}

#[tokio::test]
async fn many_studies_are_paged() {
    let (_dir, s) = open();
    for study in 0..250 {
        store_series(&s, &format!("1.{study}"), &format!("1.{study}.1"), 1).await;
    }
    let mark = tick().await;
    store_series(&s, "2.1", "2.1.1", 1).await;
    let report = s.apply_retention(since(mark, true)).await;
    assert_eq!(report.studies, 250, "{report:?}");
    let report = s.apply_retention(since(mark, false)).await;
    assert_eq!(report.studies, 250, "{report:?}");
    assert_eq!(s.statistics().await.unwrap().studies, 1);
}

trait Clean {
    async fn integrity_is_clean(&self) -> bool;
}

impl Clean for Arc<Store> {
    async fn integrity_is_clean(&self) -> bool {
        let options = fortitude_store::integrity::IntegrityOptions {
            remove_orphans: false,
            orphan_grace: Duration::ZERO,
            verify_checksums: true,
        };
        let report = self.check_integrity(options).await;
        report.is_clean() && report.orphans == 0
    }
}
