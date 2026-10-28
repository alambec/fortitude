//! Domain model shared by every Fortitude crate: identifiers, the set of indexed
//! attributes, the QIDO query model with DICOM matching rules, and the DICOM JSON encoder.

pub mod attrs;
pub mod json;
pub mod qido;
pub mod uid;

pub use dicom_core::Tag;
