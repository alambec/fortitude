//! End-to-end tests: a DIMSE server on an empty store, driven by a `dicom-ul` SCU, with a
//! storage SCP of the same kind as C-MOVE destination. The SCU receives C-GET
//! sub-operations itself.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use dicom_core::{DataElement, PrimitiveValue, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_encoding::TransferSyntaxIndex;
use dicom_object::InMemDicomObject;
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use fortitude_store::{Store, StoreOptions};
use fortitude_testkit::{EXPLICIT_VR_LE, IMPLICIT_VR_LE, Instance, JPEG_BASELINE, Pixels};
use tokio::net::TcpListener;

use super::*;
use crate::command::Command;

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

struct Harness {
    address: SocketAddr,
    store: Arc<Store>,
    _dir: tempfile::TempDir,
}

async fn harness(configure: impl FnOnce(&mut Config)) -> Harness {
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
    let mut config = Config::default();
    configure(&mut config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let dimse = Dimse {
        store: store.clone(),
        config: Arc::new(config),
    };
    tokio::spawn(serve(listener, dimse, std::future::pending()));
    Harness {
        address,
        store,
        _dir: dir,
    }
}

type Response = (Command, Option<InMemDicomObject>);

/// [`scu::Scu`] with the shortcuts of these tests: requests by abstract syntax, decoded
/// identifiers.
struct Client(scu::Scu);

impl Client {
    async fn connect(address: SocketAddr, contexts: &[(&str, &[&str])]) -> Self {
        Self::try_connect(address, "SCU", contexts).await.unwrap()
    }

    async fn try_connect(
        address: SocketAddr,
        calling: &str,
        contexts: &[(&str, &[&str])],
    ) -> Result<Self, association::Error> {
        let contexts: Vec<(String, Vec<String>)> = contexts
            .iter()
            .map(|(a, t)| (a.to_string(), t.iter().map(|t| t.to_string()).collect()))
            .collect();
        scu::Scu::connect(address, calling, "FORTITUDE", &contexts, TIMEOUT)
            .await
            .map(Self)
    }

    fn context(&self, abstract_syntax: &str) -> (u8, String) {
        let (id, ts) = self
            .0
            .context(abstract_syntax, None)
            .expect("accepted presentation context");
        (id, ts.to_owned())
    }

    async fn release(self) {
        self.0.release().await.unwrap();
    }

    /// Send a request and collect its responses, up to the final one.
    async fn request(
        &mut self,
        abstract_syntax: &str,
        field: u16,
        command: Command,
        data: Option<&[u8]>,
    ) -> Vec<Response> {
        let (pc, ts) = self.context(abstract_syntax);
        let command = command
            .with_u16(tags::COMMAND_FIELD, field)
            .with_uid(tags::AFFECTED_SOP_CLASS_UID, abstract_syntax);
        let ts = TransferSyntaxRegistry.get(&ts).unwrap();
        self.0
            .request(pc, command, data)
            .await
            .unwrap()
            .into_iter()
            .map(|r| {
                let data = r
                    .data
                    .map(|d| InMemDicomObject::read_dataset_with_ts(&d[..], ts).unwrap());
                (r.command, data)
            })
            .collect()
    }

    async fn store(&mut self, instance: &Instance) -> u16 {
        let file = instance.to_bytes();
        let offset = part10::data_set_offset(&file).unwrap();
        let command = Command::default()
            .with_uid(tags::AFFECTED_SOP_INSTANCE_UID, &instance.sop_uid)
            .with_u16(tags::PRIORITY, 0);
        let responses = self
            .request(
                &instance.sop_class_uid,
                field::C_STORE_RQ,
                command,
                Some(&file[offset..]),
            )
            .await;
        responses[0].0.status().unwrap()
    }

    fn encode(&self, model: &str, identifier: &InMemDicomObject) -> Vec<u8> {
        let (_, ts) = self.context(model);
        let mut data = Vec::new();
        identifier
            .write_dataset_with_ts(&mut data, TransferSyntaxRegistry.get(&ts).unwrap())
            .unwrap();
        data
    }

    async fn find(&mut self, model: &str, identifier: InMemDicomObject) -> Vec<Response> {
        let data = self.encode(model, &identifier);
        let command = Command::default().with_u16(tags::PRIORITY, 0);
        self.request(model, field::C_FIND_RQ, command, Some(&data))
            .await
    }

    async fn r#move(
        &mut self,
        model: &str,
        destination: &str,
        identifier: InMemDicomObject,
    ) -> Vec<Response> {
        let data = self.encode(model, &identifier);
        let command = Command::default()
            .with_u16(tags::PRIORITY, 0)
            .with_text(tags::MOVE_DESTINATION, destination);
        self.request(model, field::C_MOVE_RQ, command, Some(&data))
            .await
    }
}

fn identifier(level: &str, keys: &[(dicom_core::Tag, VR, &str)]) -> InMemDicomObject {
    let mut object = InMemDicomObject::from_element_iter([DataElement::new(
        tags::QUERY_RETRIEVE_LEVEL,
        VR::CS,
        PrimitiveValue::from(level),
    )]);
    for (tag, vr, value) in keys {
        object.put(DataElement::new(*tag, *vr, PrimitiveValue::from(*value)));
    }
    object
}

fn text(object: &InMemDicomObject, tag: dicom_core::Tag) -> String {
    find::string(object, tag).unwrap_or_default()
}

const UNCOMPRESSED: &[&str] = &[EXPLICIT_VR_LE, IMPLICIT_VR_LE];
const ALL_MODELS: &[(&str, &[&str])] = &[
    (STUDY_ROOT_FIND, UNCOMPRESSED),
    (PATIENT_ROOT_FIND, &[IMPLICIT_VR_LE]),
    (STUDY_ROOT_MOVE, UNCOMPRESSED),
    (PATIENT_ROOT_MOVE, UNCOMPRESSED),
];

/// Two studies of one patient, the second with a JPEG series.
fn dataset() -> Vec<Instance> {
    let mut instances = fortitude_testkit::series("1.2.1", "1.2.1.1", 3);
    instances.push(Instance::new("1.2.2", "1.2.2.1", "1.2.2.1.1").with(|i| {
        i.modality = "MR".into();
        i.study_date = "20240301".into();
        i.sop_class_uid = uids::MR_IMAGE_STORAGE.into();
        i.transfer_syntax = JPEG_BASELINE.into();
        i.pixels = Pixels::Jpeg;
    }));
    instances
}

async fn stored(harness: &Harness) -> Vec<Instance> {
    let instances = dataset();
    let mut scu = Client::connect(
        harness.address,
        &[
            (uids::CT_IMAGE_STORAGE, &[EXPLICIT_VR_LE]),
            // The compressed syntax is accepted, wherever it comes in the proposal.
            (uids::MR_IMAGE_STORAGE, &[EXPLICIT_VR_LE, JPEG_BASELINE]),
        ],
    )
    .await;
    assert_eq!(scu.context(uids::MR_IMAGE_STORAGE).1, JPEG_BASELINE);
    for instance in &instances {
        assert_eq!(scu.store(instance).await, status::SUCCESS);
    }
    scu.release().await;
    instances
}

#[tokio::test]
async fn echo() {
    let h = harness(|_| {}).await;
    let mut scu = Client::connect(h.address, &[(uids::VERIFICATION, UNCOMPRESSED)]).await;
    let responses = scu
        .request(
            uids::VERIFICATION,
            field::C_ECHO_RQ,
            Command::default(),
            None,
        )
        .await;
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].0.field(), Some(0x8030));
    assert_eq!(responses[0].0.status(), Some(status::SUCCESS));
    scu.release().await;
}

#[tokio::test]
async fn store_duplicates_and_failures() {
    let h = harness(|_| {}).await;
    let instance = Instance::new("1.2", "1.2.3", "1.2.3.4");
    let mut scu = Client::connect(h.address, &[(uids::CT_IMAGE_STORAGE, UNCOMPRESSED)]).await;
    assert_eq!(scu.store(&instance).await, status::SUCCESS);
    assert_eq!(scu.store(&instance).await, status::SUCCESS);
    let invalid = Instance::new("not a uid", "1.2.3", "1.2.3.5");
    assert_eq!(scu.store(&invalid).await, 0xA900);
    scu.release().await;
}

#[tokio::test]
async fn find_at_every_level() {
    let h = harness(|_| {}).await;
    stored(&h).await;
    let mut scu = Client::connect(h.address, ALL_MODELS).await;

    let responses = scu
        .find(
            STUDY_ROOT_FIND,
            identifier(
                "STUDY",
                &[
                    (tags::PATIENT_NAME, VR::PN, "doe*"),
                    (tags::STUDY_INSTANCE_UID, VR::UI, ""),
                    (tags::MODALITIES_IN_STUDY, VR::CS, ""),
                    (tags::NUMBER_OF_STUDY_RELATED_INSTANCES, VR::IS, ""),
                    (tags::STUDY_DESCRIPTION, VR::LO, ""),
                    (tags::RETRIEVE_AE_TITLE, VR::AE, ""),
                ],
            ),
        )
        .await;
    let (last, matches) = responses.split_last().unwrap();
    assert_eq!(last.0.status(), Some(status::SUCCESS));
    assert_eq!(matches.len(), 2);
    let first = matches[0].1.as_ref().unwrap();
    assert_eq!(text(first, tags::STUDY_INSTANCE_UID), "1.2.1");
    assert_eq!(text(first, tags::MODALITIES_IN_STUDY), "CT");
    assert_eq!(text(first, tags::NUMBER_OF_STUDY_RELATED_INSTANCES), "3");
    assert_eq!(text(first, tags::RETRIEVE_AE_TITLE), "FORTITUDE");
    assert_eq!(text(first, tags::QUERY_RETRIEVE_LEVEL), "STUDY");
    // Requested but absent: returned empty.
    assert!(first.get(tags::STUDY_DESCRIPTION).is_some());
    assert_eq!(text(first, tags::STUDY_DESCRIPTION), "");
    assert!(first.get(tags::PATIENT_ID).is_none());

    let responses = scu
        .find(
            STUDY_ROOT_FIND,
            identifier(
                "STUDY",
                &[
                    (tags::STUDY_DATE, VR::DA, "20240201-"),
                    (tags::STUDY_INSTANCE_UID, VR::UI, ""),
                ],
            ),
        )
        .await;
    assert_eq!(responses.len(), 2);
    assert_eq!(
        text(responses[0].1.as_ref().unwrap(), tags::STUDY_INSTANCE_UID),
        "1.2.2"
    );

    let responses = scu
        .find(
            PATIENT_ROOT_FIND,
            identifier(
                "PATIENT",
                &[
                    (tags::PATIENT_ID, VR::LO, "PAT-1"),
                    (tags::PATIENT_NAME, VR::PN, ""),
                    (tags::NUMBER_OF_PATIENT_RELATED_STUDIES, VR::IS, ""),
                    (tags::NUMBER_OF_PATIENT_RELATED_INSTANCES, VR::IS, ""),
                ],
            ),
        )
        .await;
    assert_eq!(responses.len(), 2);
    let patient = responses[0].1.as_ref().unwrap();
    assert_eq!(text(patient, tags::PATIENT_NAME), "Doe^Jane");
    assert_eq!(text(patient, tags::NUMBER_OF_PATIENT_RELATED_STUDIES), "2");
    assert_eq!(
        text(patient, tags::NUMBER_OF_PATIENT_RELATED_INSTANCES),
        "4"
    );

    let responses = scu
        .find(
            STUDY_ROOT_FIND,
            identifier("PATIENT", &[(tags::PATIENT_ID, VR::LO, "PAT-1")]),
        )
        .await;
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].0.status(), Some(status::IDENTIFIER_MISMATCH));

    let responses = scu
        .find(
            STUDY_ROOT_FIND,
            identifier(
                "SERIES",
                &[
                    (tags::STUDY_INSTANCE_UID, VR::UI, "1.2.1"),
                    (tags::SERIES_INSTANCE_UID, VR::UI, ""),
                    (tags::NUMBER_OF_SERIES_RELATED_INSTANCES, VR::IS, ""),
                ],
            ),
        )
        .await;
    assert_eq!(responses.len(), 2);
    let series = responses[0].1.as_ref().unwrap();
    assert_eq!(text(series, tags::SERIES_INSTANCE_UID), "1.2.1.1");
    assert_eq!(text(series, tags::NUMBER_OF_SERIES_RELATED_INSTANCES), "3");

    let responses = scu
        .find(
            STUDY_ROOT_FIND,
            identifier(
                "IMAGE",
                &[
                    (tags::SERIES_INSTANCE_UID, VR::UI, "1.2.1.1"),
                    (tags::SOP_INSTANCE_UID, VR::UI, ""),
                    (tags::ROWS, VR::US, ""),
                ],
            ),
        )
        .await;
    assert_eq!(responses.len(), 4);
    let image = responses[0].1.as_ref().unwrap();
    assert_eq!(text(image, tags::SOP_INSTANCE_UID), "1.2.1.1.1");
    assert_eq!(text(image, tags::ROWS), "4");
    scu.release().await;
}

#[tokio::test]
async fn find_limit() {
    let h = harness(|c| c.max_find_results = 1).await;
    stored(&h).await;
    let mut scu = Client::connect(h.address, ALL_MODELS).await;
    let responses = scu
        .find(
            STUDY_ROOT_FIND,
            identifier("STUDY", &[(tags::STUDY_INSTANCE_UID, VR::UI, "")]),
        )
        .await;
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[1].0.status(), Some(status::OUT_OF_RESOURCES));
}

/// A storage SCP recording the instances it receives: SOP Instance UID, transfer syntax
/// and data set bytes.
type Received = Arc<Mutex<Vec<(String, String, Vec<u8>)>>>;

async fn destination(transfer_syntaxes: &'static [&'static str]) -> (u16, Received) {
    responding(transfer_syntaxes, |_| status::SUCCESS).await
}

/// [`destination`], answering each instance with the status `respond` gives for its SOP
/// Instance UID.
async fn responding(
    transfer_syntaxes: &'static [&'static str],
    respond: fn(&str) -> u16,
) -> (u16, Received) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let received = Received::default();
    let recorded = received.clone();
    let acceptor =
        Acceptor::new(Arc::new(Config::default())).with_transfer_syntaxes(transfer_syntaxes);
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let mut connection = acceptor.establish(socket, false).await.unwrap();
            while let Ok(Incoming::Message(message)) =
                connection.receive(|_, _, _| Vec::new()).await
            {
                let (_, ts) = connection.context(message.context).unwrap();
                let uid = message
                    .command
                    .string(tags::AFFECTED_SOP_INSTANCE_UID)
                    .unwrap();
                let status = respond(&uid);
                recorded
                    .lock()
                    .unwrap()
                    .push((uid, ts.to_owned(), message.data.unwrap()));
                let response = message.command.response(status, false);
                connection
                    .send(message.context, &response, None)
                    .await
                    .unwrap();
            }
        }
    });
    (port, received)
}

fn peer(ae_title: &str, port: u16) -> Peer {
    Peer {
        ae_title: ae_title.into(),
        host: "127.0.0.1".into(),
        port,
    }
}

#[tokio::test]
async fn move_sends_stored_files() {
    let (port, received) = destination(&[EXPLICIT_VR_LE, IMPLICIT_VR_LE, JPEG_BASELINE]).await;
    let h = harness(|c| c.peers = vec![peer("DEST", port)].into()).await;
    let instances = stored(&h).await;
    let mut scu = Client::connect(h.address, ALL_MODELS).await;

    let responses = scu
        .r#move(
            PATIENT_ROOT_MOVE,
            "DEST",
            identifier("PATIENT", &[(tags::PATIENT_ID, VR::LO, "PAT-1")]),
        )
        .await;
    let (last, pending) = responses.split_last().unwrap();
    assert_eq!(last.0.status(), Some(status::SUCCESS));
    assert_eq!(last.0.u16(tags::NUMBER_OF_COMPLETED_SUBOPERATIONS), Some(4));
    assert_eq!(last.0.u16(tags::NUMBER_OF_FAILED_SUBOPERATIONS), Some(0));
    assert_eq!(pending.len(), 3);
    assert_eq!(
        pending[0].0.u16(tags::NUMBER_OF_REMAINING_SUBOPERATIONS),
        Some(3)
    );

    let received = received.lock().unwrap().clone();
    assert_eq!(received.len(), 4);
    for instance in &instances {
        let (_, ts, data) = received
            .iter()
            .find(|(uid, _, _)| *uid == instance.sop_uid)
            .expect("instance received");
        let file = instance.to_bytes();
        assert_eq!(*ts, instance.transfer_syntax);
        assert_eq!(data[..], file[part10::data_set_offset(&file).unwrap()..]);
    }
}

#[tokio::test]
async fn move_transcodes_when_needed() {
    let (port, received) = destination(&[IMPLICIT_VR_LE]).await;
    let h = harness(|c| c.peers = vec![peer("DEST", port)].into()).await;
    stored(&h).await;
    let mut scu = Client::connect(h.address, ALL_MODELS).await;
    let responses = scu
        .r#move(
            STUDY_ROOT_MOVE,
            "DEST",
            identifier("STUDY", &[(tags::STUDY_INSTANCE_UID, VR::UI, "1.2.2")]),
        )
        .await;
    assert_eq!(responses.last().unwrap().0.status(), Some(status::SUCCESS));
    let received = received.lock().unwrap().clone();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].1, IMPLICIT_VR_LE);
    let object = InMemDicomObject::read_dataset_with_ts(
        &received[0].2[..],
        TransferSyntaxRegistry.get(IMPLICIT_VR_LE).unwrap(),
    )
    .unwrap();
    assert_eq!(text(&object, tags::SOP_INSTANCE_UID), "1.2.2.1.1");
    assert!(object.get(tags::PIXEL_DATA).is_some());
}

#[tokio::test]
async fn move_failures() {
    let h = harness(|c| c.peers = vec![peer("OFFLINE", 1)].into()).await;
    stored(&h).await;
    let mut scu = Client::connect(h.address, ALL_MODELS).await;
    let study = || identifier("STUDY", &[(tags::STUDY_INSTANCE_UID, VR::UI, "1.2.1")]);

    let responses = scu.r#move(STUDY_ROOT_MOVE, "NOWHERE", study()).await;
    assert_eq!(
        responses[0].0.status(),
        Some(status::MOVE_DESTINATION_UNKNOWN)
    );

    let responses = scu.r#move(STUDY_ROOT_MOVE, "OFFLINE", study()).await;
    assert_eq!(responses[0].0.status(), Some(status::SUBOPERATIONS_REFUSED));
    assert_eq!(
        responses[0].0.u16(tags::NUMBER_OF_FAILED_SUBOPERATIONS),
        Some(3)
    );

    let responses = scu
        .r#move(STUDY_ROOT_MOVE, "OFFLINE", identifier("SERIES", &[]))
        .await;
    assert_eq!(responses[0].0.status(), Some(status::IDENTIFIER_MISMATCH));

    // Nothing matches: success without sub-operations.
    let responses = scu
        .r#move(
            STUDY_ROOT_MOVE,
            "OFFLINE",
            identifier("STUDY", &[(tags::STUDY_INSTANCE_UID, VR::UI, "9.9")]),
        )
        .await;
    assert_eq!(responses[0].0.status(), Some(status::SUCCESS));
    assert_eq!(
        responses[0].0.u16(tags::NUMBER_OF_COMPLETED_SUBOPERATIONS),
        Some(0)
    );
}

#[tokio::test]
async fn remote_echo_find_and_move() {
    use crate::remote::{self, Level, RemoteError, key};

    let (port, received) = destination(&[EXPLICIT_VR_LE, IMPLICIT_VR_LE, JPEG_BASELINE]).await;
    let h = harness(|c| c.peers = vec![peer("DEST", port)].into()).await;
    stored(&h).await;
    let config = Config {
        ae_title: "LOCAL".into(),
        timeout: TIMEOUT,
        ..Config::default()
    };
    let remote = peer("FORTITUDE", h.address.port());
    remote::echo(&config, &remote).await.unwrap();
    let offline = remote::echo(&config, &peer("OFFLINE", 1))
        .await
        .unwrap_err();
    assert!(matches!(offline, RemoteError::Unreachable(_)), "{offline}");
    assert!(offline.to_string().contains("127.0.0.1:1"), "{offline}");

    let query = |keys: &[(dicom_core::Tag, &str)]| {
        InMemDicomObject::from_element_iter(
            keys.iter().map(|(tag, value)| key(*tag, value).unwrap()),
        )
    };
    let studies = query(&[
        (tags::PATIENT_ID, "PAT-1"),
        (tags::STUDY_INSTANCE_UID, ""),
        (tags::STUDY_DATE, ""),
        (tags::MODALITIES_IN_STUDY, ""),
    ]);
    let found = remote::find(&config, &remote, Level::Study, studies.clone(), 0)
        .await
        .unwrap();
    assert!(!found.truncated);
    let mut uids: Vec<_> = found
        .matches
        .iter()
        .map(|m| text(m, tags::STUDY_INSTANCE_UID))
        .collect();
    uids.sort();
    assert_eq!(uids, ["1.2.1", "1.2.2"]);
    assert!(found.matches[0].get(tags::QUERY_RETRIEVE_LEVEL).is_none());
    // Beyond the limit, the query is cancelled.
    let found = remote::find(&config, &remote, Level::Study, studies, 1)
        .await
        .unwrap();
    assert_eq!((found.matches.len(), found.truncated), (1, true));
    let series = query(&[
        (tags::STUDY_INSTANCE_UID, "1.2.2"),
        (tags::SERIES_INSTANCE_UID, ""),
        (tags::MODALITY, ""),
    ]);
    let found = remote::find(&config, &remote, Level::Series, series, 0)
        .await
        .unwrap();
    assert_eq!(found.matches.len(), 1);
    assert_eq!(text(&found.matches[0], tags::MODALITY), "MR");
    let patients = query(&[(tags::PATIENT_ID, ""), (tags::PATIENT_NAME, "")]);
    let found = remote::find(&config, &remote, Level::Patient, patients, 0)
        .await
        .unwrap();
    assert_eq!(found.matches.len(), 1);

    // Retrieval, with progress.
    let study = || query(&[(tags::STUDY_INSTANCE_UID, "1.2.1")]);
    let mut updates = Vec::new();
    let moved = remote::retrieve(&config, &remote, Level::Study, study(), "DEST", |p| {
        updates.push(*p);
        false
    })
    .await
    .unwrap();
    assert_eq!(moved.status, status::SUCCESS);
    assert_eq!(
        (
            moved.progress.completed,
            moved.progress.failed,
            moved.progress.remaining
        ),
        (3, 0, None)
    );
    assert_eq!(updates.len(), 2);
    assert_eq!((updates[0].remaining, updates[0].completed), (Some(2), 1));
    assert_eq!(received.lock().unwrap().len(), 3);

    // Cancelled after the first instance.
    let moved = remote::retrieve(&config, &remote, Level::Study, study(), "DEST", |_| true)
        .await
        .unwrap();
    assert_eq!(moved.status, status::CANCEL);
    assert!(moved.progress.completed < 3, "{moved:?}");

    let unknown = remote::retrieve(&config, &remote, Level::Study, study(), "NOWHERE", |_| {
        false
    })
    .await
    .unwrap_err();
    assert!(
        matches!(
            unknown,
            RemoteError::Failed {
                status: status::MOVE_DESTINATION_UNKNOWN,
                ..
            }
        ),
        "{unknown}"
    );
}

#[tokio::test]
async fn remote_send() {
    use crate::remote::{self, RemoteError};
    use fortitude_store::retrieve::Selection;

    let h = harness(|_| {}).await;
    let instances = stored(&h).await;
    let selected = h
        .store
        .select(Selection {
            patient_id: Some("PAT-1".into()),
            ..Selection::default()
        })
        .await
        .unwrap();
    assert_eq!(selected.len(), 4);
    let config = Config {
        timeout: TIMEOUT,
        ..Config::default()
    };

    // Files go out as stored, with progress before the first instance and after each one.
    let (port, received) = destination(&[EXPLICIT_VR_LE, IMPLICIT_VR_LE, JPEG_BASELINE]).await;
    let mut updates = Vec::new();
    let sent = remote::send(
        &config,
        &peer("DEST", port),
        &h.store,
        selected.clone(),
        |p| {
            updates.push(*p);
            false
        },
    )
    .await
    .unwrap();
    assert!(!sent.cancelled);
    assert!(sent.failures.is_empty(), "{:?}", sent.failures);
    assert_eq!(
        (
            sent.progress.completed,
            sent.progress.failed,
            sent.progress.remaining
        ),
        (4, 0, None)
    );
    assert_eq!(updates.len(), 5);
    assert_eq!((updates[0].remaining, updates[0].completed), (Some(4), 0));
    assert_eq!((updates[4].remaining, updates[4].completed), (Some(0), 4));
    let received = received.lock().unwrap().clone();
    assert_eq!(received.len(), 4);
    for instance in &instances {
        let (_, ts, data) = received
            .iter()
            .find(|(uid, _, _)| *uid == instance.sop_uid)
            .expect("instance received");
        let file = instance.to_bytes();
        assert_eq!(*ts, instance.transfer_syntax);
        assert_eq!(data[..], file[part10::data_set_offset(&file).unwrap()..]);
    }

    // Transcoded when the peer accepts no compressed syntax; refusals are listed.
    let (port, received) = responding(&[IMPLICIT_VR_LE], |uid| {
        if uid.ends_with(".2") {
            0xA700
        } else {
            status::SUCCESS
        }
    })
    .await;
    let sent = remote::send(
        &config,
        &peer("DEST", port),
        &h.store,
        selected.clone(),
        |_| false,
    )
    .await
    .unwrap();
    assert_eq!((sent.progress.completed, sent.progress.failed), (3, 1));
    assert_eq!(sent.failures.len(), 1);
    assert!(sent.failures[0].sop_instance_uid.ends_with(".2"));
    assert_eq!(sent.failures[0].reason, "out of resources (A700)");
    assert!(
        received
            .lock()
            .unwrap()
            .iter()
            .all(|(_, ts, _)| ts == IMPLICIT_VR_LE)
    );

    // Into another Fortitude, cancelled after the first instance.
    let other = harness(|_| {}).await;
    let mut calls = 0;
    let sent = remote::send(
        &config,
        &peer("FORTITUDE", other.address.port()),
        &h.store,
        selected.clone(),
        |_| {
            calls += 1;
            calls > 1
        },
    )
    .await
    .unwrap();
    assert!(sent.cancelled);
    assert_eq!(
        (sent.progress.completed, sent.progress.remaining),
        (1, Some(3))
    );
    assert_eq!(stored_count(&other).await, 1);

    let offline = remote::send(&config, &peer("OFFLINE", 1), &h.store, selected, |_| false)
        .await
        .unwrap_err();
    assert!(matches!(offline, RemoteError::Unreachable(_)), "{offline}");
}

async fn stored_count(harness: &Harness) -> usize {
    harness
        .store
        .select(fortitude_store::retrieve::Selection {
            patient_id: Some("PAT-1".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .len()
}

#[tokio::test]
async fn access_control() {
    let h = harness(|c| {
        c.allow_unknown_peers = false;
        c.check_called_ae_title = true;
        c.peers = vec![peer("KNOWN", 104)].into();
    })
    .await;
    let echo: &[(&str, &[&str])] = &[(uids::VERIFICATION, UNCOMPRESSED)];
    assert!(
        Client::try_connect(h.address, "STRANGER", echo)
            .await
            .is_err()
    );
    let scu = Client::try_connect(h.address, "KNOWN", echo).await.unwrap();
    scu.release().await;
    // Unsupported abstract syntaxes are refused, the association still succeeds.
    let scu = Client::try_connect(
        h.address,
        "KNOWN",
        &[
            (uids::VERIFICATION, UNCOMPRESSED),
            (uids::MODALITY_WORKLIST_INFORMATION_MODEL_FIND, UNCOMPRESSED),
        ],
    )
    .await
    .unwrap();
    assert!(scu.0.context(uids::VERIFICATION, None).is_some());
    assert!(
        scu.0
            .context(uids::MODALITY_WORKLIST_INFORMATION_MODEL_FIND, None)
            .is_none()
    );
}

#[tokio::test]
async fn association_limit() {
    let h = harness(|c| c.max_associations = 1).await;
    let echo: &[(&str, &[&str])] = &[(uids::VERIFICATION, UNCOMPRESSED)];
    let first = Client::connect(h.address, echo).await;
    assert!(Client::try_connect(h.address, "SCU", echo).await.is_err());
    first.release().await;
    // The slot is released with the association.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let again = Client::connect(h.address, echo).await;
    again.release().await;
}

/// Instances received by a C-GET SCU: SOP Instance UID, transfer syntax and data set bytes.
type Retrieved = Vec<(String, String, Vec<u8>)>;

/// Associate as a C-GET SCU: both GET models, and `storage` contexts (SOP class, transfer
/// syntaxes) proposing the SCP role.
async fn get_client(address: SocketAddr, storage: &[(&str, &[&str])]) -> Client {
    let mut contexts: Vec<(String, Vec<String>)> = [PATIENT_ROOT_GET, STUDY_ROOT_GET]
        .iter()
        .map(|a| (a.to_string(), vec![EXPLICIT_VR_LE.to_owned()]))
        .collect();
    let mut scp_role = Vec::new();
    for (sop_class, transfer_syntaxes) in storage {
        contexts.push((
            sop_class.to_string(),
            transfer_syntaxes.iter().map(|t| t.to_string()).collect(),
        ));
        if !scp_role.iter().any(|c| c == sop_class) {
            scp_role.push(sop_class.to_string());
        }
    }
    let scu =
        scu::Scu::connect_with_scp_role(address, "SCU", "FORTITUDE", &contexts, &scp_role, TIMEOUT)
            .await
            .unwrap();
    Client(scu)
}

impl Client {
    async fn get(
        &mut self,
        model: &str,
        identifier: InMemDicomObject,
    ) -> (Vec<Response>, Retrieved) {
        let data = self.encode(model, &identifier);
        let (pc, ts) = self.context(model);
        let command = Command::default()
            .with_u16(tags::COMMAND_FIELD, field::C_GET_RQ)
            .with_uid(tags::AFFECTED_SOP_CLASS_UID, model)
            .with_u16(tags::PRIORITY, 0);
        let mut retrieved = Retrieved::new();
        let responses = self
            .0
            .request_with(pc, command, Some(&data), |request, ts, data| {
                assert!(
                    request
                        .string(tags::MOVE_ORIGINATOR_APPLICATION_ENTITY_TITLE)
                        .is_none()
                );
                retrieved.push((
                    request.string(tags::AFFECTED_SOP_INSTANCE_UID).unwrap(),
                    ts.to_owned(),
                    data,
                ));
                status::SUCCESS
            })
            .await
            .unwrap();
        let ts = TransferSyntaxRegistry.get(&ts).unwrap();
        let responses = responses
            .into_iter()
            .map(|r| {
                let data = r
                    .data
                    .map(|d| InMemDicomObject::read_dataset_with_ts(&d[..], ts).unwrap());
                (r.command, data)
            })
            .collect();
        (responses, retrieved)
    }
}

#[tokio::test]
async fn get_sends_over_the_same_association() {
    let h = harness(|_| {}).await;
    let instances = stored(&h).await;
    let mut scu = get_client(
        h.address,
        &[
            (uids::CT_IMAGE_STORAGE, UNCOMPRESSED),
            (uids::MR_IMAGE_STORAGE, &[JPEG_BASELINE]),
            (uids::MR_IMAGE_STORAGE, UNCOMPRESSED),
        ],
    )
    .await;
    assert!(scu.0.scp_role(uids::CT_IMAGE_STORAGE));
    // The model itself has no SCP role to give.
    assert!(!scu.0.scp_role(STUDY_ROOT_GET));

    let (responses, retrieved) = scu
        .get(
            PATIENT_ROOT_GET,
            identifier("PATIENT", &[(tags::PATIENT_ID, VR::LO, "PAT-1")]),
        )
        .await;
    let (last, pending) = responses.split_last().unwrap();
    assert_eq!(last.0.field(), Some(field::C_GET_RQ | field::RESPONSE));
    assert_eq!(last.0.status(), Some(status::SUCCESS));
    assert_eq!(last.0.u16(tags::NUMBER_OF_COMPLETED_SUBOPERATIONS), Some(4));
    assert_eq!(last.0.u16(tags::NUMBER_OF_FAILED_SUBOPERATIONS), Some(0));
    assert_eq!(pending.len(), 3);
    assert_eq!(pending[0].0.status(), Some(status::PENDING));
    assert_eq!(
        pending[0].0.u16(tags::NUMBER_OF_REMAINING_SUBOPERATIONS),
        Some(3)
    );

    assert_eq!(retrieved.len(), 4);
    for instance in &instances {
        let (_, ts, data) = retrieved
            .iter()
            .find(|(uid, _, _)| *uid == instance.sop_uid)
            .expect("instance retrieved");
        let file = instance.to_bytes();
        assert_eq!(*ts, instance.transfer_syntax);
        assert_eq!(data[..], file[part10::data_set_offset(&file).unwrap()..]);
    }

    // The association remains usable.
    let (responses, retrieved) = scu
        .get(
            STUDY_ROOT_GET,
            identifier("SERIES", &[(tags::SERIES_INSTANCE_UID, VR::UI, "1.2.1.1")]),
        )
        .await;
    assert_eq!(responses.last().unwrap().0.status(), Some(status::SUCCESS));
    assert_eq!(retrieved.len(), 3);
    scu.release().await;
}

#[tokio::test]
async fn get_transcodes_when_needed() {
    let h = harness(|_| {}).await;
    stored(&h).await;
    // One context with both syntaxes: the native one is chosen, any instance can be sent.
    let mut scu = get_client(
        h.address,
        &[(uids::MR_IMAGE_STORAGE, &[JPEG_BASELINE, IMPLICIT_VR_LE])],
    )
    .await;
    assert_eq!(scu.context(uids::MR_IMAGE_STORAGE).1, IMPLICIT_VR_LE);
    let (responses, retrieved) = scu
        .get(
            STUDY_ROOT_GET,
            identifier("STUDY", &[(tags::STUDY_INSTANCE_UID, VR::UI, "1.2.2")]),
        )
        .await;
    assert_eq!(responses.last().unwrap().0.status(), Some(status::SUCCESS));
    assert_eq!(retrieved.len(), 1);
    assert_eq!(retrieved[0].1, IMPLICIT_VR_LE);
    let object = InMemDicomObject::read_dataset_with_ts(
        &retrieved[0].2[..],
        TransferSyntaxRegistry.get(IMPLICIT_VR_LE).unwrap(),
    )
    .unwrap();
    assert_eq!(text(&object, tags::SOP_INSTANCE_UID), "1.2.2.1.1");
    assert!(object.get(tags::PIXEL_DATA).is_some());
    scu.release().await;
}

#[tokio::test]
async fn get_failures() {
    let h = harness(|_| {}).await;
    stored(&h).await;
    // MR is proposed without the SCP role: its instances cannot be sent.
    let contexts: Vec<(String, Vec<String>)> = [
        (STUDY_ROOT_GET, EXPLICIT_VR_LE),
        (uids::CT_IMAGE_STORAGE, EXPLICIT_VR_LE),
        (uids::MR_IMAGE_STORAGE, EXPLICIT_VR_LE),
    ]
    .iter()
    .map(|(a, t)| (a.to_string(), vec![t.to_string()]))
    .collect();
    let mut scu = Client(
        scu::Scu::connect_with_scp_role(
            h.address,
            "SCU",
            "FORTITUDE",
            &contexts,
            &[uids::CT_IMAGE_STORAGE.to_owned()],
            TIMEOUT,
        )
        .await
        .unwrap(),
    );
    assert!(!scu.0.scp_role(uids::MR_IMAGE_STORAGE));

    let (responses, retrieved) = scu
        .get(
            STUDY_ROOT_GET,
            identifier(
                "STUDY",
                &[(tags::STUDY_INSTANCE_UID, VR::UI, "1.2.1\\1.2.2")],
            ),
        )
        .await;
    let (last, _) = responses.split_last().unwrap();
    assert_eq!(last.0.status(), Some(status::SUBOPERATIONS_WARNING));
    assert_eq!(last.0.u16(tags::NUMBER_OF_COMPLETED_SUBOPERATIONS), Some(3));
    assert_eq!(last.0.u16(tags::NUMBER_OF_FAILED_SUBOPERATIONS), Some(1));
    let failed = last.1.as_ref().expect("Failed SOP Instance UID List");
    assert_eq!(
        text(failed, tags::FAILED_SOP_INSTANCE_UID_LIST),
        "1.2.2.1.1"
    );
    assert_eq!(retrieved.len(), 3);

    let (responses, _) = scu.get(STUDY_ROOT_GET, identifier("SERIES", &[])).await;
    assert_eq!(responses[0].0.status(), Some(status::IDENTIFIER_MISMATCH));

    // Nothing matches: success without sub-operations.
    let (responses, retrieved) = scu
        .get(
            STUDY_ROOT_GET,
            identifier("STUDY", &[(tags::STUDY_INSTANCE_UID, VR::UI, "9.9")]),
        )
        .await;
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].0.status(), Some(status::SUCCESS));
    assert!(retrieved.is_empty());
    scu.release().await;
}
