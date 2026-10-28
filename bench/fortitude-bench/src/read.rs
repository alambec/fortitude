use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use hdrhistogram::Histogram;
use serde::Serialize;

use crate::http::{self, Target};
use crate::manifest::{self, Entry};
use crate::report::{self, Latency};
use crate::rng::SplitMix;

const DICOM_JSON: &str = "application/dicom+json";

#[derive(Debug, Clone, Copy, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scenario {
    /// QIDO-RS studies matching one PatientID.
    QidoPatient,
    /// QIDO-RS studies matching a PatientName prefix wildcard (`TRE*`).
    QidoNameWildcard,
    /// QIDO-RS studies within one year and a modality, limited to 100 results.
    QidoDateModality,
    /// QIDO-RS series of one study.
    QidoSeries,
    /// QIDO-RS instances of one series.
    QidoInstances,
    /// WADO-RS metadata of a whole series.
    SeriesMetadata,
    /// WADO-RS retrieve of one instance (multipart, stored transfer syntax).
    Instance,
    /// WADO-RS retrieve of one frame (multipart, stored transfer syntax).
    Frame,
    /// WADO-URI retrieve of one instance as application/dicom.
    WadoUri,
    /// WADO-RS rendered CT instance as JPEG.
    Rendered,
}

#[derive(Debug, clap::Args)]
pub struct Args {
    #[command(flatten)]
    target: Target,
    /// Dataset previously uploaded with `stow`.
    #[arg(long)]
    dataset: PathBuf,
    #[arg(long, value_enum)]
    scenario: Scenario,
    /// Concurrent clients, each sending requests back to back.
    #[arg(long, default_value_t = 16)]
    concurrency: usize,
    /// Measured duration.
    #[arg(long, default_value = "30s")]
    duration: humantime::Duration,
    /// Unmeasured run before the measured duration.
    #[arg(long, default_value = "5s")]
    warmup: humantime::Duration,
    #[arg(long, default_value = "unnamed")]
    label: String,
    /// JSONL file the result is appended to.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct ReadResult<'a> {
    kind: &'static str,
    label: &'a str,
    scenario: Scenario,
    concurrency: usize,
    duration_s: f64,
    requests: u64,
    errors: u64,
    error_statuses: BTreeMap<u16, u64>,
    requests_per_s: f64,
    mib_per_s: f64,
    #[serde(flatten)]
    latency: Latency,
}

/// Request targets derived from the manifest.
struct Targets {
    entries: Vec<Entry>,
    patients: Vec<String>,
    name_prefixes: Vec<String>,
    modalities: Vec<String>,
    years: Vec<String>,
    studies: Vec<String>,
    series: Vec<usize>,
    /// CT instances: images every server can decode.
    images: Vec<usize>,
}

impl Targets {
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
        let mut seen_series = BTreeSet::new();
        let series = (0..entries.len())
            .filter(|&i| seen_series.insert(entries[i].series_uid.as_str()))
            .collect();
        Ok(Self {
            patients: unique(&|e| e.patient_id.clone()),
            name_prefixes: unique(&|e| e.patient_name.chars().take(3).collect()),
            modalities: unique(&|e| e.modality.clone()),
            years: unique(&|e| e.study_date.chars().take(4).collect()),
            studies: unique(&|e| e.study_uid.clone()),
            series,
            images: (0..entries.len())
                .filter(|&i| entries[i].modality == "CT")
                .collect(),
            entries,
        })
    }

    fn request(
        &self,
        scenario: Scenario,
        target: &Target,
        rng: &mut SplitMix,
    ) -> (String, &'static str) {
        let base = target.base();
        let pick = |rng: &mut SplitMix, list: &[String]| list[rng.index(list.len())].clone();
        match scenario {
            Scenario::QidoPatient => (
                format!("{base}/studies?PatientID={}", pick(rng, &self.patients)),
                DICOM_JSON,
            ),
            Scenario::QidoNameWildcard => (
                format!(
                    "{base}/studies?PatientName={}*",
                    pick(rng, &self.name_prefixes)
                ),
                DICOM_JSON,
            ),
            Scenario::QidoDateModality => {
                let year = pick(rng, &self.years);
                let modality = pick(rng, &self.modalities);
                (
                    format!(
                        "{base}/studies?StudyDate={year}0101-{year}1231&ModalitiesInStudy={modality}&limit=100"
                    ),
                    DICOM_JSON,
                )
            }
            Scenario::QidoSeries => (
                format!("{base}/studies/{}/series", pick(rng, &self.studies)),
                DICOM_JSON,
            ),
            Scenario::QidoInstances | Scenario::SeriesMetadata => {
                let e = &self.entries[self.series[rng.index(self.series.len())]];
                let suffix = match scenario {
                    Scenario::QidoInstances => "instances",
                    _ => "metadata",
                };
                (
                    format!(
                        "{base}/studies/{}/series/{}/{suffix}",
                        e.study_uid, e.series_uid
                    ),
                    DICOM_JSON,
                )
            }
            Scenario::Instance => {
                let e = &self.entries[rng.index(self.entries.len())];
                (
                    format!(
                        "{base}/studies/{}/series/{}/instances/{}",
                        e.study_uid, e.series_uid, e.sop_uid
                    ),
                    "multipart/related; type=\"application/dicom\"; transfer-syntax=*",
                )
            }
            Scenario::Frame => {
                let e = &self.entries[rng.index(self.entries.len())];
                let frame = 1 + rng.index(e.frames.max(1) as usize);
                (
                    format!(
                        "{base}/studies/{}/series/{}/instances/{}/frames/{frame}",
                        e.study_uid, e.series_uid, e.sop_uid
                    ),
                    "multipart/related; type=\"application/octet-stream\"; transfer-syntax=*",
                )
            }
            Scenario::WadoUri => {
                let e = &self.entries[rng.index(self.entries.len())];
                (
                    format!(
                        "{}?requestType=WADO&studyUID={}&seriesUID={}&objectUID={}&contentType=application/dicom",
                        target.wado(),
                        e.study_uid,
                        e.series_uid,
                        e.sop_uid
                    ),
                    "application/dicom",
                )
            }
            Scenario::Rendered => {
                let e = &self.entries[self.images[rng.index(self.images.len())]];
                (
                    format!(
                        "{base}/studies/{}/series/{}/instances/{}/rendered",
                        e.study_uid, e.series_uid, e.sop_uid
                    ),
                    "image/jpeg",
                )
            }
        }
    }
}

#[derive(Debug)]
struct WorkerStats {
    histogram: Histogram<u64>,
    requests: u64,
    bytes: u64,
    error_statuses: BTreeMap<u16, u64>,
}

pub async fn run(args: Args) -> anyhow::Result<()> {
    anyhow::ensure!(args.concurrency > 0, "concurrency must be positive");
    let targets = Arc::new(Targets::new(manifest::read(&args.dataset)?)?);
    let client = http::client(args.concurrency)?;
    let warmup_end = Instant::now() + Duration::from(args.warmup);
    let end = warmup_end + Duration::from(args.duration);

    let workers: Vec<_> = (0..args.concurrency)
        .map(|id| {
            let (client, targets, target) = (client.clone(), targets.clone(), args.target.clone());
            tokio::spawn(worker(
                id as u64,
                client,
                targets,
                target,
                args.scenario,
                warmup_end,
                end,
            ))
        })
        .collect();

    let mut histogram = report::histogram();
    let (mut requests, mut bytes) = (0, 0);
    let mut error_statuses = BTreeMap::new();
    for worker in workers {
        let stats = worker.await?;
        histogram.add(&stats.histogram)?;
        requests += stats.requests;
        bytes += stats.bytes;
        for (status, count) in stats.error_statuses {
            *error_statuses.entry(status).or_insert(0) += count;
        }
    }

    let secs = Duration::from(args.duration).as_secs_f64();
    report::emit(
        &ReadResult {
            kind: "read",
            label: &args.label,
            scenario: args.scenario,
            concurrency: args.concurrency,
            duration_s: secs,
            requests,
            errors: error_statuses.values().sum(),
            error_statuses,
            requests_per_s: requests as f64 / secs,
            mib_per_s: bytes as f64 / (1024.0 * 1024.0) / secs,
            latency: Latency::from(&histogram),
        },
        args.out.as_deref(),
    )
}

async fn worker(
    id: u64,
    client: reqwest::Client,
    targets: Arc<Targets>,
    target: Target,
    scenario: Scenario,
    warmup_end: Instant,
    end: Instant,
) -> WorkerStats {
    let mut rng = SplitMix::new(0x5eed ^ id.wrapping_mul(0x9e37_79b9));
    let mut stats = WorkerStats {
        histogram: report::histogram(),
        requests: 0,
        bytes: 0,
        error_statuses: BTreeMap::new(),
    };
    loop {
        let start = Instant::now();
        if start >= end {
            return stats;
        }
        let (url, accept) = targets.request(scenario, &target, &mut rng);
        let request = target
            .authorize(client.get(url))
            .header(reqwest::header::ACCEPT, accept);
        let outcome = fetch(request).await;
        if start < warmup_end {
            continue;
        }
        match outcome {
            Ok((status, bytes)) if (200..300).contains(&status) => {
                report::record(&mut stats.histogram, start.elapsed());
                stats.requests += 1;
                stats.bytes += bytes;
            }
            // Status 0 stands for transport errors (connection refused, reset, timeout).
            Ok((status, _)) => *stats.error_statuses.entry(status).or_insert(0) += 1,
            Err(_) => *stats.error_statuses.entry(0).or_insert(0) += 1,
        }
    }
}

/// Send the request and drain the body, returning the status and body size.
async fn fetch(request: reqwest::RequestBuilder) -> reqwest::Result<(u16, u64)> {
    let response = request.send().await?;
    let status = response.status().as_u16();
    let mut body = response.bytes_stream();
    let mut bytes = 0;
    while let Some(chunk) = body.next().await {
        bytes += chunk?.len() as u64;
    }
    Ok((status, bytes))
}
