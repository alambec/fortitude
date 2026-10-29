//! Read side of the store: locating instances, metadata, frames and bulk data.

use std::path::PathBuf;
use std::sync::Arc;

use dicom_core::Tag;
use dicom_core::value::Value as DicomValue;
use dicom_dictionary_std::tags;
use dicom_object::{InMemDicomObject, OpenFileOptions};
use fortitude_core::json;

use crate::Store;
use crate::index::{Connection, IndexError, Row, Value};
use crate::pixel::{PixelLayout, Range};
use crate::walker::PIXEL_TAGS;

/// A study, a series of a study, or an instance of a series.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub study: String,
    pub series: Option<String>,
    pub instance: Option<String>,
}

impl Scope {
    pub fn study(study: &str) -> Self {
        Self {
            study: study.into(),
            series: None,
            instance: None,
        }
    }

    pub fn series(study: &str, series: &str) -> Self {
        Self {
            study: study.into(),
            series: Some(series.into()),
            instance: None,
        }
    }

    pub fn instance(study: &str, series: &str, instance: &str) -> Self {
        Self {
            study: study.into(),
            series: Some(series.into()),
            instance: Some(instance.into()),
        }
    }

    /// `FROM … WHERE …` clause selecting the instances of the scope, with its parameters.
    /// Instances are ordered by series number and instance number.
    pub(crate) fn select(&self, columns: &str) -> (String, Vec<Value>) {
        let from = "FROM instances i JOIN series s ON s.pk = i.series_pk JOIN studies st ON st.pk = i.study_pk";
        let order = "ORDER BY s.series_number, s.pk, i.instance_number, i.pk";
        match (&self.series, &self.instance) {
            (Some(series), Some(instance)) => (
                format!(
                    "SELECT {columns} {from} WHERE i.sop_uid = ?1 AND s.series_uid = ?2 AND st.study_uid = ?3"
                ),
                vec![
                    Value::Text(instance.clone()),
                    Value::Text(series.clone()),
                    Value::Text(self.study.clone()),
                ],
            ),
            (Some(series), None) => (
                format!(
                    "SELECT {columns} {from} WHERE s.series_uid = ?1 AND st.study_uid = ?2 {order}"
                ),
                vec![Value::Text(series.clone()), Value::Text(self.study.clone())],
            ),
            _ => (
                format!("SELECT {columns} {from} WHERE st.study_uid = ?1 {order}"),
                vec![Value::Text(self.study.clone())],
            ),
        }
    }
}

/// A stored instance file.
#[derive(Debug, Clone)]
pub struct StoredInstance {
    pub study_uid: String,
    pub series_uid: String,
    pub sop_uid: String,
    pub sop_class_uid: String,
    pub transfer_syntax: String,
    pub path: PathBuf,
    pub size: u64,
}

/// Instances selected by identifiers (C-MOVE): each criterion that is given must match.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub patient_id: Option<String>,
    pub studies: Vec<String>,
    pub series: Vec<String>,
    pub instances: Vec<String>,
}

impl Selection {
    pub fn is_empty(&self) -> bool {
        self.patient_id.is_none()
            && self.studies.is_empty()
            && self.series.is_empty()
            && self.instances.is_empty()
    }
}

/// Where the pixel data of an instance lives.
#[derive(Debug, Clone)]
pub struct PixelSource {
    pub path: PathBuf,
    pub transfer_syntax: String,
    pub layout: Option<PixelLayout>,
}

/// Bulk data value: byte ranges of the stored file, or bytes decoded from it.
#[derive(Debug)]
pub enum BulkData {
    /// Pixel data: frames of the stored file.
    Pixels(PixelSource),
    Bytes(Vec<u8>),
}

impl Store {
    /// Instance files of a scope, in series and instance order.
    pub async fn instances(
        self: &Arc<Self>,
        scope: Scope,
    ) -> Result<Vec<StoredInstance>, IndexError> {
        let root = self.files().root().to_owned();
        self.index()
            .readers()
            .run(move |conn| {
                query(conn, &scope, STORED_COLUMNS, |row| {
                    stored_instance(row, &root)
                })
            })
            .await
    }

    /// Instances of a selection, in study, series and instance order; none for an empty
    /// selection.
    pub async fn select(
        self: &Arc<Self>,
        selection: Selection,
    ) -> Result<Vec<StoredInstance>, IndexError> {
        if selection.is_empty() {
            return Ok(Vec::new());
        }
        let root = self.files().root().to_owned();
        self.index()
            .readers()
            .run(move |conn| {
                let mut conditions = Vec::new();
                let mut params = Vec::new();
                if let Some(patient_id) = &selection.patient_id {
                    conditions.push("st.patient_id = ?".to_owned());
                    params.push(Value::Text(patient_id.clone()));
                }
                for (column, values) in [
                    ("st.study_uid", &selection.studies),
                    ("s.series_uid", &selection.series),
                    ("i.sop_uid", &selection.instances),
                ] {
                    if !values.is_empty() {
                        let placeholders = vec!["?"; values.len()].join(", ");
                        conditions.push(format!("{column} IN ({placeholders})"));
                        params.extend(values.iter().cloned().map(Value::Text));
                    }
                }
                let sql = format!(
                    "SELECT {STORED_COLUMNS} FROM instances i JOIN series s ON s.pk = i.series_pk \
                     JOIN studies st ON st.pk = i.study_pk WHERE {} \
                     ORDER BY st.pk, s.series_number, s.pk, i.instance_number, i.pk",
                    conditions.join(" AND ")
                );
                conn.query(&sql, &params, |row| stored_instance(row, &root))
            })
            .await
    }

    /// DICOM JSON metadata of a scope, as a rendered JSON array, or `None` when the scope
    /// matches no instance.
    pub async fn metadata(
        self: &Arc<Self>,
        scope: Scope,
        base: String,
    ) -> Result<Option<Vec<u8>>, IndexError> {
        self.index()
            .readers()
            .run(move |conn| {
                let (sql, params) = scope.select("i.metadata");
                let mut out = Vec::with_capacity(64 * 1024);
                out.push(b'[');
                let mut count = 0;
                conn.for_each(&sql, &params, |row| {
                    if count > 0 {
                        out.push(b',');
                    }
                    out.push(b'{');
                    json::render(row.bytes(0)?, &base, &mut out);
                    out.push(b'}');
                    count += 1;
                    Ok(true)
                })?;
                out.push(b']');
                Ok((count > 0).then_some(out))
            })
            .await
    }

    /// Pixel data location of an instance; `None` when the instance does not exist.
    pub async fn pixels(self: &Arc<Self>, scope: Scope) -> Result<Option<PixelSource>, IndexError> {
        let root = self.files().root().to_owned();
        let found = self
            .index()
            .readers()
            .run(move |conn| {
                query(conn, &scope, "i.path, i.transfer_syntax, i.pixel", |row| {
                    Ok(PixelSource {
                        path: root.join(row.str(0)?),
                        transfer_syntax: row.str(1)?.to_owned(),
                        layout: row.opt_bytes(2)?.and_then(PixelLayout::decode),
                    })
                })
            })
            .await?;
        Ok(found.into_iter().next())
    }

    /// Bulk data at `path` (`GGGGEEEE[/item/GGGGEEEE…]`) of an instance; `None` when the
    /// instance or the attribute does not exist.
    pub async fn bulk_data(
        self: &Arc<Self>,
        scope: Scope,
        path: &str,
    ) -> Result<Option<BulkData>, IndexError> {
        let Some(steps) = parse_path(path) else {
            return Ok(None);
        };
        let Some(source) = self.pixels(scope).await? else {
            return Ok(None);
        };
        if let [Step::Tag(tag)] = steps.as_slice()
            && PIXEL_TAGS.contains(tag)
        {
            return Ok(source.layout.is_some().then_some(BulkData::Pixels(source)));
        }
        let bytes = tokio::task::spawn_blocking(move || read_element(&source.path, &steps))
            .await
            .map_err(|e| IndexError::Database(e.to_string()))?;
        Ok(bytes.map(BulkData::Bytes))
    }
}

const STORED_COLUMNS: &str =
    "st.study_uid, s.series_uid, i.sop_uid, i.sop_class_uid, i.transfer_syntax, i.path, i.size";

fn stored_instance(row: &Row<'_>, root: &std::path::Path) -> Result<StoredInstance, IndexError> {
    Ok(StoredInstance {
        study_uid: row.str(0)?.to_owned(),
        series_uid: row.str(1)?.to_owned(),
        sop_uid: row.str(2)?.to_owned(),
        sop_class_uid: row.str(3)?.to_owned(),
        transfer_syntax: row.str(4)?.to_owned(),
        path: root.join(row.str(5)?),
        size: row.i64(6)? as u64,
    })
}

fn query<T>(
    conn: &mut Connection,
    scope: &Scope,
    columns: &str,
    map: impl FnMut(&Row<'_>) -> Result<T, IndexError>,
) -> Result<Vec<T>, IndexError> {
    let (sql, params) = scope.select(columns);
    conn.query(&sql, &params, map)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Tag(Tag),
    Item(usize),
}

/// Parse a bulk data path: tags and item indexes, alternating, starting with a tag.
fn parse_path(path: &str) -> Option<Vec<Step>> {
    let steps: Vec<Step> = path
        .trim_matches('/')
        .split('/')
        .enumerate()
        .map(|(i, part)| {
            if i % 2 == 0 {
                let value = u32::from_str_radix(part, 16)
                    .ok()
                    .filter(|_| part.len() == 8)?;
                Some(Step::Tag(Tag((value >> 16) as u16, value as u16)))
            } else {
                part.parse().ok().map(Step::Item)
            }
        })
        .collect::<Option<_>>()?;
    matches!(steps.last(), Some(Step::Tag(_))).then_some(steps)
}

/// Read the value of the element at `steps` from a stored file (header part only).
fn read_element(path: &std::path::Path, steps: &[Step]) -> Option<Vec<u8>> {
    let obj = OpenFileOptions::new()
        .read_until(tags::FLOAT_PIXEL_DATA)
        .open_file(path)
        .ok()?;
    let mut current: &InMemDicomObject = &obj;
    let mut iter = steps.iter().peekable();
    while let Some(step) = iter.next() {
        let Step::Tag(tag) = step else { return None };
        let element = current.element_opt(*tag).ok().flatten()?;
        match (element.value(), iter.next()) {
            (DicomValue::Primitive(value), None) => return Some(value.to_bytes().into_owned()),
            (DicomValue::Sequence(sequence), Some(Step::Item(index))) => {
                current = sequence.items().get(*index)?
            }
            _ => return None,
        }
    }
    None
}

/// Read byte ranges of a file.
pub fn read_ranges(path: &std::path::Path, ranges: &[Range]) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let file = std::fs::File::open(path)?;
    let total: u64 = ranges.iter().map(|r| r.length).sum();
    let mut out = vec![0; total as usize];
    let mut position = 0;
    for range in ranges {
        let end = position + range.length as usize;
        file.read_exact_at(&mut out[position..end], range.offset)?;
        position = end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bulk_paths() {
        assert_eq!(
            parse_path("7FE00010"),
            Some(vec![Step::Tag(tags::PIXEL_DATA)])
        );
        assert_eq!(
            parse_path("00540016/1/00181078"),
            Some(vec![
                Step::Tag(Tag(0x0054, 0x0016)),
                Step::Item(1),
                Step::Tag(Tag(0x0018, 0x1078))
            ])
        );
        assert_eq!(parse_path("00540016/1"), None);
        assert_eq!(parse_path("7FE0001"), None);
        assert_eq!(parse_path("zzzz0010"), None);
    }
}
