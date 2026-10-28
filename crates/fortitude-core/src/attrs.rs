//! Attributes known to the index.
//!
//! [`INDEXED`] drives the database schema, the values extracted at ingest and the QIDO-RS
//! matching: adding an attribute there is enough to make it a fast matching key.
//! The `*_RETURN` lists are the attributes returned by QIDO-RS by default for each level
//! (PS3.18 Table 6.7.1-2 to 6.7.1-2b); they are pre-encoded at ingest.

use dicom_core::Tag;
use dicom_dictionary_std::tags;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Level {
    Study,
    Series,
    Instance,
}

impl Level {
    pub fn table(self) -> &'static str {
        match self {
            Level::Study => "studies",
            Level::Series => "series",
            Level::Instance => "instances",
        }
    }
}

/// How values of an indexed attribute are stored and matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Case-sensitive string, with wildcard matching.
    Text,
    /// Person name: also stored upper-cased in `<column>_key` for case-insensitive matching.
    PersonName,
    /// UID: single value or list matching only.
    Uid,
    /// DA, matched as a range.
    Date,
    /// TM, matched as a range.
    Time,
    /// IS, stored as an integer.
    Integer,
}

#[derive(Debug, Clone, Copy)]
pub struct IndexedAttribute {
    pub tag: Tag,
    pub keyword: &'static str,
    pub level: Level,
    pub column: &'static str,
    pub kind: Kind,
}

const fn attr(
    tag: Tag,
    keyword: &'static str,
    level: Level,
    column: &'static str,
    kind: Kind,
) -> IndexedAttribute {
    IndexedAttribute {
        tag,
        keyword,
        level,
        column,
        kind,
    }
}

use Kind::*;
use Level::*;

#[rustfmt::skip]
pub const INDEXED: &[IndexedAttribute] = &[
    attr(tags::STUDY_INSTANCE_UID, "StudyInstanceUID", Study, "study_uid", Uid),
    attr(tags::STUDY_DATE, "StudyDate", Study, "study_date", Date),
    attr(tags::STUDY_TIME, "StudyTime", Study, "study_time", Time),
    attr(tags::ACCESSION_NUMBER, "AccessionNumber", Study, "accession_number", Text),
    attr(tags::STUDY_ID, "StudyID", Study, "study_id", Text),
    attr(tags::STUDY_DESCRIPTION, "StudyDescription", Study, "study_description", Text),
    attr(tags::REFERRING_PHYSICIAN_NAME, "ReferringPhysicianName", Study, "referring_physician_name", PersonName),
    attr(tags::PATIENT_NAME, "PatientName", Study, "patient_name", PersonName),
    attr(tags::PATIENT_ID, "PatientID", Study, "patient_id", Text),
    attr(tags::ISSUER_OF_PATIENT_ID, "IssuerOfPatientID", Study, "issuer_of_patient_id", Text),
    attr(tags::PATIENT_BIRTH_DATE, "PatientBirthDate", Study, "patient_birth_date", Date),
    attr(tags::PATIENT_SEX, "PatientSex", Study, "patient_sex", Text),
    attr(tags::SERIES_INSTANCE_UID, "SeriesInstanceUID", Series, "series_uid", Uid),
    attr(tags::MODALITY, "Modality", Series, "modality", Text),
    attr(tags::SERIES_NUMBER, "SeriesNumber", Series, "series_number", Integer),
    attr(tags::SERIES_DESCRIPTION, "SeriesDescription", Series, "series_description", Text),
    attr(tags::SERIES_DATE, "SeriesDate", Series, "series_date", Date),
    attr(tags::SERIES_TIME, "SeriesTime", Series, "series_time", Time),
    attr(tags::BODY_PART_EXAMINED, "BodyPartExamined", Series, "body_part_examined", Text),
    attr(tags::PERFORMED_PROCEDURE_STEP_START_DATE, "PerformedProcedureStepStartDate", Series, "pps_start_date", Date),
    attr(tags::PERFORMED_PROCEDURE_STEP_START_TIME, "PerformedProcedureStepStartTime", Series, "pps_start_time", Time),
    attr(tags::SOP_INSTANCE_UID, "SOPInstanceUID", Instance, "sop_uid", Uid),
    attr(tags::SOP_CLASS_UID, "SOPClassUID", Instance, "sop_class_uid", Uid),
    attr(tags::INSTANCE_NUMBER, "InstanceNumber", Instance, "instance_number", Integer),
];

pub fn indexed(level: Level) -> impl Iterator<Item = &'static IndexedAttribute> {
    INDEXED.iter().filter(move |a| a.level == level)
}

pub fn find_indexed(tag: Tag) -> Option<&'static IndexedAttribute> {
    INDEXED.iter().find(|a| a.tag == tag)
}

pub const STUDY_RETURN: &[Tag] = &[
    tags::SPECIFIC_CHARACTER_SET,
    tags::STUDY_DATE,
    tags::STUDY_TIME,
    tags::ACCESSION_NUMBER,
    tags::REFERRING_PHYSICIAN_NAME,
    tags::TIMEZONE_OFFSET_FROM_UTC,
    tags::STUDY_DESCRIPTION,
    tags::PATIENT_NAME,
    tags::PATIENT_ID,
    tags::ISSUER_OF_PATIENT_ID,
    tags::PATIENT_BIRTH_DATE,
    tags::PATIENT_SEX,
    tags::STUDY_INSTANCE_UID,
    tags::STUDY_ID,
];

pub const SERIES_RETURN: &[Tag] = &[
    tags::SPECIFIC_CHARACTER_SET,
    tags::SERIES_DATE,
    tags::SERIES_TIME,
    tags::MODALITY,
    tags::TIMEZONE_OFFSET_FROM_UTC,
    tags::SERIES_DESCRIPTION,
    tags::PERFORMED_PROCEDURE_STEP_START_DATE,
    tags::PERFORMED_PROCEDURE_STEP_START_TIME,
    tags::REQUEST_ATTRIBUTES_SEQUENCE,
    tags::BODY_PART_EXAMINED,
    tags::SERIES_INSTANCE_UID,
    tags::SERIES_NUMBER,
];

pub const INSTANCE_RETURN: &[Tag] = &[
    tags::SPECIFIC_CHARACTER_SET,
    tags::SOP_CLASS_UID,
    tags::SOP_INSTANCE_UID,
    tags::TIMEZONE_OFFSET_FROM_UTC,
    tags::INSTANCE_NUMBER,
    tags::ROWS,
    tags::COLUMNS,
    tags::BITS_ALLOCATED,
    tags::NUMBER_OF_FRAMES,
];

pub fn default_return(level: Level) -> &'static [Tag] {
    match level {
        Level::Study => STUDY_RETURN,
        Level::Series => SERIES_RETURN,
        Level::Instance => INSTANCE_RETURN,
    }
}
