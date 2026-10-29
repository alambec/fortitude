//! End-to-end tests: a DIMSE server on an empty store, driven by a `dicom-ul` SCU, with a
//! storage SCP of the same kind as C-MOVE destination.

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
    _dir: tempfile::TempDir,
}

async fn harness(configure: impl FnOnce(&mut Config)) -> Harness {
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
    let mut config = Config::default();
    configure(&mut config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let dimse = Dimse {
        store,
        config: Arc::new(config),
    };
    tokio::spawn(serve(listener, dimse, std::future::pending()));
    Harness { address, _dir: dir }
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
                recorded.lock().unwrap().push((
                    message
                        .command
                        .string(tags::AFFECTED_SOP_INSTANCE_UID)
                        .unwrap(),
                    ts.to_owned(),
                    message.data.unwrap(),
                ));
                let response = message.command.response(status::SUCCESS, false);
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
    let h = harness(|c| c.peers = vec![peer("DEST", port)]).await;
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
    let h = harness(|c| c.peers = vec![peer("DEST", port)]).await;
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
    let h = harness(|c| c.peers = vec![peer("OFFLINE", 1)]).await;
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
async fn access_control() {
    let h = harness(|c| {
        c.allow_unknown_peers = false;
        c.check_called_ae_title = true;
        c.peers = vec![peer("KNOWN", 104)];
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
            (
                uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_GET,
                UNCOMPRESSED,
            ),
        ],
    )
    .await
    .unwrap();
    assert!(scu.0.context(uids::VERIFICATION, None).is_some());
    assert!(
        scu.0
            .context(uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_GET, None)
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
