//! Pure host-name normalisation and candidate generation for the
//! `universities/domain/{domain}` route.
//!
//! Ported 1:1 from `university-api/src/universities/domain.ts`. Pure: no I/O,
//! no clock, no globals.

use master_common::error::ApiError;
use serde_json::json;

use crate::code;

fn invalid(message: impl Into<String>, details: serde_json::Value) -> ApiError {
    ApiError::validation_details(code("INVALID_REQUEST"), message, details)
}

/// A DNS label: 1-63 chars, alphanumeric with interior hyphens only.
fn is_dns_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    if bytes.is_empty() || bytes.len() > 63 {
        return false;
    }
    let inner_ok = bytes[1..bytes.len() - 1]
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'-');
    let first = bytes[0].is_ascii_alphanumeric();
    let last = bytes[bytes.len() - 1].is_ascii_alphanumeric();
    first && last && inner_ok
}

/// Scheme prefix of a URL, e.g. `https://` (also matches `mailto:`-style
/// schemes with a `//` authority).
fn strip_scheme(value: &str) -> &str {
    let bytes = value.as_bytes();
    // `^[a-z][a-z0-9+.-]*://`
    if bytes.is_empty() || !bytes[0].is_ascii_lowercase() {
        return value;
    }
    let mut i = 1;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'+' || bytes[i] == b'.' || bytes[i] == b'-') {
        i += 1;
    }
    if bytes[i..].starts_with(b"://") {
        &value[i + 3..]
    } else {
        value
    }
}

/// Reduces any hostname-ish string to a bare, lower-case host.
///
/// Accepts what a link shortener, a mail parser or a spam filter actually
/// hands you: a full URL, a scheme-relative URL, `user@host`, `host:port`, a
/// path, a trailing dot, or a bare host. Returns `None` for anything that
/// cannot be a host, so the caller decides whether that is a 400 or a 404.
pub fn normalize_host(input: &str) -> Option<String> {
    let mut value = input.trim().to_lowercase();
    if value.is_empty() {
        return None;
    }

    // 1. Scheme. Also collapses `https:example.edu`.
    value = strip_scheme(&value).to_string();
    if let Some(rest) = value.strip_prefix("//") {
        value = rest.to_string();
    }

    // 2. Path, query and fragment. A `?` or `#` can appear in either order.
    if let Some(cut) = value.find(['/', '?', '#']) {
        value.truncate(cut);
    }

    // 3. Userinfo. The last `@` is the host separator.
    if let Some(at) = value.rfind('@') {
        value = value[at + 1..].to_string();
    }

    // 4. Port, including the IPv6-with-bracket form: `[::1]:8080` -> `[::1]`.
    if value.starts_with('[') {
        if let Some(end) = value.find(']') {
            let tail = &value[end + 1..];
            let port = tail.strip_prefix(':').unwrap_or("");
            if tail.is_empty() || (!port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())) {
                value.truncate(end + 1);
            }
        }
    } else if let Some(colon) = value.rfind(':') {
        let port = &value[colon + 1..];
        if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) {
            value.truncate(colon);
        }
    }

    // 5. A fully-qualified name may end in a root dot; it is not significant.
    let trimmed = value.trim_end_matches('.');
    value = trimmed.to_string();

    // 6. `www` is a hosting convention, never part of the registrable domain.
    if let Some(rest) = value.strip_prefix("www.") {
        value = rest.to_string();
    }

    if value.len() > 253 {
        return None;
    }
    let labels: Vec<&str> = value.split('.').collect();
    if labels.len() < 2 {
        return None;
    }
    if !labels.iter().all(|label| is_dns_label(label)) {
        return None;
    }
    Some(value)
}

/// Every host a lookup should try, most specific first, down to the last two
/// labels — the shortest suffix that can be a registrable domain.
///
/// `dept.sub.example.co.uk` -> `['dept.sub.example.co.uk', 'sub.example.co.uk',
/// 'example.co.uk']`. The dataset is consulted in this order rather than a
/// bundled public-suffix list, so `example.ac.uk` and `example.co.uk` are both
/// handled correctly.
pub fn host_candidates(host: &str) -> Vec<String> {
    let labels: Vec<&str> = host.split('.').collect();
    let mut candidates = Vec::new();
    if labels.len() >= 2 {
        for i in 0..=labels.len() - 2 {
            candidates.push(labels[i..].join("."));
        }
    }
    candidates
}

/// `normalize_host` plus the 400 the API contract promises. Kept separate from
/// the pure function so `normalize_host` stays trivially testable.
pub fn require_host(input: &str, param_name: &str) -> Result<String, ApiError> {
    normalize_host(input).ok_or_else(|| {
        invalid(
            format!(
                "'{param_name}' is not a valid host name. Pass a domain such as ox.ac.uk, or a URL such as https://dept.example.edu/path."
            ),
            json!({ "parameter": param_name, "value": input }),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_urls_userinfo_ports_and_paths() {
        assert_eq!(normalize_host("ox.ac.uk").as_deref(), Some("ox.ac.uk"));
        assert_eq!(
            normalize_host("https://dept.example.edu/path?x=1#frag").as_deref(),
            Some("dept.example.edu")
        );
        assert_eq!(
            normalize_host("HTTP://Example.EDU:8080/").as_deref(),
            Some("example.edu")
        );
        assert_eq!(
            normalize_host("user@mail.university.edu").as_deref(),
            Some("mail.university.edu")
        );
        assert_eq!(
            normalize_host("//cdn.example.ac.uk").as_deref(),
            Some("example.ac.uk")
        );
        assert_eq!(
            normalize_host("www.cam.ac.uk.").as_deref(),
            Some("cam.ac.uk")
        );
        assert_eq!(
            normalize_host("https:example.edu").as_deref(),
            Some("example.edu")
        );
    }

    #[test]
    fn rejects_values_that_cannot_be_hosts() {
        assert_eq!(normalize_host(""), None);
        assert_eq!(normalize_host("   "), None);
        assert_eq!(normalize_host("localhost"), None, "single label is not a host");
        assert_eq!(normalize_host("not a host"), None, "spaces are not labels");
        assert_eq!(normalize_host("-bad.example.edu"), None, "leading hyphen is not a label");
        assert_eq!(normalize_host("https://"), None);
        let long = format!("{}.edu", "a".repeat(250));
        assert_eq!(normalize_host(&long), None, "over 253 characters is rejected");
    }

    #[test]
    fn candidates_are_most_specific_first_down_to_two_labels() {
        assert_eq!(
            host_candidates("dept.sub.example.co.uk"),
            vec![
                "dept.sub.example.co.uk".to_string(),
                "sub.example.co.uk".to_string(),
                "example.co.uk".to_string()
            ]
        );
        assert_eq!(host_candidates("example.edu"), vec!["example.edu".to_string()]);
        assert!(host_candidates("edu").is_empty());
    }

    #[test]
    fn require_host_turns_garbage_into_a_400() {
        assert!(require_host("ox.ac.uk", "domain").is_ok());
        let err = require_host("not a host", "domain").unwrap_err();
        assert!(err.to_string().contains("is not a valid host name"));
    }
}
