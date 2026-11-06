//! Integrity check of the stored files against the index.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use fortitude_store::files::FileStore;
use fortitude_store::index::Value;
use fortitude_store::integrity::{Crc32, IntegrityOptions, ProblemKind};
use fortitude_store::retrieve::Scope;
use fortitude_store::{IndexBackend, Store, StoreOptions, Stored};
use fortitude_testkit::Instance;

fn open() -> (tempfile::TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(StoreOptions {
        path: dir.path().to_owned(),
        fsync: false,
        files: fortitude_testkit::file_backend(),
        index: fortitude_testkit::postgres_index()
            .map_or(IndexBackend::Sqlite, IndexBackend::Postgres),
        index_readers: 2,
        inline_binary_max: 64,
    })
    .unwrap();
    (dir, store)
}

const REMOVE: IntegrityOptions = IntegrityOptions {
    remove_orphans: true,
    orphan_grace: Duration::ZERO,
    verify_checksums: false,
};

const VERIFY: IntegrityOptions = IntegrityOptions {
    verify_checksums: true,
    ..REMOVE
};

/// Flip a bit in the middle of a stored file, keeping its size; returns its bytes before and
/// after.
async fn damage(store: &Arc<Store>, path: &str) -> (Bytes, Bytes) {
    let bytes = store.files().read(path).await.unwrap();
    let mut damaged = bytes.to_vec();
    damaged[bytes.len() / 2] ^= 0x10;
    let damaged = Bytes::from(damaged);
    store.files().write(path, damaged.clone()).await.unwrap();
    (bytes, damaged)
}

async fn store_series(store: &Arc<Store>, series_uid: &str, count: u32) -> Vec<Instance> {
    let instances = fortitude_testkit::series("1.2.3", series_uid, count);
    for instance in &instances {
        let result = store.store(Bytes::from(instance.to_bytes()), None).await;
        assert_eq!(result.outcome.unwrap(), Stored::New);
    }
    instances
}

async fn path(store: &Arc<Store>, sop_uid: &str) -> String {
    let sop_uid = Value::Text(sop_uid.to_owned());
    store
        .index()
        .readers()
        .run(move |conn| {
            conn.query_row(
                "SELECT path FROM instances WHERE sop_uid = ?",
                &[sop_uid],
                |r| Ok(r.str(0)?.to_owned()),
            )
        })
        .await
        .unwrap()
        .expect("no such instance")
}

#[tokio::test]
async fn consistent_store_is_clean() {
    let (_dir, s) = open();
    let report = s.check_integrity(REMOVE).await;
    assert!(report.is_clean(), "{report:?}");
    assert_eq!((report.files, report.instances), (0, 0));

    store_series(&s, "1.2.3.1", 5).await;
    let report = s.check_integrity(REMOVE).await;
    assert!(report.is_clean(), "{report:?}");
    assert_eq!((report.files, report.instances), (5, 5));
    assert_eq!((report.orphans, report.orphans_removed), (0, 0));
    assert_eq!(s.files().keys().await.unwrap().len(), 5);
}

#[tokio::test]
async fn orphans_are_removed_after_the_grace_period() {
    let (_dir, s) = open();
    store_series(&s, "1.2.3.1", 2).await;
    let orphan = FileStore::new_path();
    s.files()
        .write(&orphan, Bytes::from_static(b"left by a crash"))
        .await
        .unwrap();

    // Young: possibly an ingest in progress.
    let options = IntegrityOptions {
        orphan_grace: Duration::from_secs(3600),
        ..REMOVE
    };
    let report = s.check_integrity(options).await;
    assert_eq!((report.orphans, report.recent_files), (0, 1), "{report:?}");
    assert_eq!(s.files().size(&orphan).await.unwrap(), Some(15));

    // Only reported.
    let options = IntegrityOptions {
        remove_orphans: false,
        ..REMOVE
    };
    let report = s.check_integrity(options).await;
    assert_eq!((report.orphans, report.orphans_size), (1, 15), "{report:?}");
    assert_eq!(report.orphans_removed, 0);
    assert!(report.is_clean());
    assert_eq!(s.files().size(&orphan).await.unwrap(), Some(15));

    let report = s.check_integrity(REMOVE).await;
    assert_eq!(
        (report.orphans, report.orphans_removed),
        (1, 1),
        "{report:?}"
    );
    assert_eq!(report.files, 3);
    assert!(report.is_clean());
    assert_eq!(s.files().size(&orphan).await.unwrap(), None);
    assert_eq!(s.files().keys().await.unwrap().len(), 2);
}

#[tokio::test]
async fn foreign_files_are_left_alone() {
    let (_dir, s) = open();
    for key in [
        "ab/notes.txt",
        "ab/cd/abcd0123456789abcdef0123456789ab.dcm.part",
    ] {
        s.files()
            .write(key, Bytes::from_static(b"not ours"))
            .await
            .unwrap();
    }
    let report = s.check_integrity(REMOVE).await;
    assert_eq!((report.foreign_files, report.orphans), (2, 0), "{report:?}");
    assert_eq!(s.files().keys().await.unwrap().len(), 2);
}

#[tokio::test]
async fn missing_and_damaged_files_are_reported() {
    let (_dir, s) = open();
    let instances = store_series(&s, "1.2.3.1", 3).await;
    let missing = path(&s, &instances[0].sop_uid).await;
    s.files().remove(&missing).await.unwrap();
    let damaged = path(&s, &instances[1].sop_uid).await;
    let bytes = s.files().read(&damaged).await.unwrap();
    s.files()
        .write(&damaged, bytes.slice(..bytes.len() / 2))
        .await
        .unwrap();

    let report = s.check_integrity(REMOVE).await;
    assert!(!report.is_clean());
    assert_eq!(
        (report.missing, report.size_mismatches),
        (1, 1),
        "{report:?}"
    );
    assert_eq!((report.files, report.instances), (2, 3));
    let problem = |kind| report.problems.iter().find(|p| p.kind == kind).unwrap();
    let gone = problem(ProblemKind::Missing);
    assert_eq!(gone.sop_uid, instances[0].sop_uid);
    assert_eq!(
        (gone.study_uid.as_str(), gone.series_uid.as_str()),
        ("1.2.3", "1.2.3.1")
    );
    assert_eq!(
        (gone.path.as_str(), gone.actual_size),
        (missing.as_str(), None)
    );
    let short = problem(ProblemKind::SizeMismatch);
    assert_eq!(short.sop_uid, instances[1].sop_uid);
    assert_eq!(short.expected_size, bytes.len() as u64);
    assert_eq!(short.actual_size, Some(bytes.len() as u64 / 2));

    // Reported, never deleted: the index still lists both instances.
    assert_eq!(s.statistics().await.unwrap().instances, 3);
}

#[tokio::test]
async fn concurrent_ingest_is_not_disturbed() {
    let (_dir, s) = open();
    let options = IntegrityOptions {
        orphan_grace: Duration::from_secs(60),
        ..VERIFY
    };
    let ingest = {
        let s = s.clone();
        tokio::spawn(async move { store_series(&s, "1.2.3.1", 200).await })
    };
    let mut reports = Vec::new();
    while !ingest.is_finished() {
        reports.push(s.check_integrity(options).await);
    }
    let instances = ingest.await.unwrap();
    for report in &reports {
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.orphans, 0, "{report:?}");
    }
    let report = s.check_integrity(options).await;
    assert!(report.is_clean(), "{report:?}");
    assert_eq!((report.files, report.instances), (200, 200));
    assert_eq!(report.checksums_verified, 200);
    for instance in &instances {
        let path = path(&s, &instance.sop_uid).await;
        assert!(s.files().size(&path).await.unwrap().is_some());
    }
}

#[tokio::test]
async fn study_and_series_checks() {
    let (_dir, s) = open();
    let first = store_series(&s, "1.2.3.1", 3).await;
    store_series(&s, "1.2.3.2", 2).await;
    let missing = path(&s, &first[1].sop_uid).await;
    s.files().remove(&missing).await.unwrap();
    // Orphans are not attributable to a study: left to the full check.
    let orphan = FileStore::new_path();
    s.files()
        .write(&orphan, Bytes::from_static(b"x"))
        .await
        .unwrap();

    let study = s.check_scope_integrity(Scope::study("1.2.3"), true).await;
    let study = study.unwrap().expect("stored study");
    assert_eq!(study.study_uid.as_deref(), Some("1.2.3"));
    assert_eq!(study.series_uid, None);
    assert_eq!((study.files, study.instances), (4, 5));
    assert_eq!((study.missing, study.orphans), (1, 0), "{study:?}");
    assert_eq!(study.problems[0].sop_uid, first[1].sop_uid);
    assert_eq!(study.problems[0].kind, ProblemKind::Missing);

    let broken = s
        .check_scope_integrity(Scope::series("1.2.3", "1.2.3.1"), true)
        .await;
    let broken = broken.unwrap().unwrap();
    assert_eq!((broken.instances, broken.missing), (3, 1));
    assert_eq!(broken.series_uid.as_deref(), Some("1.2.3.1"));
    let intact = s
        .check_scope_integrity(Scope::series("1.2.3", "1.2.3.2"), true)
        .await;
    let intact = intact.unwrap().unwrap();
    assert!(intact.is_clean(), "{intact:?}");
    assert_eq!((intact.files, intact.instances), (2, 2));
    assert_eq!(intact.checksums_verified, 2);

    for unknown in [Scope::study("9.9"), Scope::series("1.2.3", "9.9")] {
        assert!(
            s.check_scope_integrity(unknown, true)
                .await
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(s.files().size(&orphan).await.unwrap(), Some(1));
}

#[tokio::test]
async fn damaged_files_of_the_right_size_are_found_by_their_checksum() {
    let (_dir, s) = open();
    let instances = store_series(&s, "1.2.3.1", 3).await;
    let path = path(&s, &instances[0].sop_uid).await;
    let (bytes, damaged) = damage(&s, &path).await;

    // Same size: only the checksum tells.
    let report = s.check_integrity(REMOVE).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(!report.verify_checksums);
    assert_eq!(
        (report.checksums_verified, report.checksum_mismatches),
        (0, 0)
    );

    let report = s.check_integrity(VERIFY).await;
    assert!(!report.is_clean());
    assert!(report.verify_checksums);
    assert_eq!(
        (report.checksums_verified, report.checksum_mismatches),
        (2, 1),
        "{report:?}"
    );
    assert_eq!(
        (report.files, report.missing, report.size_mismatches),
        (3, 0, 0)
    );
    let problem = &report.problems[0];
    assert_eq!(problem.kind, ProblemKind::ChecksumMismatch);
    assert_eq!(problem.sop_uid, instances[0].sop_uid);
    assert_eq!(problem.path, path);
    assert_eq!(problem.expected_size, bytes.len() as u64);
    assert_eq!(problem.actual_size, Some(bytes.len() as u64));
    assert_eq!(problem.expected_crc32, Some(Crc32(crc32fast::hash(&bytes))));
    assert_eq!(problem.actual_crc32, Some(Crc32(crc32fast::hash(&damaged))));
    let json = serde_json::to_value(problem).unwrap();
    assert_eq!(json["kind"], "checksum_mismatch");
    assert_eq!(
        json["expected_crc32"],
        format!("{:08x}", crc32fast::hash(&bytes))
    );

    let scope = Scope::series("1.2.3", "1.2.3.1");
    let series = s.check_scope_integrity(scope.clone(), true).await;
    let series = series.unwrap().unwrap();
    assert_eq!(
        (series.checksums_verified, series.checksum_mismatches),
        (2, 1),
        "{series:?}"
    );
    assert_eq!(series.files, 3);
    assert_eq!(series.problems[0].kind, ProblemKind::ChecksumMismatch);
    let unverified = s
        .check_scope_integrity(scope, false)
        .await
        .unwrap()
        .unwrap();
    assert!(unverified.is_clean(), "{unverified:?}");

    // Reported, never deleted.
    assert_eq!(s.statistics().await.unwrap().instances, 3);
}

#[tokio::test]
async fn concurrent_deletions_are_not_reported() {
    let (_dir, s) = open();
    let instances = store_series(&s, "1.2.3.1", 100).await;
    let deletions = {
        let s = s.clone();
        tokio::spawn(async move {
            for instance in &instances {
                let scope = Scope::instance("1.2.3", "1.2.3.1", &instance.sop_uid);
                assert_eq!(s.delete(scope).await.unwrap(), 1);
            }
        })
    };
    let mut reports = Vec::new();
    while !deletions.is_finished() {
        reports.push(s.check_integrity(VERIFY).await);
        if let Some(study) = s
            .check_scope_integrity(Scope::study("1.2.3"), true)
            .await
            .unwrap()
        {
            reports.push(study);
        }
    }
    deletions.await.unwrap();
    for report in &reports {
        assert!(report.is_clean(), "{report:?}");
    }
    let report = s.check_integrity(VERIFY).await;
    assert_eq!((report.files, report.instances), (0, 0), "{report:?}");
}
