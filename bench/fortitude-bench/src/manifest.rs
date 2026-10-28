use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub const MANIFEST_FILE: &str = "manifest.jsonl";

/// One generated instance, as recorded in `manifest.jsonl` next to the files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// Path relative to the dataset directory.
    pub path: PathBuf,
    pub size: u64,
    pub patient_id: String,
    pub patient_name: String,
    pub study_uid: String,
    pub study_date: String,
    pub series_uid: String,
    pub sop_uid: String,
    pub modality: String,
    pub frames: u32,
}

pub fn write(dataset: &Path, entries: &[Entry]) -> anyhow::Result<()> {
    let path = dataset.join(MANIFEST_FILE);
    let mut out = BufWriter::new(File::create(&path).with_context(|| path.display().to_string())?);
    for entry in entries {
        serde_json::to_writer(&mut out, entry)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

pub fn read(dataset: &Path) -> anyhow::Result<Vec<Entry>> {
    let path = dataset.join(MANIFEST_FILE);
    let file = File::open(&path).with_context(|| format!("cannot open {}", path.display()))?;
    BufReader::new(file)
        .lines()
        .map(|line| Ok(serde_json::from_str(&line?)?))
        .collect()
}
