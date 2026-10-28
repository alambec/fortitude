//! Content negotiation for WADO-RS (PS3.18 §8.7.3).

use crate::media::MediaType;

pub const EXPLICIT_VR_LE: &str = "1.2.840.10008.1.2.1";
const IMPLICIT_VR_LE: &str = "1.2.840.10008.1.2";

/// A representation the client accepts.
#[derive(Debug, Clone, PartialEq)]
pub struct Representation {
    pub multipart: bool,
    /// Media type of the payload (of each part when multipart), lowercase; `*/*` for any.
    pub media: String,
    /// Requested transfer syntax: `None` for the media type default, `Some("*")` for any.
    pub transfer_syntax: Option<String>,
}

impl Representation {
    fn any_transfer_syntax(&self) -> bool {
        self.transfer_syntax.as_deref() == Some("*")
    }
}

pub fn representations(accept: &[MediaType]) -> Vec<Representation> {
    accept
        .iter()
        .map(|m| {
            let transfer_syntax = m.param("transfer-syntax").map(str::to_owned);
            match m.essence.as_str() {
                "multipart/related" | "multipart/*" => Representation {
                    multipart: true,
                    media: m.param("type").unwrap_or("*/*").to_ascii_lowercase(),
                    transfer_syntax,
                },
                essence => Representation {
                    multipart: false,
                    media: essence.to_owned(),
                    transfer_syntax,
                },
            }
        })
        .collect()
}

fn is_any(media: &str) -> bool {
    matches!(media, "*/*" | "application/*")
}

/// How instances are returned: multipart or a single `application/dicom` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceFormat {
    Multipart,
    Single,
}

/// Pick how to return instances stored with `stored` transfer syntaxes.
///
/// Instances are returned in their stored transfer syntax when none is requested (as other
/// archives do); a specific transfer syntax is only accepted when it is the stored one.
pub fn instances(
    representations: &[Representation],
    stored: &[&str],
    count: usize,
) -> Option<InstanceFormat> {
    representations.iter().find_map(|r| {
        let media_ok = r.media == "application/dicom" || is_any(&r.media);
        let syntax_ok = match r.transfer_syntax.as_deref() {
            None | Some("*") => true,
            Some(ts) => stored.iter().all(|s| *s == ts),
        };
        if !(media_ok && syntax_ok) {
            return None;
        }
        match (r.multipart, r.media.as_str()) {
            (true, _) => Some(InstanceFormat::Multipart),
            (false, "application/dicom") if count == 1 => Some(InstanceFormat::Single),
            (false, media) if is_any(media) => Some(InstanceFormat::Multipart),
            _ => None,
        }
    })
}

/// Media type of a transfer syntax's pixel data (PS3.18 Table 8.7.3-5).
pub fn pixel_media_type(transfer_syntax: &str) -> &'static str {
    match transfer_syntax {
        "1.2.840.10008.1.2.4.50"
        | "1.2.840.10008.1.2.4.51"
        | "1.2.840.10008.1.2.4.57"
        | "1.2.840.10008.1.2.4.70" => "image/jpeg",
        "1.2.840.10008.1.2.4.80" | "1.2.840.10008.1.2.4.81" => "image/jls",
        "1.2.840.10008.1.2.4.90" | "1.2.840.10008.1.2.4.91" => "image/jp2",
        "1.2.840.10008.1.2.4.92" | "1.2.840.10008.1.2.4.93" => "image/jpx",
        "1.2.840.10008.1.2.4.201" | "1.2.840.10008.1.2.4.202" | "1.2.840.10008.1.2.4.203" => {
            "image/jphc"
        }
        "1.2.840.10008.1.2.5" => "image/dicom-rle",
        "1.2.840.10008.1.2.4.100" | "1.2.840.10008.1.2.4.101" => "video/mpeg",
        "1.2.840.10008.1.2.4.102"
        | "1.2.840.10008.1.2.4.103"
        | "1.2.840.10008.1.2.4.104"
        | "1.2.840.10008.1.2.4.105"
        | "1.2.840.10008.1.2.4.106" => "video/mp4",
        "1.2.840.10008.1.2.4.107" | "1.2.840.10008.1.2.4.108" => "video/H265",
        _ => "application/octet-stream",
    }
}

/// Native pixel data whose bytes are already Explicit VR Little Endian ones.
pub fn is_native_little_endian(transfer_syntax: &str) -> bool {
    matches!(transfer_syntax, EXPLICIT_VR_LE | IMPLICIT_VR_LE)
}

/// Pick the media type of each part when returning pixel data stored with `stored`, as
/// `(multipart, part content type)`.
pub fn pixels(representations: &[Representation], stored: &str) -> Option<(bool, String)> {
    let stored_media = pixel_media_type(stored);
    representations.iter().find_map(|r| {
        let media = if is_any(&r.media) {
            stored_media
        } else {
            r.media.as_str()
        };
        let acceptable = match r.transfer_syntax.as_deref() {
            _ if r.any_transfer_syntax() => {
                media == "application/octet-stream" || media == stored_media
            }
            Some(ts) => ts == stored || (ts == EXPLICIT_VR_LE && is_native_little_endian(stored)),
            None if media == "application/octet-stream" => is_native_little_endian(stored),
            None => media == stored_media,
        };
        acceptable.then(|| {
            (
                r.multipart || is_any(&r.media),
                format!("{media}; transfer-syntax={stored}"),
            )
        })
    })
}

/// Whether DICOM JSON is acceptable.
pub fn json(representations: &[Representation]) -> bool {
    representations.iter().any(|r| {
        !r.multipart
            && (matches!(
                r.media.as_str(),
                "application/dicom+json" | "application/json"
            ) || is_any(&r.media))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::accept;

    fn reps(header: &str) -> Vec<Representation> {
        representations(&accept(Some(header)))
    }

    const JPEG: &str = "1.2.840.10008.1.2.4.50";

    #[test]
    fn instance_formats() {
        assert_eq!(
            instances(&reps("*/*"), &[JPEG], 2),
            Some(InstanceFormat::Multipart)
        );
        assert_eq!(
            instances(&reps("application/dicom"), &[JPEG], 1),
            Some(InstanceFormat::Single)
        );
        assert_eq!(instances(&reps("application/dicom"), &[JPEG], 2), None);
        let explicit =
            r#"multipart/related; type="application/dicom"; transfer-syntax=1.2.840.10008.1.2.1"#;
        assert_eq!(instances(&reps(explicit), &[JPEG], 1), None);
        assert_eq!(
            instances(&reps(explicit), &[EXPLICIT_VR_LE], 1),
            Some(InstanceFormat::Multipart)
        );
        assert_eq!(instances(&reps("application/json"), &[JPEG], 1), None);
    }

    #[test]
    fn pixel_formats() {
        let any = r#"multipart/related; type="application/octet-stream"; transfer-syntax=*"#;
        assert_eq!(
            pixels(&reps(any), JPEG),
            Some((
                true,
                format!("application/octet-stream; transfer-syntax={JPEG}")
            ))
        );
        let raw = r#"multipart/related; type="application/octet-stream""#;
        assert_eq!(pixels(&reps(raw), JPEG), None);
        assert!(pixels(&reps(raw), EXPLICIT_VR_LE).is_some());
        let jpeg = r#"multipart/related; type="image/jpeg""#;
        assert_eq!(
            pixels(&reps(jpeg), JPEG),
            Some((true, format!("image/jpeg; transfer-syntax={JPEG}")))
        );
        assert_eq!(pixels(&reps(jpeg), EXPLICIT_VR_LE), None);
        assert_eq!(
            pixels(&reps("*/*"), JPEG),
            Some((true, format!("image/jpeg; transfer-syntax={JPEG}")))
        );
        assert_eq!(
            pixels(&reps("application/octet-stream"), EXPLICIT_VR_LE).map(|p| p.0),
            Some(false)
        );
    }
}
