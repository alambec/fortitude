//! Modalities registered at runtime, kept in the index.

use fortitude_store::modalities::Modality;
use fortitude_store::{Store, StoreOptions};

#[tokio::test]
async fn register_update_and_remove() {
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
    let ct = Modality {
        ae_title: "CT ROOM 3".into(),
        host: "10.0.0.3".into(),
        port: 104,
        description: "CT".into(),
    };
    assert!(store.put_modality(ct.clone()).await.unwrap());
    let pacs = Modality {
        ae_title: "PACS".into(),
        host: "pacs.example.org".into(),
        port: 11112,
        description: String::new(),
    };
    assert!(store.put_modality(pacs.clone()).await.unwrap());
    assert_eq!(
        store.modalities().await.unwrap(),
        [ct.clone(), pacs.clone()]
    );

    let moved = Modality {
        port: 4242,
        ..ct.clone()
    };
    assert!(!store.put_modality(moved.clone()).await.unwrap());
    assert_eq!(store.modalities().await.unwrap(), [moved, pacs.clone()]);

    assert!(store.delete_modality("CT ROOM 3").await.unwrap());
    assert!(!store.delete_modality("CT ROOM 3").await.unwrap());
    assert_eq!(store.modalities().await.unwrap(), [pacs]);
}
