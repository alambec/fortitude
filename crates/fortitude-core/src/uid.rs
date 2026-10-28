//! DICOM unique identifiers.

/// Whether `uid` is a syntactically valid UID: at most 64 characters, dot-separated
/// numeric components without leading zeros (PS3.5 §9.1).
pub fn is_valid(uid: &str) -> bool {
    !uid.is_empty()
        && uid.len() <= 64
        && uid.split('.').all(|component| {
            !component.is_empty()
                && component.bytes().all(|b| b.is_ascii_digit())
                && (component == "0" || !component.starts_with('0'))
        })
}

#[cfg(test)]
mod tests {
    use super::is_valid;

    #[test]
    fn validates_uids() {
        assert!(is_valid("1.2.840.10008.1.2.1"));
        assert!(is_valid("2.25.0"));
        assert!(!is_valid(""));
        assert!(!is_valid("1..2"));
        assert!(!is_valid("1.02"));
        assert!(!is_valid("1.2a"));
        assert!(!is_valid(&"1.".repeat(40)));
    }
}
