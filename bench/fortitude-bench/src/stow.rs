use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use futures::{StreamExt, stream};
use serde::Serialize;

use crate::http::{self, Target};
use crate::manifest::{self, Entry};
use crate::report::{self, Latency};

const BOUNDARY: &str = "fortitude-bench-7d1f0c2a9e";

#[derive(Debug, clap::Args)]
pub struct Args {
    #[command(flatten)]
    target: Target,
    /// Dataset produced by `generate`.
    #[arg(long)]
    dataset: PathBuf,
    /// Concurrent STOW-RS requests.
    #[arg(long, default_value_t = 8)]
    concurrency: usize,
    /// Instances per request; a request never mixes studies.
    #[arg(long, default_value_t = 20)]
    batch: usize,
    /// Upload only the first N instances of the manifest.
    #[arg(long)]
    limit: Option<usize>,
    /// Name of the run in the report (e.g. "orthanc-pg").
    #[arg(long, default_value = "unnamed")]
    label: String,
    /// JSONL file the result is appended to.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct StowResult<'a> {
    kind: &'static str,
    label: &'a str,
    url: &'a str,
    concurrency: usize,
    batch: usize,
    instances: usize,
    failed_instances: usize,
    requests: usize,
    error_statuses: BTreeMap<u16, usize>,
    wall_s: f64,
    instances_per_s: f64,
    mib_per_s: f64,
    #[serde(flatten)]
    request_latency: Latency,
}

struct Outcome {
    instances: usize,
    bytes: u64,
    elapsed: Duration,
    status: u16,
    failed: usize,
}

pub async fn run(args: Args) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.batch > 0 && args.concurrency > 0,
        "batch and concurrency must be positive"
    );
    let mut entries = manifest::read(&args.dataset)?;
    if let Some(limit) = args.limit {
        entries.truncate(limit);
    }
    let batches = batches_by_study(&entries, args.batch);
    let client = http::client(args.concurrency)?;
    let url = format!("{}/studies", args.target.base());

    let start = Instant::now();
    let outcomes: Vec<anyhow::Result<Outcome>> = stream::iter(batches)
        .map(|batch| send(&client, &args.target, &url, &args.dataset, batch))
        .buffer_unordered(args.concurrency)
        .collect()
        .await;
    let wall = start.elapsed();

    let mut histogram = report::histogram();
    let (mut instances, mut failed, mut bytes) = (0, 0, 0);
    let mut error_statuses = BTreeMap::new();
    let requests = outcomes.len();
    for outcome in outcomes {
        let outcome = outcome?;
        report::record(&mut histogram, outcome.elapsed);
        instances += outcome.instances;
        failed += outcome.failed;
        bytes += outcome.bytes;
        if !(200..300).contains(&outcome.status) {
            *error_statuses.entry(outcome.status).or_insert(0) += 1;
        }
    }

    let secs = wall.as_secs_f64();
    report::emit(
        &StowResult {
            kind: "stow",
            label: &args.label,
            url: &url,
            concurrency: args.concurrency,
            batch: args.batch,
            instances,
            failed_instances: failed,
            requests,
            error_statuses,
            wall_s: secs,
            instances_per_s: instances as f64 / secs,
            mib_per_s: bytes as f64 / (1024.0 * 1024.0) / secs,
            request_latency: Latency::from(&histogram),
        },
        args.out.as_deref(),
    )
}

/// Split the manifest into requests of at most `size` instances of the same study,
/// keeping the manifest order of studies.
fn batches_by_study(entries: &[Entry], size: usize) -> Vec<Vec<&Entry>> {
    let mut order: Vec<&str> = Vec::new();
    let mut by_study: HashMap<&str, Vec<&Entry>> = HashMap::new();
    for entry in entries {
        by_study
            .entry(&entry.study_uid)
            .or_insert_with(|| {
                order.push(&entry.study_uid);
                Vec::new()
            })
            .push(entry);
    }
    order
        .into_iter()
        .flat_map(|study| {
            let instances = by_study.remove(study).unwrap_or_default();
            instances
                .chunks(size)
                .map(<[&Entry]>::to_vec)
                .collect::<Vec<_>>()
        })
        .collect()
}

async fn send(
    client: &reqwest::Client,
    target: &Target,
    url: &str,
    dataset: &Path,
    batch: Vec<&Entry>,
) -> anyhow::Result<Outcome> {
    let mut body = Vec::with_capacity(batch.iter().map(|e| e.size as usize + 96).sum());
    for entry in &batch {
        let path = dataset.join(&entry.path);
        let file = tokio::fs::read(&path)
            .await
            .with_context(|| format!("cannot read {}", path.display()))?;
        body.extend_from_slice(
            format!("--{BOUNDARY}\r\nContent-Type: application/dicom\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(&file);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    let bytes = body.len() as u64;

    let request = target
        .authorize(client.post(url))
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("multipart/related; type=\"application/dicom\"; boundary={BOUNDARY}"),
        )
        .header(reqwest::header::ACCEPT, "application/dicom+json")
        .body(body);

    let start = Instant::now();
    let response = request.send().await?;
    let status = response.status().as_u16();
    let payload = response.bytes().await?;
    let elapsed = start.elapsed();

    let failed = match failed_sop_count(&payload) {
        Some(count) => count,
        None if (200..300).contains(&status) => 0,
        None => batch.len(),
    };
    Ok(Outcome {
        instances: batch.len(),
        bytes,
        elapsed,
        status,
        failed,
    })
}

/// Number of items in FailedSOPSequence (0008,1198) of a STOW-RS response, if it parses.
fn failed_sop_count(payload: &[u8]) -> Option<usize> {
    let response: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let failed = response.get("00081198").and_then(|seq| seq.get("Value"));
    Some(
        failed
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_failed_sop_sequence_items() {
        let ok = br#"{"00081199":{"vr":"SQ","Value":[{},{}]}}"#;
        let partial =
            br#"{"00081198":{"vr":"SQ","Value":[{}]},"00081199":{"vr":"SQ","Value":[{}]}}"#;
        assert_eq!(failed_sop_count(ok), Some(0));
        assert_eq!(failed_sop_count(partial), Some(1));
        assert_eq!(failed_sop_count(b"<html>"), None);
    }
}
