//! Device Tree `compatible` property helpers.

/// Return whether a NUL-separated `compatible` property contains `needle`.
pub fn contains(mut value: &[u8], needle: &[u8]) -> bool {
    while !value.is_empty() {
        let end = value.iter().position(|byte| *byte == 0).unwrap_or(value.len());
        if value[..end] == *needle {
            return true;
        }
        if end == value.len() {
            break;
        }
        value = &value[end + 1..];
    }
    false
}

/// Return whether any NUL-separated compatible string starts with `prefix`.
pub fn has_prefix(mut value: &[u8], prefix: &[u8]) -> bool {
    while !value.is_empty() {
        let end = value.iter().position(|byte| *byte == 0).unwrap_or(value.len());
        if value[..end].starts_with(prefix) {
            return true;
        }
        if end == value.len() {
            break;
        }
        value = &value[end + 1..];
    }
    false
}
