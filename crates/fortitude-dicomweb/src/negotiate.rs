//! Content negotiation for WADO-RS (PS3.18 §8.7.3).

use fortitude_store::imaging::{self, ImageFormat};

use crate::media::MediaType;

pub const EXPLICIT_VR_LE: &str = "1.2.840.10008.1.2.1";
const IMPLICIT_VR_LE: &str = "1.2.840.10008.1.2";
pub const DEFLATED_EXPLICIT_VR_LE: &str = "1.2.840.10008.1.2.1.99";

/// A representation the client accepts.
#[derive(Debug, Clone, PartialEq)]
pub struct Representation {
    pub multipart: bool,
    /// Media type of the payload (of each part when multipart), lowercase; `*/*` for any.
    pub media: String,
    /// Requested transfer syntax: `None` for the media type default, `Some("*")` for any.
    pub transfer_syntax: Option<String>,
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

/// Pick how to return instances stored with `stored` transfer syntaxes, and the transfer
/// syntax to transcode them to, if any.
///
/// Instances are returned in their stored transfer syntax when none is requested (as other
/// archives do).
pub fn instances(
    representations: &[Representation],
    stored: &[&str],
    count: usize,
) -> Option<(InstanceFormat, Option<String>)> {
    representations.iter().find_map(|r| {
        if !(r.media == "application/dicom" || is_any(&r.media)) {
            return None;
        }
        let target = match r.transfer_syntax.as_deref() {
            None | Some("*") => None,
            Some(ts) if stored.iter().all(|s| *s == ts) => None,
            Some(ts) if imaging::can_transcode_to(ts) => Some(ts.to_owned()),
            Some(_) => return None,
        };
        let format = match (r.multipart, r.media.as_str()) {
            (true, _) => InstanceFormat::Multipart,
            (false, "application/dicom") if count == 1 => InstanceFormat::Single,
            (false, media) if is_any(media) => InstanceFormat::Multipart,
            _ => return None,
        };
        Some((format, target))
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

/// How to return pixel data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PixelPlan {
    pub multipart: bool,
    /// Content type of each part.
    pub content_type: String,
    /// Frames must be decoded to native little endian pixels.
    pub decode: bool,
}

/// Pick how to return pixel data stored with `stored`.
pub fn pixels(representations: &[Representation], stored: &str) -> Option<PixelPlan> {
    let stored_media = pixel_media_type(stored);
    let native = is_native_little_endian(stored);
    representations.iter().find_map(|r| {
        let media = if is_any(&r.media) {
            stored_media
        } else {
            r.media.as_str()
        };
        let octets = media == "application/octet-stream";
        let decode = match r.transfer_syntax.as_deref() {
            Some("*") if octets || media == stored_media => false,
            Some(ts) if ts == stored => false,
            Some(EXPLICIT_VR_LE) | None if octets => !native,
            None if media == stored_media => false,
            _ => return None,
        };
        let transfer_syntax = if decode { EXPLICIT_VR_LE } else { stored };
        Some(PixelPlan {
            multipart: r.multipart || is_any(&r.media),
            content_type: format!("{media}; transfer-syntax={transfer_syntax}"),
            decode,
        })
    })
}

/// Pick the image format of rendered resources (JPEG by default).
pub fn rendered(representations: &[Representation], quality: u8) -> Option<(bool, ImageFormat)> {
    representations.iter().find_map(|r| {
        let format = match r.media.as_str() {
            "image/jpeg" | "image/*" | "*/*" => ImageFormat::Jpeg { quality },
            "image/png" => ImageFormat::Png,
            _ => return None,
        };
        Some((r.multipart, format))
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
            Some((InstanceFormat::Multipart, None))
        );
        assert_eq!(
            instances(&reps("application/dicom"), &[JPEG], 1),
            Some((InstanceFormat::Single, None))
        );
        assert_eq!(instances(&reps("application/dicom"), &[JPEG], 2), None);
        let explicit =
            r#"multipart/related; type="application/dicom"; transfer-syntax=1.2.840.10008.1.2.1"#;
        assert_eq!(
            instances(&reps(explicit), &[JPEG], 1),
            Some((InstanceFormat::Multipart, Some(EXPLICIT_VR_LE.into())))
        );
        assert_eq!(
            instances(&reps(explicit), &[EXPLICIT_VR_LE], 1),
            Some((InstanceFormat::Multipart, None))
        );
        let unknown = r#"multipart/related; type="application/dicom"; transfer-syntax=1.2.3"#;
        assert_eq!(instances(&reps(unknown), &[JPEG], 1), None);
        assert_eq!(instances(&reps("application/json"), &[JPEG], 1), None);
    }

    #[test]
    fn pixel_formats() {
        let any = r#"multipart/related; type="application/octet-stream"; transfer-syntax=*"#;
        let plan = pixels(&reps(any), JPEG).unwrap();
        assert_eq!(
            plan.content_type,
            format!("application/octet-stream; transfer-syntax={JPEG}")
        );
        assert!(plan.multipart && !plan.decode);

        let raw = r#"multipart/related; type="application/octet-stream""#;
        let decoded = pixels(&reps(raw), JPEG).unwrap();
        assert!(decoded.decode);
        assert_eq!(
            decoded.content_type,
            format!("application/octet-stream; transfer-syntax={EXPLICIT_VR_LE}")
        );
        assert!(!pixels(&reps(raw), EXPLICIT_VR_LE).unwrap().decode);

        let jpeg = r#"multipart/related; type="image/jpeg""#;
        assert_eq!(
            pixels(&reps(jpeg), JPEG).unwrap().content_type,
            format!("image/jpeg; transfer-syntax={JPEG}")
        );
        assert_eq!(pixels(&reps(jpeg), EXPLICIT_VR_LE), None);
        assert_eq!(
            pixels(&reps("*/*"), JPEG).unwrap().content_type,
            format!("image/jpeg; transfer-syntax={JPEG}")
        );
        assert!(
            !pixels(&reps("application/octet-stream"), EXPLICIT_VR_LE)
                .unwrap()
                .multipart
        );
    }

    #[test]
    fn rendered_formats() {
        assert_eq!(
            rendered(&reps("*/*"), 90),
            Some((false, ImageFormat::Jpeg { quality: 90 }))
        );
        assert_eq!(
            rendered(&reps("image/png"), 90),
            Some((false, ImageFormat::Png))
        );
        assert_eq!(rendered(&reps("application/dicom"), 90), None);
    }
}
