//! DIMSE scenarios: C-STORE of a dataset, C-FIND queries for a fixed duration, and C-MOVE
//! of every study to a sink storage SCP run by the tool itself.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use dicom_core::{DataElement, PrimitiveValue, Tag, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_object::{FileMetaTable, InMemDicomObject};
use fortitude_dimse::association::{Acceptor, Incoming};
use fortitude_dimse::command::{Command, field, status};
use fortitude_dimse::part10;
use fortitude_dimse::scu::Scu;
use hdrhistogram::Histogram;
use serde::Serialize;

use crate::manifest::{self, Entry};
use crate::report::{self, Latency};
use crate::rng::SplitMix;

const TIMEOUT: Duration = Duration::from_secs(120);
const EXPLICIT_VR_LE: &str = "1.2.840.10008.1.2.1";
const IMPLICIT_VR_LE: &str = "1.2.840.10008.1.2";

#[derive(Debug, Clone, clap::Args)]
pub struct Target {
    /// DIMSE address of the server under test.
    #[arg(long)]
    addr: String,
    /// Called AE title.
    #[arg(long, default_value = "FORTITUDE")]
    ae: String,
    /// Calling AE title.
    #[arg(long, default_value = "BENCH")]
    calling_ae: String,
}

impl Target {
    async fn connect(&self, contexts: &[(String, Vec<String>)]) -> anyhow::Result<Scu> {
        Scu::connect(&self.addr, &self.calling_ae, &self.ae, contexts, TIMEOUT)
            .await
            .with_context(|| format!("cannot associate with {}@{}", self.ae, self.addr))
    }
}

#[derive(Debug, clap::Args)]
pub struct StoreArgs {
    #[command(flatten)]
    target: Target,
    /// Dataset produced by `generate`.
    #[arg(long)]
    dataset: PathBuf,
    /// Concurrent associations, each sending its share of the instances back to back.
    #[arg(long, default_value_t = 8)]
    concurrency: usize,
    /// Skip the first N instances of the manifest.
    #[arg(long, default_value_t = 0)]
    skip: usize,
    /// Send at most N instances.
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long, default_value = "unnamed")]
    label: String,
    /// JSONL file the result is appended to.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct StoreResult<'a> {
    kind: &'static str,
    label: &'a str,
    concurrency: usize,
    instances: usize,
    failed_instances: usize,
    statuses: BTreeMap<String, usize>,
    wall_s: f64,
    instances_per_s: f64,
    mib_per_s: f64,
    #[serde(flatten)]
    request_latency: Latency,
}

/// A file to send: its path and the identification read from its meta group.
#[derive(Debug, Clone)]
struct File {
    path: PathBuf,
    sop_class_uid: String,
    sop_instance_uid: String,
    transfer_syntax: String,
}

fn scan(dataset: &Path, entries: &[Entry]) -> anyhow::Result<Vec<File>> {
    entries
        .iter()
        .map(|entry| {
            let path = dataset.join(&entry.path);
            let mut reader = std::io::BufReader::new(std::fs::File::open(&path)?);
            let mut preamble = [0; 128];
            std::io::Read::read_exact(&mut reader, &mut preamble)?;
            let meta = FileMetaTable::from_reader(reader)
                .with_context(|| format!("cannot read {}", path.display()))?;
            let trim = |s: &str| s.trim_end_matches(['\0', ' ']).to_owned();
            Ok(File {
                sop_class_uid: trim(meta.media_storage_sop_class_uid()),
                sop_instance_uid: trim(meta.media_storage_sop_instance_uid()),
                transfer_syntax: trim(meta.transfer_syntax()),
                path,
            })
        })
        .collect()
}

pub async fn store(args: StoreArgs) -> anyhow::Result<()> {
    anyhow::ensure!(args.concurrency > 0, "concurrency must be positive");
    let entries = manifest::read(&args.dataset)?;
    let entries: Vec<Entry> = entries
        .into_iter()
        .skip(args.skip)
        .take(args.limit.unwrap_or(usize::MAX))
        .collect();
    let files = scan(&args.dataset, &entries)?;
    // One presentation context per (SOP class, transfer syntax): files are sent as they are.
    let contexts: Vec<(String, Vec<String>)> = files
        .iter()
        .map(|f| (f.sop_class_uid.clone(), f.transfer_syntax.clone()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|(class, ts)| (class, vec![ts]))
        .collect();
    anyhow::ensure!(contexts.len() <= 128, "more than 128 presentation contexts");
    let files = Arc::new(files);
    let contexts = Arc::new(contexts);

    let start = Instant::now();
    let workers: Vec<_> = (0..args.concurrency)
        .map(|id| {
            let (target, files, contexts) = (args.target.clone(), files.clone(), contexts.clone());
            let concurrency = args.concurrency;
            tokio::spawn(async move {
                let mut scu = target.connect(&contexts).await?;
                let mut histogram = report::histogram();
                let mut statuses: BTreeMap<String, usize> = BTreeMap::new();
                let mut bytes = 0;
                for file in files.iter().skip(id).step_by(concurrency) {
                    let content = tokio::fs::read(&file.path).await?;
                    let offset = part10::data_set_offset(&content).context("not a Part-10 file")?;
                    let (pc, _) = scu
                        .context(&file.sop_class_uid, Some(&file.transfer_syntax))
                        .context("presentation context refused")?;
                    let command = Command::default()
                        .with_u16(tags::COMMAND_FIELD, field::C_STORE_RQ)
                        .with_u16(tags::PRIORITY, 0)
                        .with_uid(tags::AFFECTED_SOP_CLASS_UID, &file.sop_class_uid)
                        .with_uid(tags::AFFECTED_SOP_INSTANCE_UID, &file.sop_instance_uid);
                    let sent = Instant::now();
                    let responses = scu.request(pc, command, Some(&content[offset..])).await?;
                    report::record(&mut histogram, sent.elapsed());
                    let status = responses[0].command.status().unwrap_or(0xFFFF);
                    *statuses.entry(format!("{status:04X}")).or_default() += 1;
                    bytes += content.len() as u64;
                }
                scu.release().await?;
                anyhow::Ok((histogram, statuses, bytes))
            })
        })
        .collect();

    let mut histogram = report::histogram();
    let mut statuses: BTreeMap<String, usize> = BTreeMap::new();
    let mut bytes = 0;
    for worker in workers {
        let (h, s, b) = worker.await??;
        histogram.add(&h)?;
        for (status, count) in s {
            *statuses.entry(status).or_default() += count;
        }
        bytes += b;
    }
    let wall = start.elapsed().as_secs_f64();
    let instances: usize = statuses.values().sum();
    report::emit(
        &StoreResult {
            kind: "dimse-store",
            label: &args.label,
            concurrency: args.concurrency,
            instances,
            failed_instances: instances - statuses.get("0000").copied().unwrap_or(0),
            statuses,
            wall_s: wall,
            instances_per_s: instances as f64 / wall,
            mib_per_s: bytes as f64 / (1024.0 * 1024.0) / wall,
            request_latency: Latency::from(&histogram),
        },
        args.out.as_deref(),
    )
}

#[derive(Debug, Clone, Copy, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FindScenario {
    /// Studies of one PatientID.
    Patient,
    /// Studies matching a PatientName prefix wildcard (`TRE*`).
    NameWildcard,
    /// Studies within one year with one modality.
    DateModality,
    /// Series of one study.
    Series,
    /// Instances of one series.
    Instances,
}

#[derive(Debug, clap::Args)]
pub struct FindArgs {
    #[command(flatten)]
    target: Target,
    /// Dataset previously stored.
    #[arg(long)]
    dataset: PathBuf,
    #[arg(long, value_enum)]
    scenario: FindScenario,
    /// Concurrent associations, each sending queries back to back.
    #[arg(long, default_value_t = 16)]
    concurrency: usize,
    #[arg(long, default_value = "30s")]
    duration: humantime::Duration,
    #[arg(long, default_value = "5s")]
    warmup: humantime::Duration,
    #[arg(long, default_value = "unnamed")]
    label: String,
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct FindResult<'a> {
    kind: &'static str,
    label: &'a str,
    scenario: FindScenario,
    concurrency: usize,
    duration_s: f64,
    requests: u64,
    errors: u64,
    matches: u64,
    requests_per_s: f64,
    #[serde(flatten)]
    latency: Latency,
}

/// Query/Retrieve Level, return keys, and matching keys with their values.
type Query = (&'static str, &'static [(Tag, VR)], Vec<(Tag, String)>);

/// Query targets derived from the manifest.
struct Queries {
    entries: Vec<Entry>,
    patients: Vec<String>,
    name_prefixes: Vec<String>,
    modalities: Vec<String>,
    years: Vec<String>,
    studies: Vec<String>,
    series: Vec<usize>,
}

impl Queries {
    fn new(entries: Vec<Entry>) -> anyhow::Result<Self> {
        anyhow::ensure!(!entries.is_empty(), "empty manifest");
        let unique = |f: &dyn Fn(&Entry) -> String| -> Vec<String> {
            entries
                .iter()
                .map(f)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        };
        let mut seen = BTreeSet::new();
        let series = (0..entries.len())
            .filter(|&i| seen.insert(entries[i].series_uid.as_str()))
            .collect();
        Ok(Self {
            patients: unique(&|e| e.patient_id.clone()),
            name_prefixes: unique(&|e| e.patient_name.chars().take(3).collect()),
            modalities: unique(&|e| e.modality.clone()),
            years: unique(&|e| e.study_date.chars().take(4).collect()),
            studies: unique(&|e| e.study_uid.clone()),
            series,
            entries,
        })
    }

    /// The identifier of one query: matching keys with values, return keys empty.
    fn identifier(&self, scenario: FindScenario, rng: &mut SplitMix) -> InMemDicomObject {
        let pick = |rng: &mut SplitMix, list: &[String]| list[rng.index(list.len())].clone();
        let study_keys: &[(Tag, VR)] = &[
            (tags::STUDY_INSTANCE_UID, VR::UI),
            (tags::PATIENT_NAME, VR::PN),
            (tags::PATIENT_ID, VR::LO),
            (tags::STUDY_DATE, VR::DA),
            (tags::ACCESSION_NUMBER, VR::SH),
            (tags::STUDY_DESCRIPTION, VR::LO),
            (tags::MODALITIES_IN_STUDY, VR::CS),
            (tags::NUMBER_OF_STUDY_RELATED_INSTANCES, VR::IS),
        ];
        let (level, keys, values): Query = match scenario {
            FindScenario::Patient => (
                "STUDY",
                study_keys,
                vec![(tags::PATIENT_ID, pick(rng, &self.patients))],
            ),
            FindScenario::NameWildcard => (
                "STUDY",
                study_keys,
                vec![(
                    tags::PATIENT_NAME,
                    format!("{}*", pick(rng, &self.name_prefixes)),
                )],
            ),
            FindScenario::DateModality => {
                let year = pick(rng, &self.years);
                (
                    "STUDY",
                    study_keys,
                    vec![
                        (tags::STUDY_DATE, format!("{year}0101-{year}1231")),
                        (tags::MODALITIES_IN_STUDY, pick(rng, &self.modalities)),
                    ],
                )
            }
            FindScenario::Series => (
                "SERIES",
                &[
                    (tags::STUDY_INSTANCE_UID, VR::UI),
                    (tags::SERIES_INSTANCE_UID, VR::UI),
                    (tags::MODALITY, VR::CS),
                    (tags::SERIES_NUMBER, VR::IS),
                    (tags::SERIES_DESCRIPTION, VR::LO),
                    (tags::NUMBER_OF_SERIES_RELATED_INSTANCES, VR::IS),
                ],
                vec![(tags::STUDY_INSTANCE_UID, pick(rng, &self.studies))],
            ),
            FindScenario::Instances => {
                let e = &self.entries[self.series[rng.index(self.series.len())]];
                (
                    "IMAGE",
                    &[
                        (tags::STUDY_INSTANCE_UID, VR::UI),
                        (tags::SERIES_INSTANCE_UID, VR::UI),
                        (tags::SOP_INSTANCE_UID, VR::UI),
                        (tags::SOP_CLASS_UID, VR::UI),
                        (tags::INSTANCE_NUMBER, VR::IS),
                        (tags::ROWS, VR::US),
                        (tags::COLUMNS, VR::US),
                    ],
                    vec![
                        (tags::STUDY_INSTANCE_UID, e.study_uid.clone()),
                        (tags::SERIES_INSTANCE_UID, e.series_uid.clone()),
                    ],
                )
            }
        };
        let mut object = InMemDicomObject::from_element_iter([DataElement::new(
            tags::QUERY_RETRIEVE_LEVEL,
            VR::CS,
            PrimitiveValue::from(level),
        )]);
        for (tag, vr) in keys {
            let value = values
                .iter()
                .find(|(t, _)| t == tag)
                .map(|(_, v)| PrimitiveValue::from(v.as_str()))
                .unwrap_or(PrimitiveValue::Empty);
            object.put(DataElement::new(*tag, *vr, value));
        }
        object
    }
}

fn query_contexts() -> Vec<(String, Vec<String>)> {
    vec![
        (
            uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND.to_owned(),
            vec![EXPLICIT_VR_LE.to_owned(), IMPLICIT_VR_LE.to_owned()],
        ),
        (
            uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE.to_owned(),
            vec![EXPLICIT_VR_LE.to_owned(), IMPLICIT_VR_LE.to_owned()],
        ),
    ]
}

fn encode(object: &InMemDicomObject, transfer_syntax: &str) -> anyhow::Result<Vec<u8>> {
    use dicom_encoding::TransferSyntaxIndex;
    let ts = dicom_transfer_syntax_registry::TransferSyntaxRegistry
        .get(transfer_syntax)
        .context("unknown transfer syntax")?;
    let mut out = Vec::new();
    object.write_dataset_with_ts(&mut out, ts)?;
    Ok(out)
}

struct FindStats {
    histogram: Histogram<u64>,
    requests: u64,
    errors: u64,
    matches: u64,
}

pub async fn find(args: FindArgs) -> anyhow::Result<()> {
    anyhow::ensure!(args.concurrency > 0, "concurrency must be positive");
    let queries = Arc::new(Queries::new(manifest::read(&args.dataset)?)?);
    let warmup_end = Instant::now() + Duration::from(args.warmup);
    let end = warmup_end + Duration::from(args.duration);
    let workers: Vec<_> = (0..args.concurrency)
        .map(|id| {
            let (target, queries, scenario) = (args.target.clone(), queries.clone(), args.scenario);
            tokio::spawn(async move {
                let mut scu = target.connect(&query_contexts()).await?;
                let model = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND;
                let (pc, ts) = scu.context(model, None).context("C-FIND refused")?;
                let ts = ts.to_owned();
                let mut rng = SplitMix::new(0x5eed ^ (id as u64).wrapping_mul(0x9e37_79b9));
                let mut stats = FindStats {
                    histogram: report::histogram(),
                    requests: 0,
                    errors: 0,
                    matches: 0,
                };
                loop {
                    let start = Instant::now();
                    if start >= end {
                        break;
                    }
                    let identifier = encode(&queries.identifier(scenario, &mut rng), &ts)?;
                    let command = Command::default()
                        .with_u16(tags::COMMAND_FIELD, field::C_FIND_RQ)
                        .with_u16(tags::PRIORITY, 0)
                        .with_uid(tags::AFFECTED_SOP_CLASS_UID, model);
                    let responses = scu.request(pc, command, Some(&identifier)).await?;
                    if start < warmup_end {
                        continue;
                    }
                    let last = responses.last().and_then(|r| r.command.status());
                    if last == Some(status::SUCCESS) {
                        report::record(&mut stats.histogram, start.elapsed());
                        stats.requests += 1;
                        stats.matches += responses.len() as u64 - 1;
                    } else {
                        stats.errors += 1;
                    }
                }
                scu.release().await?;
                anyhow::Ok(stats)
            })
        })
        .collect();

    let mut histogram = report::histogram();
    let (mut requests, mut errors, mut matches) = (0, 0, 0);
    for worker in workers {
        let stats = worker.await??;
        histogram.add(&stats.histogram)?;
        requests += stats.requests;
        errors += stats.errors;
        matches += stats.matches;
    }
    let secs = Duration::from(args.duration).as_secs_f64();
    report::emit(
        &FindResult {
            kind: "dimse-find",
            label: &args.label,
            scenario: args.scenario,
            concurrency: args.concurrency,
            duration_s: secs,
            requests,
            errors,
            matches,
            requests_per_s: requests as f64 / secs,
            latency: Latency::from(&histogram),
        },
        args.out.as_deref(),
    )
}

#[derive(Debug, clap::Args)]
pub struct MoveArgs {
    #[command(flatten)]
    target: Target,
    /// Dataset previously stored.
    #[arg(long)]
    dataset: PathBuf,
    /// Address the sink storage SCP listens on.
    #[arg(long, default_value = "0.0.0.0:11120")]
    sink: SocketAddr,
    /// AE title of the sink, known to the server as a move destination.
    #[arg(long, default_value = "SINK")]
    sink_ae: String,
    /// Concurrent associations, each moving its share of the studies one after the other.
    #[arg(long, default_value_t = 4)]
    concurrency: usize,
    #[arg(long, default_value = "unnamed")]
    label: String,
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct MoveResult<'a> {
    kind: &'static str,
    label: &'a str,
    concurrency: usize,
    studies: usize,
    instances: u64,
    failed_instances: u64,
    received: u64,
    wall_s: f64,
    instances_per_s: f64,
    mib_per_s: f64,
    #[serde(flatten)]
    study_latency: Latency,
}

/// Instances and bytes received by the sink.
#[derive(Default)]
struct Sink {
    instances: AtomicU64,
    bytes: AtomicU64,
}

async fn run_sink(listener: tokio::net::TcpListener, sink: Arc<Sink>) {
    let acceptor = Arc::new(Acceptor::new(Arc::new(fortitude_dimse::Config {
        timeout: TIMEOUT,
        ..fortitude_dimse::Config::default()
    })));
    loop {
        let Ok((socket, _)) = listener.accept().await else {
            continue;
        };
        let _ = socket.set_nodelay(true);
        let (acceptor, sink) = (acceptor.clone(), sink.clone());
        tokio::spawn(async move {
            let Ok(mut connection) = acceptor.establish(socket, false).await else {
                return;
            };
            while let Ok(Incoming::Message(message)) =
                connection.receive(|_, _, _| Vec::new()).await
            {
                if let Some(data) = &message.data {
                    sink.instances.fetch_add(1, Ordering::Relaxed);
                    sink.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                }
                let response = message.command.response(status::SUCCESS, false);
                if connection
                    .send(message.context, &response, None)
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }
}

pub async fn r#move(args: MoveArgs) -> anyhow::Result<()> {
    anyhow::ensure!(args.concurrency > 0, "concurrency must be positive");
    let entries = manifest::read(&args.dataset)?;
    let studies: Vec<String> = entries
        .iter()
        .map(|e| e.study_uid.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let studies = Arc::new(studies);
    let sink = Arc::new(Sink::default());
    let listener = tokio::net::TcpListener::bind(args.sink)
        .await
        .with_context(|| format!("cannot listen on {}", args.sink))?;
    tokio::spawn(run_sink(listener, sink.clone()));

    let start = Instant::now();
    let workers: Vec<_> = (0..args.concurrency)
        .map(|id| {
            let (target, studies) = (args.target.clone(), studies.clone());
            let (concurrency, sink_ae) = (args.concurrency, args.sink_ae.clone());
            tokio::spawn(async move {
                let mut scu = target.connect(&query_contexts()).await?;
                let model = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE;
                let (pc, ts) = scu.context(model, None).context("C-MOVE refused")?;
                let ts = ts.to_owned();
                let mut histogram = report::histogram();
                let (mut completed, mut failed) = (0u64, 0u64);
                for study in studies.iter().skip(id).step_by(concurrency) {
                    let identifier = InMemDicomObject::from_element_iter([
                        DataElement::new(
                            tags::QUERY_RETRIEVE_LEVEL,
                            VR::CS,
                            PrimitiveValue::from("STUDY"),
                        ),
                        DataElement::new(
                            tags::STUDY_INSTANCE_UID,
                            VR::UI,
                            PrimitiveValue::from(study.as_str()),
                        ),
                    ]);
                    let command = Command::default()
                        .with_u16(tags::COMMAND_FIELD, field::C_MOVE_RQ)
                        .with_u16(tags::PRIORITY, 0)
                        .with_uid(tags::AFFECTED_SOP_CLASS_UID, model)
                        .with_text(tags::MOVE_DESTINATION, &sink_ae);
                    let sent = Instant::now();
                    let responses = scu
                        .request(pc, command, Some(&encode(&identifier, &ts)?))
                        .await?;
                    report::record(&mut histogram, sent.elapsed());
                    let last = &responses.last().context("no response")?.command;
                    completed += u64::from(
                        last.u16(tags::NUMBER_OF_COMPLETED_SUBOPERATIONS)
                            .unwrap_or(0),
                    );
                    failed +=
                        u64::from(last.u16(tags::NUMBER_OF_FAILED_SUBOPERATIONS).unwrap_or(0));
                }
                scu.release().await?;
                anyhow::Ok((histogram, completed, failed))
            })
        })
        .collect();

    let mut histogram = report::histogram();
    let (mut completed, mut failed) = (0, 0);
    for worker in workers {
        let (h, c, f) = worker.await??;
        histogram.add(&h)?;
        completed += c;
        failed += f;
    }
    let wall = start.elapsed().as_secs_f64();
    let bytes = sink.bytes.load(Ordering::Relaxed);
    report::emit(
        &MoveResult {
            kind: "dimse-move",
            label: &args.label,
            concurrency: args.concurrency,
            studies: studies.len(),
            instances: completed,
            failed_instances: failed,
            received: sink.instances.load(Ordering::Relaxed),
            wall_s: wall,
            instances_per_s: completed as f64 / wall,
            mib_per_s: bytes as f64 / (1024.0 * 1024.0) / wall,
            study_latency: Latency::from(&histogram),
        },
        args.out.as_deref(),
    )
}
