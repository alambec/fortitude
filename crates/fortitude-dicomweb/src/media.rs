//! Media types of `Content-Type` and `Accept` headers (RFC 9110 §8.3.1, §12.5.1).

/// A parsed media type: lowercase `type/subtype` and its parameters (lowercase names,
/// unquoted values).
#[derive(Debug, Clone, PartialEq)]
pub struct MediaType {
    pub essence: String,
    pub params: Vec<(String, String)>,
}

impl MediaType {
    pub fn parse(value: &str) -> Option<Self> {
        let mut parts = split_unquoted(value, ';').into_iter();
        let essence = parts.next()?.trim().to_ascii_lowercase();
        if essence.is_empty() || !essence.contains('/') {
            return None;
        }
        let params = parts
            .filter_map(|p| {
                let (name, value) = p.split_once('=')?;
                let value = value.trim();
                let value = value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(value);
                Some((name.trim().to_ascii_lowercase(), value.to_owned()))
            })
            .collect();
        Some(Self { essence, params })
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Quality value (`q` parameter), 1 when absent.
    pub fn quality(&self) -> f32 {
        self.param("q").and_then(|q| q.parse().ok()).unwrap_or(1.0)
    }
}

/// Media ranges of an `Accept` header, best quality first; `*/*` when absent or empty.
pub fn accept(header: Option<&str>) -> Vec<MediaType> {
    let mut ranges: Vec<MediaType> = header
        .map(|h| {
            split_unquoted(h, ',')
                .iter()
                .filter_map(|r| MediaType::parse(r))
                .collect()
        })
        .unwrap_or_default();
    if ranges.is_empty() {
        ranges.push(MediaType {
            essence: "*/*".into(),
            params: Vec::new(),
        });
    }
    ranges.sort_by(|a, b| b.quality().total_cmp(&a.quality()));
    ranges.retain(|r| r.quality() > 0.0);
    ranges
}

/// Split on `separator`, ignoring separators inside double quotes.
fn split_unquoted(value: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (i, c) in value.char_indices() {
        match c {
            '"' => quoted = !quoted,
            c if c == separator && !quoted => {
                parts.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quoted_parameters() {
        let m =
            MediaType::parse(r#"Multipart/Related; type="application/dicom"; boundary="a;b,c""#)
                .unwrap();
        assert_eq!(m.essence, "multipart/related");
        assert_eq!(m.param("type"), Some("application/dicom"));
        assert_eq!(m.param("boundary"), Some("a;b,c"));
    }

    #[test]
    fn accept_orders_by_quality() {
        let ranges = accept(Some(
            r#"application/json;q=0.5, multipart/related; type="application/octet-stream"; transfer-syntax=*, */*;q=0"#,
        ));
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].essence, "multipart/related");
        assert_eq!(ranges[0].param("transfer-syntax"), Some("*"));
        assert_eq!(accept(None)[0].essence, "*/*");
    }
}
