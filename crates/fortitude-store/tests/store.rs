use std::sync::Arc;

use bytes::Bytes;
use fortitude_core::json;
use fortitude_store::index::Value;
use fortitude_store::pixel::PixelLayout;
use fortitude_store::{Store, StoreFailure, StoreOptions, Stored};
use fortitude_testkit::{IMPLICIT_VR_LE, Instance, JPEG_BASELINE, Pixels};

fn open() -> (tempfile::TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(StoreOptions {
        path: dir.path().to_owned(),
        fsync: false,
        index: fortitude_testkit::postgres_index().map_or(
            fortitude_store::IndexBackend::Sqlite,
            fortitude_store::IndexBackend::Postgres,
        ),
        index_readers: 2,
        inline_binary_max: 64,
    })
    .unwrap();
    (dir, store)
}

async fn store(store: &Arc<Store>, instance: &Instance) -> Result<Stored, StoreFailure> {
    store
        .store(Bytes::from(instance.to_bytes()), None)
        .await
        .outcome
}

struct Row {
    path: String,
    frames: i64,
    pixel: Option<Vec<u8>>,
    metadata: Vec<u8>,
}

async fn row(store: &Arc<Store>, sop_uid: &str) -> Row {
    let sop_uid = Value::Text(sop_uid.to_owned());
    store
        .index()
        .readers()
        .run(move |conn| {
            conn.query_row(
                "SELECT path, frames, pixel, metadata FROM instances WHERE sop_uid = ?",
                &[sop_uid],
                |r| {
                    Ok(Row {
                        path: r.str(0)?.to_owned(),
                        frames: r.i64(1)?,
                        pixel: r.opt_bytes(2)?.map(<[u8]>::to_vec),
                        metadata: r.bytes(3)?.to_vec(),
                    })
                },
            )
        })
        .await
        .unwrap()
        .expect("no such instance")
}

/// Read every frame back from the stored file through the recorded layout.
async fn stored_frames(store: &Arc<Store>, sop_uid: &str) -> Vec<Vec<u8>> {
    let row = row(store, sop_uid).await;
    let file = std::fs::read(store.files().path(&row.path)).unwrap();
    let layout = PixelLayout::decode(&row.pixel.expect("pixel layout")).unwrap();
    (0..layout.frame_count())
        .map(|i| {
            layout
                .frame(i)
                .unwrap()
                .iter()
                .flat_map(|r| file[r.offset as usize..(r.offset + r.length) as usize].to_vec())
                .collect()
        })
        .collect()
}

fn metadata_json(metadata: &[u8]) -> serde_json::Value {
    let mut out = Vec::new();
    json::render(metadata, "http://base", &mut out);
    let mut object = b"{".to_vec();
    object.extend(out);
    object.push(b'}');
    serde_json::from_slice(&object).unwrap()
}

#[tokio::test]
async fn native_frames_are_located() {
    let (_dir, s) = open();
    let instance = Instance::new("1.2.3", "1.2.3.4", "1.2.3.4.5").with(|i| i.frames = 3);
    assert_eq!(store(&s, &instance).await.unwrap(), Stored::New);
    let frames = stored_frames(&s, "1.2.3.4.5").await;
    assert_eq!(
        frames,
        (0..3).map(|i| instance.frame(i)).collect::<Vec<_>>()
    );
    assert_eq!(row(&s, "1.2.3.4.5").await.frames, 3);
}

#[tokio::test]
async fn implicit_vr_frames_are_located() {
    let (_dir, s) = open();
    let instance = Instance::new("1.2.3", "1.2.3.4", "1.2.3.4.6").with(|i| {
        i.transfer_syntax = IMPLICIT_VR_LE.into();
        i.frames = 2;
    });
    store(&s, &instance).await.unwrap();
    assert_eq!(
        stored_frames(&s, "1.2.3.4.6").await,
        vec![instance.frame(0), instance.frame(1)]
    );
}

#[tokio::test]
async fn encapsulated_fragments_are_attributed_to_frames() {
    let (_dir, s) = open();
    for (sop, fragments_per_frame) in [("1.9.1", 1), ("1.9.2", 3)] {
        let instance = Instance::new("1.9", "1.9.0", sop).with(|i| {
            i.transfer_syntax = JPEG_BASELINE.into();
            i.pixels = Pixels::Encapsulated {
                fragments_per_frame,
            };
            i.frames = 4;
        });
        store(&s, &instance).await.unwrap();
        let frames = stored_frames(&s, sop).await;
        assert_eq!(frames.len(), 4, "{fragments_per_frame} fragments per frame");
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(frame, &instance.frame(i as u32));
        }
    }
}

#[tokio::test]
async fn metadata_is_pre_encoded_with_bulk_data_uris() {
    let (_dir, s) = open();
    let instance = Instance::new("1.5", "1.5.1", "1.5.1.1").with(|i| {
        i.charset = Some("ISO_IR 100".into());
        i.patient_name = "Gagné^Hélène".into();
    });
    store(&s, &instance).await.unwrap();
    let metadata = metadata_json(&row(&s, "1.5.1.1").await.metadata);
    assert_eq!(
        metadata["00100010"]["Value"][0]["Alphabetic"],
        "Gagné^Hélène"
    );
    assert_eq!(metadata["00080005"]["Value"][0], "ISO_IR 192");
    assert_eq!(
        metadata["7FE00010"]["BulkDataURI"],
        "http://base/studies/1.5/series/1.5.1/instances/1.5.1.1/bulk/7FE00010"
    );
}

#[tokio::test]
async fn duplicates_are_discarded() {
    let (dir, s) = open();
    let instance = Instance::new("1.2.3", "1.2.3.4", "1.2.3.4.5");
    assert_eq!(store(&s, &instance).await.unwrap(), Stored::New);
    assert_eq!(store(&s, &instance).await.unwrap(), Stored::Duplicate);
    let files = walkdir_count(&dir.path().join("files"));
    assert_eq!(files, 1);
}

#[tokio::test]
async fn invalid_instances_are_rejected() {
    let (_dir, s) = open();
    let garbage = s.store(Bytes::from_static(b"not a dicom file"), None).await;
    assert_eq!(garbage.outcome.unwrap_err().failure_reason(), 0xC000);

    let bad_uid = Instance::new("1.02.abc", "1.2.3.4", "1.2.3.4.7");
    let result = s.store(Bytes::from(bad_uid.to_bytes()), None).await;
    assert_eq!(
        result.sop_uid.as_deref(),
        Some("1.2.3.4.7"),
        "{:?}",
        result.outcome
    );
    assert_eq!(result.outcome.unwrap_err().failure_reason(), 0xA900);

    let other_study = Instance::new("1.2.3", "1.2.3.4", "1.2.3.4.5");
    let result = s
        .store(Bytes::from(other_study.to_bytes()), Some("1.2.99".into()))
        .await;
    assert!(matches!(
        result.outcome,
        Err(StoreFailure::StudyMismatch { .. })
    ));

    store(&s, &other_study).await.unwrap();
    let moved_series = Instance::new("1.2.4", "1.2.3.4", "1.2.3.4.6");
    assert!(matches!(
        store(&s, &moved_series).await,
        Err(StoreFailure::SeriesConflict(_))
    ));
}

#[tokio::test]
async fn concurrent_stores_keep_counts_consistent() {
    let (_dir, s) = open();
    let mut tasks = Vec::new();
    for series in 0..4 {
        for i in 0..50 {
            let s = s.clone();
            let instance = Instance::new(
                "1.7",
                &format!("1.7.{series}"),
                &format!("1.7.{series}.{i}"),
            )
            .with(|inst| {
                inst.modality = if series % 2 == 0 {
                    "CT".into()
                } else {
                    "SR".into()
                }
            });
            tasks.push(tokio::spawn(async move { store(&s, &instance).await }));
        }
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap(), Stored::New);
    }
    let (series, instances, modalities): (i64, i64, String) = s
        .index()
        .readers()
        .run(|conn| {
            conn.query_row(
                "SELECT num_series, num_instances, modalities FROM studies WHERE study_uid = '1.7'",
                &[],
                |r| Ok((r.i64(0)?, r.i64(1)?, r.str(2)?.to_owned())),
            )
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!((series, instances), (4, 200));
    let mut modalities: Vec<&str> = modalities.split('\\').collect();
    modalities.sort_unstable();
    assert_eq!(modalities, ["CT", "SR"]);
}

fn walkdir_count(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                walkdir_count(&e.path())
            } else {
                1
            }
        })
        .sum()
}

#[tokio::test]
async fn deletion_keeps_counts_consistent() {
    use fortitude_store::retrieve::Scope;
    let (dir, s) = open();
    for (series, modality, count) in [("1.8.1", "CT", 3), ("1.8.2", "SR", 1)] {
        for i in 0..count {
            let instance = Instance::new("1.8", series, &format!("{series}.{i}"))
                .with(|inst| inst.modality = modality.into());
            store(&s, &instance).await.unwrap();
        }
    }
    let study = |s: Arc<Store>| async move {
        s.index()
            .readers()
            .run(|conn| {
                conn.query_row(
                    "SELECT num_series, num_instances, modalities, rep_instance FROM studies",
                    &[],
                    |r| {
                        Ok((
                            r.i64(0)?,
                            r.i64(1)?,
                            r.str(2)?.to_owned(),
                            r.opt_i64(3)?.is_some(),
                        ))
                    },
                )
            })
            .await
            .unwrap()
            .unwrap()
    };
    assert_eq!(s.statistics().await.unwrap().instances, 4);

    assert_eq!(
        s.delete(Scope::instance("1.8", "1.8.1", "1.8.1.0"))
            .await
            .unwrap(),
        1
    );
    assert_eq!(study(s.clone()).await, (2, 3, "CT\\SR".into(), true));

    assert_eq!(s.delete(Scope::series("1.8", "1.8.2")).await.unwrap(), 1);
    assert_eq!(study(s.clone()).await, (1, 2, "CT".into(), true));
    assert_eq!(s.delete(Scope::series("1.8", "1.8.2")).await.unwrap(), 0);

    assert_eq!(s.delete(Scope::study("1.8")).await.unwrap(), 2);
    let stats = s.statistics().await.unwrap();
    assert_eq!(
        (stats.studies, stats.series, stats.instances, stats.size),
        (0, 0, 0, 0)
    );
    assert_eq!(walkdir_count(&dir.path().join("files")), 0);

    // The study can be stored again.
    store(&s, &Instance::new("1.8", "1.8.1", "1.8.1.0"))
        .await
        .unwrap();
    assert_eq!(s.statistics().await.unwrap().studies, 1);
}
