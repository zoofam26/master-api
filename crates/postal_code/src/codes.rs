//! Postal-code normalisation, country-code validation, GeoNames coordinate
//! coercion and the published truncation caveats.
//!
//! Ported 1:1 from `postal-code-api/src/postal/postal-code.ts`.

use master_common::error::ApiError;
use serde_json::json;

/// A syntactically valid ISO 3166-1 alpha-2 code: exactly two ASCII letters.
fn is_country_code_shape(value: &str) -> bool {
    value.len() == 2 && value.bytes().all(|b| b.is_ascii_alphabetic())
}

/// Trims, collapses every run of internal whitespace to a single space and
/// uppercases.
///
/// This is the *presentation* form and the value that is handed to a country's
/// postal pattern, because some GeoNames patterns care about a single space
/// (`SE` is `^(?:SE)?\d{3}\s\d{2}$` — exactly one separator) and some do not
/// (`GB` is `…\s?[0-9][A-Za-z]{2}$` — zero or one).
///
/// Punctuation is deliberately preserved: the hyphen is part of the format in
/// `JP` (`###-####`), `BR` (`#####-###`) and `PL` (`##-###`), and the leading
/// `L-` is part of Luxembourg's published code, so stripping it before
/// validating would reject codes that are genuinely correct.
pub fn format_postal_code(raw: &str) -> String {
    raw.trim().split_whitespace().collect::<Vec<_>>().join(" ").to_uppercase()
}

/// The *matching* form: [`format_postal_code`] with every non-alphanumeric
/// character removed, e.g. `sw1a 1aa` → `SW1A1AA`, `100-0001` → `1000001`.
///
/// GeoNames publishes each code once but a caller may reasonably supply it
/// hyphenated, spaced or neither, so dataset lookups compare on this form. It
/// is never used for pattern validation, because the hyphen is meaningful there.
pub fn normalise_postal_code_key(raw: &str) -> String {
    format_postal_code(raw)
        .chars()
        .filter(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
        .collect()
}

/// Validates the *shape* of a country code and returns it uppercased.
///
/// This is deliberately separate from the "is this country supported" check: a
/// malformed shape is the caller's mistake (`400`) and costs no upstream
/// request, whereas a well-formed code GeoNames has never heard of is a genuine
/// `404`.
pub fn normalise_country_code(raw: &str) -> Result<String, ApiError> {
    let trimmed = raw.trim();
    if !is_country_code_shape(trimmed) {
        return Err(ApiError::validation_details(
            "INVALID_REQUEST",
            "A country code must be exactly two letters, for example \"US\" or \"GB\"",
            json!({ "received": trimmed, "expected": "two ASCII letters" }),
        ));
    }
    Ok(trimmed.to_uppercase())
}

/// Coerces a GeoNames coordinate field, which is a string and may be empty.
pub fn parse_coordinate(raw: Option<&str>) -> Option<f64> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// Coerces the GeoNames accuracy column, which is a string and may be empty.
pub fn parse_accuracy(raw: Option<&str>) -> Option<i64> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value = trimmed.parse::<f64>().ok()?;
    if value.fract() == 0.0 && value.is_finite() {
        Some(value as i64)
    } else {
        None
    }
}

/// `None` for an empty GeoNames text column, so responses do not carry `""`.
pub fn text_or_null(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Hard cap on an inbound postal code before any matching is attempted.
pub const MAX_POSTAL_CODE_LENGTH: usize = 24;

/// Hard cap on the string handed to a country regex. Every GeoNames pattern
/// targets at most 8 characters, so anything longer cannot match and is
/// rejected before the regex engine is involved at all.
pub const MAX_PATTERN_INPUT_LENGTH: usize = 32;

/// Rejects an empty or absurdly long postal code before any work is done.
pub fn clean_postal_code(raw: &str) -> Result<String, ApiError> {
    let trimmed = raw.trim().split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.is_empty() {
        return Err(ApiError::validation_details(
            "INVALID_REQUEST",
            "A postal code is required",
            json!({ "received": raw }),
        ));
    }
    if trimmed.len() > MAX_POSTAL_CODE_LENGTH {
        return Err(ApiError::validation_details(
            "INVALID_REQUEST",
            format!("A postal code must not exceed {MAX_POSTAL_CODE_LENGTH} characters"),
            json!({ "length": trimmed.len(), "maximum": MAX_POSTAL_CODE_LENGTH }),
        ));
    }
    Ok(trimmed)
}

/// Truncation caveats published by GeoNames in `readme.txt`, keyed by country.
///
/// The standard per-country subsets contain only the first part of the code
/// for CA, NL and UK, so surfacing this is not optional: a `GB/SW1A` result is
/// not a defect of this service, it is exactly what the source publishes.
pub fn truncated_note(code: &str) -> Option<&'static str> {
    match code {
        "CA" => Some("The CA dataset contains only the first three characters of each postal code (Forward Sortation Area). Full codes are published separately as CA_full.csv.zip."),
        "GB" => Some("The GB dataset contains only the outward code (e.g. SW1A), not the full postcode (SW1A 1AA). Full postcodes are published separately as GB_full.csv.zip."),
        "NL" => Some("The NL dataset contains only the first four characters of each postal code. Full codes are published separately as NL_full.csv.zip."),
        "AR" => Some("The AR dataset contains only the first five positions of each postal code."),
        "BR" => Some("The BR dataset contains only major postal codes (those ending in -000 plus one per municipality)."),
        "CL" => Some("The CL dataset contains only the first digits of each postal code, for copyright reasons."),
        "CN" => Some("The CN dataset contains only the first digits of each postal code, ending in 00."),
        "IE" => Some("The IE dataset contains only the first letters of each postal code, for copyright reasons."),
        "MT" => Some("The MT dataset contains only the first letters of each postal code, for copyright reasons."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn country_codes_must_be_two_ascii_letters() {
        assert_eq!(normalise_country_code(" us ").unwrap(), "US");
        assert_eq!(normalise_country_code("Gb").unwrap(), "GB");
        let err = normalise_country_code("USA").unwrap_err();
        assert_eq!(
            err.to_string(),
            "A country code must be exactly two letters, for example \"US\" or \"GB\""
        );
        assert!(normalise_country_code("U1").is_err());
        assert!(normalise_country_code("").is_err());
    }

    #[test]
    fn postal_keys_drop_punctuation_and_case() {
        assert_eq!(format_postal_code(" sw1a 1aa "), "SW1A 1AA");
        assert_eq!(normalise_postal_code_key("sw1a 1aa"), "SW1A1AA");
        assert_eq!(normalise_postal_code_key("100-0001"), "1000001");
        assert_eq!(normalise_postal_code_key("100"), "100");
        assert_eq!(normalise_postal_code_key("100-"), "100");
        assert_eq!(normalise_postal_code_key("L-1661"), "L1661");
    }

    #[test]
    fn clean_postal_code_bounds_input() {
        assert_eq!(clean_postal_code("  100-0001 ").unwrap(), "100-0001");
        let err = clean_postal_code("   ").unwrap_err();
        assert_eq!(err.to_string(), "A postal code is required");
        let long = "x".repeat(MAX_POSTAL_CODE_LENGTH + 1);
        let err = clean_postal_code(&long).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("A postal code must not exceed {MAX_POSTAL_CODE_LENGTH} characters")
        );
    }

    #[test]
    fn parses_coordinates_and_accuracy() {
        assert_eq!(parse_coordinate(Some("42.5833")), Some(42.5833));
        assert_eq!(parse_coordinate(Some("")), None);
        assert_eq!(parse_coordinate(None), None);
        assert_eq!(parse_coordinate(Some("nope")), None);
        assert_eq!(parse_accuracy(Some("6")), Some(6));
        assert_eq!(parse_accuracy(Some("")), None);
        assert_eq!(parse_accuracy(Some("6.5")), None);
        assert_eq!(text_or_null(Some(" Canillo ")), Some("Canillo".to_string()));
        assert_eq!(text_or_null(Some("  ")), None);
    }

    #[test]
    fn truncation_notes_cover_the_documented_countries() {
        assert!(truncated_note("GB").unwrap().contains("outward code"));
        assert!(truncated_note("CA").unwrap().contains("Forward Sortation Area"));
        assert!(truncated_note("US").is_none());
    }
}
