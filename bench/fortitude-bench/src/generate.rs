use std::path::{Path, PathBuf};

use anyhow::Context;
use dicom_core::value::PrimitiveValue;
use dicom_core::{DataElement, Tag, VR};
use dicom_dictionary_std::tags;
use dicom_object::{DefaultDicomObject, open_file};
use dicom_transfer_syntax_registry::entries::EXPLICIT_VR_LITTLE_ENDIAN;
use rayon::prelude::*;

use crate::manifest::{self, Entry};
use crate::rng::hash;

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Directory scanned recursively for seed DICOM files (loaded in memory once);
    /// repeat the option to combine several directories.
    #[arg(long, required = true)]
    seed: Vec<PathBuf>,
    /// Output dataset directory; receives the files and `manifest.jsonl`.
    #[arg(long)]
    out: PathBuf,
    /// Number of copies of the seed set, each with distinct patients and UIDs.
    #[arg(long, default_value_t = 10)]
    copies: u32,
    /// Replace pixel data with an 8x8 uncompressed image, to reach index-scale
    /// datasets (millions of instances) with little disk usage.
    #[arg(long)]
    tiny: bool,
}

/// Top-level UIDs remapped in each copy. Nested references (SR, SEG, RTSTRUCT) are left
/// untouched; they are irrelevant to the benchmarks.
const REMAPPED_UIDS: [Tag; 4] = [
    tags::STUDY_INSTANCE_UID,
    tags::SERIES_INSTANCE_UID,
    tags::SOP_INSTANCE_UID,
    tags::FRAME_OF_REFERENCE_UID,
];

const FAMILY_NAMES: [&str; 24] = [
    "TREMBLAY",
    "GAGNON",
    "ROY",
    "COTE",
    "BOUCHARD",
    "GAUTHIER",
    "MORIN",
    "LAVOIE",
    "FORTIN",
    "GAGNE",
    "OUELLET",
    "PELLETIER",
    "BELANGER",
    "LEVESQUE",
    "BERGERON",
    "LEBLANC",
    "PAQUETTE",
    "GIRARD",
    "SIMARD",
    "BOUCHER",
    "CARON",
    "BEAULIEU",
    "CLOUTIER",
    "DUBE",
];
const GIVEN_NAMES: [&str; 16] = [
    "MARIE", "JEAN", "LOUISE", "PIERRE", "SOPHIE", "MICHEL", "JULIE", "ANDRE", "NATHALIE",
    "FRANCOIS", "ISABELLE", "LUC", "CHANTAL", "DENIS", "MANON", "GABRIEL",
];

pub fn run(args: &Args) -> anyhow::Result<()> {
    let seeds = load_seeds(&args.seed)?;
    anyhow::ensure!(
        !seeds.is_empty(),
        "no DICOM file found in the seed directories"
    );
    eprintln!("loaded {} seed instances", seeds.len());

    for copy in 0..args.copies {
        std::fs::create_dir_all(copy_dir(&args.out, copy))?;
    }
    let entries = (0..args.copies)
        .into_par_iter()
        .flat_map_iter(|copy| seeds.iter().map(move |seed| (copy, seed)))
        .map(|(copy, seed)| make_copy(seed.clone(), copy, args.tiny, &args.out))
        .collect::<anyhow::Result<Vec<_>>>()?;

    manifest::write(&args.out, &entries)?;
    let bytes: u64 = entries.iter().map(|e| e.size).sum();
    eprintln!(
        "wrote {} instances ({:.1} MiB) to {}",
        entries.len(),
        bytes as f64 / (1024.0 * 1024.0),
        args.out.display()
    );
    Ok(())
}

fn copy_dir(root: &Path, copy: u32) -> PathBuf {
    root.join(format!("{copy:05}"))
}

fn load_seeds(dirs: &[PathBuf]) -> anyhow::Result<Vec<DefaultDicomObject>> {
    let paths: Vec<PathBuf> = dirs
        .iter()
        .flat_map(walkdir::WalkDir::new)
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .collect();
    Ok(paths
        .par_iter()
        .filter_map(|path| match open_file(path) {
            Ok(obj)
                if obj
                    .element_opt(tags::SOP_INSTANCE_UID)
                    .ok()
                    .flatten()
                    .is_some() =>
            {
                Some(obj)
            }
            Ok(_) => None,
            Err(err) => {
                eprintln!("skipping {}: {err}", path.display());
                None
            }
        })
        .collect())
}

fn make_copy(
    mut obj: DefaultDicomObject,
    copy: u32,
    tiny: bool,
    root: &Path,
) -> anyhow::Result<Entry> {
    let copy_key = copy.to_le_bytes();
    let derive = |kind: &str, value: &str| hash(0, &[kind.as_bytes(), value.as_bytes(), &copy_key]);

    for tag in REMAPPED_UIDS {
        if let Some(original) = string(&obj, tag) {
            let uid = format!(
                "2.25.{}",
                uid_number(derive("uid", &original), derive("uid2", &original))
            );
            obj.put_str(tag, VR::UI, uid);
        }
    }
    let sop_uid = string(&obj, tags::SOP_INSTANCE_UID).context("missing SOPInstanceUID")?;
    let new_sop = sop_uid.clone();
    obj.update_meta(|meta| meta.media_storage_sop_instance_uid = new_sop);

    let patient = derive(
        "patient",
        &string(&obj, tags::PATIENT_ID).unwrap_or_default(),
    );
    let patient_id = format!("FT{:010}", patient % 10_000_000_000);
    let patient_name = format!(
        "{}^{}",
        FAMILY_NAMES[(patient % FAMILY_NAMES.len() as u64) as usize],
        GIVEN_NAMES[((patient >> 16) % GIVEN_NAMES.len() as u64) as usize]
    );
    obj.put_str(tags::PATIENT_ID, VR::LO, patient_id.clone());
    obj.put_str(tags::PATIENT_NAME, VR::PN, patient_name.clone());

    // StudyInstanceUID is already remapped, so it is unique per copy.
    let study_uid = string(&obj, tags::STUDY_INSTANCE_UID).context("missing StudyInstanceUID")?;
    let study = hash(1, &[study_uid.as_bytes()]);
    let study_date = format!(
        "{}{:02}{:02}",
        2000 + study % 26,
        1 + (study >> 8) % 12,
        1 + (study >> 16) % 28
    );
    obj.put_str(tags::STUDY_DATE, VR::DA, study_date.clone());
    obj.put_str(
        tags::ACCESSION_NUMBER,
        VR::SH,
        format!("A{:09}", study % 1_000_000_000),
    );

    if tiny {
        shrink_pixel_data(&mut obj);
    }

    let path = copy_dir(root, copy).join(format!("{sop_uid}.dcm"));
    obj.write_to_file(&path)
        .with_context(|| format!("cannot write {}", path.display()))?;

    Ok(Entry {
        size: std::fs::metadata(&path)?.len(),
        path: path.strip_prefix(root)?.to_path_buf(),
        patient_id,
        patient_name,
        series_uid: string(&obj, tags::SERIES_INSTANCE_UID).context("missing SeriesInstanceUID")?,
        study_uid,
        study_date,
        sop_uid,
        modality: string(&obj, tags::MODALITY).unwrap_or_default(),
        frames: string(&obj, tags::NUMBER_OF_FRAMES)
            .and_then(|f| f.parse().ok())
            .unwrap_or(1),
    })
}

/// Decimal number for a `2.25.` UID: 128 bits from two hashes, without leading zero.
fn uid_number(high: u64, low: u64) -> u128 {
    ((u128::from(high) << 64) | u128::from(low)).max(1)
}

fn string(obj: &DefaultDicomObject, tag: Tag) -> Option<String> {
    let value = obj.element_opt(tag).ok().flatten()?.to_str().ok()?;
    let value = value.trim_end_matches(['\0', ' ']);
    (!value.is_empty()).then(|| value.to_owned())
}

fn shrink_pixel_data(obj: &mut DefaultDicomObject) {
    for tag in [
        tags::NUMBER_OF_FRAMES,
        tags::PLANAR_CONFIGURATION,
        tags::PIXEL_DATA,
    ] {
        obj.remove_element(tag);
    }
    let us = |tag, value: u16| DataElement::new(tag, VR::US, PrimitiveValue::from(value));
    obj.put(us(tags::ROWS, 8));
    obj.put(us(tags::COLUMNS, 8));
    obj.put(us(tags::SAMPLES_PER_PIXEL, 1));
    obj.put(us(tags::BITS_ALLOCATED, 16));
    obj.put(us(tags::BITS_STORED, 16));
    obj.put(us(tags::HIGH_BIT, 15));
    obj.put(us(tags::PIXEL_REPRESENTATION, 0));
    obj.put_str(tags::PHOTOMETRIC_INTERPRETATION, VR::CS, "MONOCHROME2");
    obj.put(DataElement::new(
        tags::PIXEL_DATA,
        VR::OW,
        PrimitiveValue::U16(vec![0u16; 64].into()),
    ));
    obj.update_meta(|meta| meta.set_transfer_syntax(&EXPLICIT_VR_LITTLE_ENDIAN));
}
