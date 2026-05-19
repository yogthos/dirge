//! `file://` URI ↔ filesystem path conversion with percent-encoding.
//!
//! Consolidates what previously lived in both `init.rs` and `client.rs`.
//! The encoding rule: preserve `unreserved` chars + `/` (path separator) +
//! `:` (Windows drive separators, though dirge isn't tested there).
//! Everything else gets `%XX` percent-encoded by raw byte; non-ASCII bytes
//! emit as percent-encoded UTF-8 sequences.
//!
//! Decoding is permissive: invalid `%XX` sequences pass through unchanged,
//! and the result is interpreted as a best-effort UTF-8 string (lossy on
//! ill-formed bytes).

use std::path::{Path, PathBuf};

use lsp_types::Uri;

/// Convert a path to a `file://` URI string. Always returns a string (never
/// fails); call [`path_to_file_uri`] when you need a parsed `Uri` and want
/// the parse error.
pub fn path_to_file_uri_string(path: &Path) -> String {
    let s = path.to_string_lossy();
    let encoded = percent_encode_path(&s);
    if s.starts_with('/') {
        format!("file://{encoded}")
    } else {
        // Relative path or Windows-style. Emit with an extra `/` so the
        // result is parseable as a URI.
        format!("file:///{encoded}")
    }
}

/// Convert a path to a parsed [`Uri`]. Returns an I/O error wrapped around
/// the parse failure so callers can propagate it through their own error
/// chains.
pub fn path_to_file_uri(path: &Path) -> std::io::Result<Uri> {
    let s = path_to_file_uri_string(path);
    s.parse::<Uri>().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid path for file URI: {e}"),
        )
    })
}

/// Decode a `file://` URI back into a [`PathBuf`]. Returns `None` for any
/// scheme other than `file:` (e.g. `https://`).
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let trimmed = uri
        .strip_prefix("file://")
        .or_else(|| uri.strip_prefix("file:"))?;
    Some(PathBuf::from(percent_decode(trimmed)))
}

/// Percent-encode `path` per RFC 3986. Slashes are preserved (path
/// separators). Conforms to `unreserved` + `/` + `:`.
pub fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        let safe =
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~' | b':');
        if safe {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Permissive percent-decoder. Invalid `%XX` sequences pass through as-is.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_value(bytes[i + 1]);
            let lo = hex_value(bytes[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_chars_pass_through_unchanged() {
        let p = Path::new("/tmp/proj_v1.0-rc/main.rs");
        let s = path_to_file_uri_string(p);
        assert_eq!(s, "file:///tmp/proj_v1.0-rc/main.rs");
    }

    // Regression: paths containing URI-significant characters must be
    // percent-encoded. A `#` would otherwise terminate the path early and
    // produce a fragment.
    #[test]
    fn regression_special_chars_are_percent_encoded() {
        let p = Path::new("/tmp/proj #1/main rs");
        let s = path_to_file_uri_string(p);
        assert!(s.contains("%23"), "must encode '#' as %23: {s}");
        assert!(s.contains("%20"), "must encode space as %20: {s}");
    }

    #[test]
    fn round_trip_preserves_path() {
        for p in &[
            "/tmp/a/b/c.rs",
            "/tmp/with spaces/main.rs",
            "/tmp/with#hash/main.rs",
            "/tmp/with?q/main.rs",
        ] {
            let path = PathBuf::from(p);
            let uri = path_to_file_uri_string(&path);
            let decoded = uri_to_path(&uri).unwrap();
            assert_eq!(decoded, path, "round-trip failed for {p}");
        }
    }

    #[test]
    fn non_file_uri_returns_none() {
        assert!(uri_to_path("https://example.com").is_none());
        assert!(uri_to_path("not a uri").is_none());
    }

    #[test]
    fn invalid_percent_escape_passes_through() {
        // Stray `%` followed by non-hex must not panic; emit as-is.
        let s = percent_decode("hello%zz world");
        assert_eq!(s, "hello%zz world");
    }

    #[test]
    fn parses_to_lsp_types_uri() {
        let uri = path_to_file_uri(Path::new("/tmp/main.rs")).unwrap();
        assert_eq!(uri.as_str(), "file:///tmp/main.rs");
    }

    #[test]
    fn multibyte_utf8_percent_encodes_per_byte() {
        let p = Path::new("/tmp/🦀.rs");
        let s = path_to_file_uri_string(p);
        // 🦀 is F0 9F A6 80 in UTF-8 → four %XX escapes.
        assert!(s.contains("%F0%9F%A6%80"), "got: {s}");
        // Round-trips.
        let decoded = uri_to_path(&s).unwrap();
        assert_eq!(decoded, p);
    }
}
