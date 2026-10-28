//! DICOM JSON model encoder (PS3.18 Annex F) producing *member lists*.
//!
//! Fortitude encodes attributes once, at ingest, and serves them many times. A member list is
//! the serialized top-level members of a JSON object (`"GGGGEEEE":{...}`), each separated by
//! [`MEMBER_SEP`], in tag order. Bulk data URIs start with [`BASE_MARKER`], which stands for
//! the DICOMweb base URL of the request. Both markers are control characters, which JSON
//! strings always escape, so they can never be confused with encoded content.
//!
//! [`members`] splits a list back into `(tag, member)` pairs and [`render_object`] turns
//! member lists into a JSON object for a response.

use std::fmt::Write as _;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use dicom_core::value::{PrimitiveValue, Value};
use dicom_core::{Tag, VR};
use dicom_dictionary_std::tags;
use dicom_object::InMemDicomObject;
use dicom_object::mem::InMemElement;

/// Placeholder for the DICOMweb base URL (e.g. `https://host/dicom-web`).
pub const BASE_MARKER: u8 = 0x01;
/// Separator between top-level members of a member list.
pub const MEMBER_SEP: u8 = 0x02;

/// Specific Character Set of every encoded value: the JSON model is always UTF-8.
const UTF8_CHARSET: &str = "ISO_IR 192";

#[derive(Debug, Clone)]
pub struct EncodeOptions<'a> {
    /// Binary values up to this many bytes are inlined (`InlineBinary`), larger ones are
    /// referenced (`BulkDataURI`).
    pub inline_binary_max: usize,
    /// Prefix of bulk data URIs, without trailing slash; the attribute path is appended
    /// (e.g. `\x01/studies/1.2/series/1.3/instances/1.4/bulk`).
    pub bulk_prefix: &'a str,
}

/// Append to `out` the members of `obj` whose tag is accepted by `filter`, separated by
/// [`MEMBER_SEP`]. Group lengths and the file meta group are never encoded.
pub fn encode_members(
    obj: &InMemDicomObject,
    options: &EncodeOptions<'_>,
    filter: impl Fn(Tag) -> bool,
    out: &mut String,
) {
    let mut path = String::new();
    for element in obj.iter() {
        let tag = element.header().tag;
        if !encodable(tag) || !filter(tag) {
            continue;
        }
        if !out.is_empty() {
            out.push(MEMBER_SEP as char);
        }
        encode_element(element, options, &mut path, out);
    }
}

/// Append a member referencing bulk data at `<bulk_prefix>/<tag>`, e.g. for pixel data,
/// which is never loaded at ingest.
pub fn push_bulk_member(tag: Tag, vr: VR, options: &EncodeOptions<'_>, out: &mut String) {
    if !out.is_empty() {
        out.push(MEMBER_SEP as char);
    }
    push_key(tag, out);
    let _ = write!(
        out,
        "{{\"vr\":\"{}\",\"BulkDataURI\":\"{}/{}\"}}",
        vr.to_string(),
        options.bulk_prefix,
        tag_hex(tag)
    );
}

/// Iterate over the `(tag, member)` pairs of a member list.
pub fn members(list: &[u8]) -> impl Iterator<Item = (Tag, &[u8])> {
    list.split(|b| *b == MEMBER_SEP)
        .filter(|m| m.len() > 10)
        .filter_map(|member| Some((parse_tag_key(member)?, member)))
}

/// Tag of a member starting with `"GGGGEEEE"`.
fn parse_tag_key(member: &[u8]) -> Option<Tag> {
    let hex = std::str::from_utf8(member.get(1..9)?).ok()?;
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(Tag((value >> 16) as u16, value as u16))
}

/// Write a JSON object made of `members` (any order, duplicates allowed: the first
/// occurrence of a tag wins) into `out`, replacing [`BASE_MARKER`] by `base`.
pub fn render_object(members: &mut Vec<(Tag, &[u8])>, base: &str, out: &mut Vec<u8>) {
    members.sort_by_key(|(tag, _)| *tag);
    members.dedup_by_key(|(tag, _)| *tag);
    out.push(b'{');
    for (i, (_, member)) in members.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        render(member, base, out);
    }
    out.push(b'}');
}

/// Copy `encoded` into `out`, replacing [`BASE_MARKER`] by `base` and [`MEMBER_SEP`] by `,`.
pub fn render(encoded: &[u8], base: &str, out: &mut Vec<u8>) {
    let mut rest = encoded;
    while let Some(i) = memchr::memchr2(BASE_MARKER, MEMBER_SEP, rest) {
        out.extend_from_slice(&rest[..i]);
        if rest[i] == BASE_MARKER {
            out.extend_from_slice(base.as_bytes());
        } else {
            out.push(b',');
        }
        rest = &rest[i + 1..];
    }
    out.extend_from_slice(rest);
}

/// A member with a single string value, e.g. for computed attributes.
pub fn string_member(tag: Tag, vr: VR, value: &str) -> String {
    let mut out = String::new();
    push_key(tag, &mut out);
    let _ = write!(out, "{{\"vr\":\"{}\",\"Value\":[", vr.to_string());
    push_json_string(value, &mut out);
    out.push_str("]}");
    out
}

/// A member with string values.
pub fn strings_member<'s>(tag: Tag, vr: VR, values: impl IntoIterator<Item = &'s str>) -> String {
    let mut out = String::new();
    push_key(tag, &mut out);
    let _ = write!(out, "{{\"vr\":\"{}\"", vr.to_string());
    let mut values = values.into_iter().peekable();
    if values.peek().is_some() {
        out.push_str(",\"Value\":[");
        for (i, value) in values.enumerate() {
            if i > 0 {
                out.push(',');
            }
            push_json_string(value, &mut out);
        }
        out.push(']');
    }
    out.push('}');
    out
}

/// A member with a single integer value.
pub fn integer_member(tag: Tag, vr: VR, value: i64) -> String {
    let mut out = String::new();
    push_key(tag, &mut out);
    let _ = write!(out, "{{\"vr\":\"{}\",\"Value\":[{value}]}}", vr.to_string());
    out
}

/// A member holding a sequence of already encoded items (complete JSON objects).
pub fn sequence_member(tag: Tag, items: &[String]) -> String {
    let mut out = String::new();
    push_key(tag, &mut out);
    out.push_str("{\"vr\":\"SQ\",\"Value\":[");
    out.push_str(&items.join(","));
    out.push_str("]}");
    out
}

pub fn tag_hex(tag: Tag) -> String {
    format!("{:04X}{:04X}", tag.group(), tag.element())
}

fn encodable(tag: Tag) -> bool {
    tag.element() != 0 && tag.group() != 0x0002 && tag.group() != 0xFFFE
}

fn push_key(tag: Tag, out: &mut String) {
    let _ = write!(out, "\"{:04X}{:04X}\":", tag.group(), tag.element());
}

fn encode_element(
    element: &InMemElement,
    options: &EncodeOptions<'_>,
    path: &mut String,
    out: &mut String,
) {
    let tag = element.header().tag;
    let vr = element.vr();
    push_key(tag, out);
    let _ = write!(out, "{{\"vr\":\"{}\"", vr.to_string());

    let path_len = path.len();
    if !path.is_empty() {
        path.push('/');
    }
    path.push_str(&tag_hex(tag));

    match element.value() {
        Value::Sequence(sequence) => {
            if !sequence.items().is_empty() {
                out.push_str(",\"Value\":[");
                for (index, item) in sequence.items().iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    let item_path_len = path.len();
                    let _ = write!(path, "/{index}");
                    encode_item(item, options, path, out);
                    path.truncate(item_path_len);
                }
                out.push(']');
            }
        }
        Value::PixelSequence(_) => push_bulk_uri(options, path, out),
        Value::Primitive(value) => encode_primitive(tag, vr, value, options, path, out),
    }
    path.truncate(path_len);
    out.push('}');
}

fn encode_item(
    item: &InMemDicomObject,
    options: &EncodeOptions<'_>,
    path: &mut String,
    out: &mut String,
) {
    out.push('{');
    let mut first = true;
    for element in item.iter() {
        if !encodable(element.header().tag) {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        encode_element(element, options, path, out);
    }
    out.push('}');
}

fn push_bulk_uri(options: &EncodeOptions<'_>, path: &str, out: &mut String) {
    let _ = write!(out, ",\"BulkDataURI\":\"{}/{path}\"", options.bulk_prefix);
}

fn encode_primitive(
    tag: Tag,
    vr: VR,
    value: &PrimitiveValue,
    options: &EncodeOptions<'_>,
    path: &str,
    out: &mut String,
) {
    if value.multiplicity() == 0 || matches!(value, PrimitiveValue::Empty) {
        return;
    }
    match vr {
        VR::OB | VR::OD | VR::OF | VR::OL | VR::OV | VR::OW | VR::UN => {
            let bytes = value.to_bytes();
            if bytes.is_empty() {
            } else if bytes.len() <= options.inline_binary_max {
                out.push_str(",\"InlineBinary\":\"");
                out.push_str(&BASE64.encode(&bytes));
                out.push('"');
            } else {
                push_bulk_uri(options, path, out);
            }
        }
        VR::PN => {
            let values = value.to_multi_str();
            out.push_str(",\"Value\":[");
            for (i, name) in values.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_person_name(name, out);
            }
            out.push(']');
        }
        VR::IS | VR::DS | VR::FL | VR::FD | VR::SL | VR::SS | VR::UL | VR::US | VR::SV | VR::UV => {
            out.push_str(",\"Value\":[");
            push_numbers(value, out);
            out.push(']');
        }
        VR::AT => {
            out.push_str(",\"Value\":[");
            if let PrimitiveValue::Tags(values) = value {
                for (i, t) in values.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    let _ = write!(out, "\"{}\"", tag_hex(*t));
                }
            }
            out.push(']');
        }
        VR::LT | VR::ST | VR::UT => {
            out.push_str(",\"Value\":[");
            push_json_string(value.to_str().trim_end_matches([' ', '\0']), out);
            out.push(']');
        }
        _ if tag == tags::SPECIFIC_CHARACTER_SET => {
            out.push_str(",\"Value\":[");
            push_json_string(UTF8_CHARSET, out);
            out.push(']');
        }
        _ => {
            out.push_str(",\"Value\":[");
            for (i, s) in value.to_multi_str().iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let s = s.trim_matches([' ', '\0']);
                if s.is_empty() {
                    out.push_str("null");
                } else {
                    push_json_string(s, out);
                }
            }
            out.push(']');
        }
    }
}

fn push_person_name(name: &str, out: &mut String) {
    let name = name.trim_matches([' ', '\0']);
    if name.is_empty() {
        out.push_str("null");
        return;
    }
    out.push('{');
    let mut first = true;
    for (group, label) in name
        .split('=')
        .zip(["Alphabetic", "Ideographic", "Phonetic"])
    {
        let group = group.trim_end_matches(' ');
        if group.is_empty() {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        let _ = write!(out, "\"{label}\":");
        push_json_string(group, out);
    }
    out.push('}');
}

fn push_numbers(value: &PrimitiveValue, out: &mut String) {
    fn join<T>(values: &[T], out: &mut String, mut push: impl FnMut(&T, &mut String)) {
        for (i, v) in values.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            push(v, out);
        }
    }
    let float = |v: f64, out: &mut String| {
        if v.is_finite() {
            let _ = write!(out, "{v}");
        } else {
            out.push_str("null");
        }
    };
    match value {
        PrimitiveValue::I16(v) => join(v, out, |n, o| {
            let _ = write!(o, "{n}");
        }),
        PrimitiveValue::U16(v) => join(v, out, |n, o| {
            let _ = write!(o, "{n}");
        }),
        PrimitiveValue::I32(v) => join(v, out, |n, o| {
            let _ = write!(o, "{n}");
        }),
        PrimitiveValue::U32(v) => join(v, out, |n, o| {
            let _ = write!(o, "{n}");
        }),
        PrimitiveValue::I64(v) => join(v, out, |n, o| {
            let _ = write!(o, "{n}");
        }),
        PrimitiveValue::U64(v) => join(v, out, |n, o| {
            let _ = write!(o, "{n}");
        }),
        PrimitiveValue::F32(v) => join(v, out, |n, o| {
            if n.is_finite() {
                let _ = write!(o, "{n}");
            } else {
                o.push_str("null");
            }
        }),
        PrimitiveValue::F64(v) => join(v, out, |n, o| float(*n, o)),
        _ => {
            let strings = value.to_multi_str();
            join(&strings, out, |s, o| {
                push_decimal_string(s.trim_matches([' ', '\0']), o)
            });
        }
    }
}

/// Write an IS/DS value as a JSON number, keeping its textual precision when it is already
/// a valid JSON number.
fn push_decimal_string(s: &str, out: &mut String) {
    if is_json_number(s) {
        out.push_str(s);
    } else if let Ok(v) = s.parse::<f64>()
        && v.is_finite()
    {
        let _ = write!(out, "{v}");
    } else {
        out.push_str("null");
    }
}

fn is_json_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if b.get(i) == Some(&b'-') {
        i += 1;
    }
    let digits = |i: &mut usize| {
        let start = *i;
        while b.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i - start
    };
    match b.get(i) {
        Some(b'0') => i += 1,
        Some(c) if c.is_ascii_digit() => {
            digits(&mut i);
        }
        _ => return false,
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        if digits(&mut i) == 0 {
            return false;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return false;
        }
    }
    i == b.len()
}

/// Append `s` as a JSON string literal.
pub fn push_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use dicom_core::{DataElement, PrimitiveValue, dicom_value};
    use dicom_object::mem::InMemDicomObject;

    use super::*;

    fn options() -> EncodeOptions<'static> {
        EncodeOptions {
            inline_binary_max: 4,
            bulk_prefix: "\u{1}/studies/1/series/2/instances/3/bulk",
        }
    }

    fn encode(obj: &InMemDicomObject) -> serde_json::Value {
        let mut list = String::new();
        encode_members(obj, &options(), |_| true, &mut list);
        let mut members: Vec<_> = members(list.as_bytes()).collect();
        let mut out = Vec::new();
        render_object(&mut members, "http://h/dw", &mut out);
        serde_json::from_slice(&out).expect("valid JSON")
    }

    #[test]
    fn encodes_value_representations() {
        let obj = InMemDicomObject::from_element_iter([
            DataElement::new(
                tags::SPECIFIC_CHARACTER_SET,
                VR::CS,
                PrimitiveValue::from("ISO_IR 100"),
            ),
            DataElement::new(
                tags::IMAGE_TYPE,
                VR::CS,
                dicom_value!(Strs, ["ORIGINAL", "", "AXIAL "]),
            ),
            DataElement::new(
                tags::PATIENT_NAME,
                VR::PN,
                PrimitiveValue::from("Doe^John=Ideo"),
            ),
            DataElement::new(
                tags::PATIENT_COMMENTS,
                VR::LT,
                PrimitiveValue::from("a \"quoted\"\\ line\n"),
            ),
            DataElement::new(tags::SERIES_NUMBER, VR::IS, PrimitiveValue::from(" 12 ")),
            DataElement::new(
                tags::SLICE_THICKNESS,
                VR::DS,
                dicom_value!(Strs, ["1.25", "+2", ".5"]),
            ),
            DataElement::new(tags::ROWS, VR::US, PrimitiveValue::from(512_u16)),
            DataElement::new(
                tags::FRAME_INCREMENT_POINTER,
                VR::AT,
                PrimitiveValue::Tags(vec![tags::FRAME_TIME].into()),
            ),
            DataElement::new(tags::REFERENCED_FRAME_NUMBER, VR::IS, PrimitiveValue::Empty),
            DataElement::new(
                Tag(0x0009, 0x1001),
                VR::OB,
                PrimitiveValue::U8(vec![1, 2, 3].into()),
            ),
            DataElement::new(
                Tag(0x0009, 0x1002),
                VR::OB,
                PrimitiveValue::U8(vec![0; 16].into()),
            ),
        ]);
        let json = encode(&obj);
        assert_eq!(json["00080005"]["Value"][0], "ISO_IR 192");
        assert_eq!(
            json["00080008"]["Value"],
            serde_json::json!(["ORIGINAL", null, "AXIAL"])
        );
        assert_eq!(
            json["00100010"]["Value"][0],
            serde_json::json!({"Alphabetic": "Doe^John", "Ideographic": "Ideo"})
        );
        assert_eq!(json["00104000"]["Value"][0], "a \"quoted\"\\ line\n");
        assert_eq!(json["00200011"]["Value"], serde_json::json!([12]));
        assert_eq!(json["00180050"]["Value"], serde_json::json!([1.25, 2, 0.5]));
        assert_eq!(json["00280010"]["Value"], serde_json::json!([512]));
        assert_eq!(json["00280009"]["Value"], serde_json::json!(["00181063"]));
        assert_eq!(json["00081160"], serde_json::json!({"vr": "IS"}));
        assert_eq!(json["00091001"]["InlineBinary"], "AQID");
        assert_eq!(
            json["00091002"]["BulkDataURI"],
            "http://h/dw/studies/1/series/2/instances/3/bulk/00091002"
        );
    }

    #[test]
    fn encodes_sequences_with_bulk_paths() {
        let item = InMemDicomObject::from_element_iter([
            DataElement::new(tags::CODE_VALUE, VR::SH, PrimitiveValue::from("123")),
            DataElement::new(
                Tag(0x0009, 0x1010),
                VR::OB,
                PrimitiveValue::U8(vec![0; 8].into()),
            ),
        ]);
        let obj = InMemDicomObject::from_element_iter([DataElement::new(
            tags::ANATOMIC_REGION_SEQUENCE,
            VR::SQ,
            dicom_core::value::DataSetSequence::from(vec![InMemDicomObject::new_empty(), item]),
        )]);
        let json = encode(&obj);
        let items = &json["00082218"]["Value"];
        assert_eq!(items[0], serde_json::json!({}));
        assert_eq!(items[1]["00080100"]["Value"][0], "123");
        assert_eq!(
            items[1]["00091010"]["BulkDataURI"],
            "http://h/dw/studies/1/series/2/instances/3/bulk/00082218/1/00091010"
        );
    }

    #[test]
    fn filters_and_merges_member_lists() {
        let obj = InMemDicomObject::from_element_iter([
            DataElement::new(tags::MODALITY, VR::CS, PrimitiveValue::from("CT")),
            DataElement::new(tags::PATIENT_ID, VR::LO, PrimitiveValue::from("P1")),
        ]);
        let mut list = String::new();
        encode_members(&obj, &options(), |t| t == tags::PATIENT_ID, &mut list);
        push_bulk_member(tags::PIXEL_DATA, VR::OW, &options(), &mut list);
        let computed = integer_member(tags::NUMBER_OF_STUDY_RELATED_SERIES, VR::IS, 3);
        let mut all: Vec<_> = members(list.as_bytes()).collect();
        all.push((tags::NUMBER_OF_STUDY_RELATED_SERIES, computed.as_bytes()));
        let mut out = Vec::new();
        render_object(&mut all, "B", &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"00100020":{"vr":"LO","Value":["P1"]},"00201206":{"vr":"IS","Value":[3]},"7FE00010":{"vr":"OW","BulkDataURI":"B/studies/1/series/2/instances/3/bulk/7FE00010"}}"#
        );
    }

    #[test]
    fn json_number_grammar() {
        for ok in ["0", "-1", "1.5", "1e5", "1.5E-3"] {
            assert!(is_json_number(ok), "{ok}");
        }
        for bad in ["", "+1", ".5", "1.", "01", "1e", "nan"] {
            assert!(!is_json_number(bad), "{bad}");
        }
    }
}
