//! Pure ISBN-10 / ISBN-13 logic — the standard ISBN users' manual rules.
//! Ported from `book-api/src/books/isbn/isbn.ts`.

use master_common::error::ApiError;
use serde_json::json;

use crate::CODE_PREFIX;

const ISBN13_GROUPS: [&str; 2] = ["978", "979"];
const BRIDGING_PREFIX: &str = "978";

#[derive(Debug, Clone)]
pub struct NormalisedIsbn {
    pub input: String,
    pub value: String,
    pub is_isbn13: bool,
    pub isbn10: String,
    pub isbn13: String,
    pub check_digit: String,
    pub group: String,
    pub language_group: String,
}

/// Removes hyphens, dashes, underscores and whitespace; upper-cases.
pub fn strip_separators(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '-' | '_' | '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'))
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Mod-11 check digit for the first 9 digits of an ISBN-10.
pub fn isbn10_check_digit(first_nine: &str) -> String {
    let sum: u32 = first_nine
        .chars()
        .enumerate()
        .map(|(i, c)| c.to_digit(10).unwrap_or(0) * (10 - i as u32))
        .sum();
    let check = (11 - (sum % 11)) % 11;
    if check == 10 { "X".to_string() } else { check.to_string() }
}

/// Mod-10 check digit for the first 12 digits of an ISBN-13.
pub fn isbn13_check_digit(first_twelve: &str) -> String {
    let sum: u32 = first_twelve
        .chars()
        .enumerate()
        .map(|(i, c)| c.to_digit(10).unwrap_or(0) * (if i % 2 == 0 { 1 } else { 3 }))
        .sum();
    ((10 - (sum % 10)) % 10).to_string()
}

fn is_isbn10_body(v: &str) -> bool {
    v.len() == 10
        && v.chars().take(9).all(|c| c.is_ascii_digit())
        && (v.chars().nth(9).unwrap().is_ascii_digit() || v.ends_with('X'))
}

fn is_isbn13_body(v: &str) -> bool {
    v.len() == 13 && v.chars().all(|c| c.is_ascii_digit())
}

/// The ISBN-10 language group: one leading digit, or two for the 80x-94x groups.
pub fn isbn10_language_group(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'8' || bytes[0] == b'9')
        && bytes[1] >= b'0'
        && bytes[1] <= b'4'
    {
        value[..2].to_string()
    } else {
        value[..1].to_string()
    }
}

pub fn isbn10_to_isbn13(value: &str) -> String {
    let head = format!("{BRIDGING_PREFIX}{}", &value[..9]);
    format!("{head}{}", isbn13_check_digit(&head))
}

pub fn isbn13_to_isbn10(value: &str) -> Option<String> {
    let group = &value[..3];
    if group != BRIDGING_PREFIX {
        return None; // 979 group: ISBN-13 only.
    }
    let head = &value[3..12];
    Some(format!("{head}{}", isbn10_check_digit(head)))
}

/// Normalises and fully validates a caller-supplied ISBN (400 on any error).
pub fn parse_isbn(raw: &str) -> Result<NormalisedIsbn, ApiError> {
    let invalid = |code: &str, message: String, details: serde_json::Value| {
        ApiError::validation_details(format!("{CODE_PREFIX}_{code}"), message, details)
    };

    if raw.trim().is_empty() {
        return Err(invalid(
            "INVALID_ISBN",
            "An ISBN is required".to_string(),
            json!({ "received": raw }),
        ));
    }
    let value = strip_separators(raw);
    if value.is_empty() {
        return Err(invalid(
            "INVALID_ISBN",
            "An ISBN must contain at least one digit".to_string(),
            json!({ "received": raw }),
        ));
    }
    if !value.chars().all(|c| c.is_ascii_digit() || c == 'X') {
        return Err(invalid(
            "INVALID_ISBN",
            "An ISBN may only contain digits, hyphens, spaces and a trailing \"X\" check digit".to_string(),
            json!({ "received": raw, "normalised": value }),
        ));
    }
    if value.len() != 10 && value.len() != 13 {
        return Err(invalid(
            "INVALID_ISBN",
            format!(
                "An ISBN must be 10 or 13 characters long once hyphens and spaces are removed; received {}",
                value.len()
            ),
            json!({ "received": raw, "normalised": value, "length": value.len() }),
        ));
    }

    if value.len() == 10 {
        if !is_isbn10_body(&value) {
            return Err(invalid(
                "INVALID_ISBN",
                "An ISBN-10 must be nine digits followed by a check digit that is a digit or \"X\"; \"X\" is only valid in the final position".to_string(),
                json!({ "received": raw, "normalised": value }),
            ));
        }
        let expected = isbn10_check_digit(&value[..9]);
        if expected != value[9..] {
            return Err(invalid(
                "INVALID_ISBN",
                format!("ISBN-10 check digit is invalid: \"{}{expected}\" would be valid", &value[..9]),
                json!({ "received": raw, "expected": expected, "normalised": value }),
            ));
        }
        Ok(NormalisedIsbn {
            input: raw.to_string(),
            isbn13: isbn10_to_isbn13(&value),
            isbn10: value.clone(),
            check_digit: value[9..].to_string(),
            group: BRIDGING_PREFIX.to_string(),
            language_group: isbn10_language_group(&value),
            value,
            is_isbn13: false,
        })
    } else {
        if !is_isbn13_body(&value) {
            return Err(invalid(
                "INVALID_ISBN",
                "An ISBN-13 must be thirteen digits; its check digit is always numeric and an \"X\" is never valid".to_string(),
                json!({ "received": raw, "normalised": value }),
            ));
        }
        let group = value[..3].to_string();
        if !ISBN13_GROUPS.contains(&group.as_str()) {
            return Err(invalid(
                "INVALID_ISBN",
                format!("An ISBN-13 must begin with the GS1 prefix \"978\" or \"979\"; received \"{group}\""),
                json!({ "received": raw, "group": group, "normalised": value }),
            ));
        }
        let expected = isbn13_check_digit(&value[..12]);
        if expected != value[12..] {
            return Err(invalid(
                "INVALID_ISBN",
                format!("ISBN-13 check digit is invalid: \"{}{expected}\" would be valid", &value[..12]),
                json!({ "received": raw, "expected": expected, "normalised": value }),
            ));
        }
        Ok(NormalisedIsbn {
            input: raw.to_string(),
            isbn10: isbn13_to_isbn10(&value).unwrap_or_default(),
            isbn13: value.clone(),
            check_digit: value[12..].to_string(),
            group,
            language_group: String::new(),
            value,
            is_isbn13: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_and_converts_isbn10() {
        let n = parse_isbn("978-0-306-40615-7").unwrap(); // actually an ISBN-13
        assert!(n.is_isbn13);
        let n10 = parse_isbn("0-306-40615-2").unwrap();
        assert!(!n10.is_isbn13);
        assert_eq!(n10.isbn13, "9780306406157");
        assert_eq!(n10.language_group, "0");
    }

    #[test]
    fn rejects_bad_check_digit() {
        assert!(parse_isbn("0306406153").is_err()); // wrong check digit
        assert!(parse_isbn("9780306406158").is_err());
    }

    #[test]
    fn rejects_979_isbn10_claim() {
        // 979-prefixed ISBN-13s have no ISBN-10 form.
        let n = parse_isbn("9791234567896").unwrap();
        assert_eq!(n.isbn10, "");
        assert_eq!(n.group, "979");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_isbn("").is_err());
        assert!(parse_isbn("hello").is_err());
        assert!(parse_isbn("12345").is_err());
        assert!(parse_isbn("X123456789").is_err());
    }
}
