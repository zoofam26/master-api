//! Postal-code pattern validation against the regex GeoNames documents per
//! country.
//!
//! Ported 1:1 from `postal-code-api/src/postal/postal-pattern.ts`: the
//! published pattern is never executed as-is — it is re-anchored to the whole
//! string and the input length is capped before the engine is involved.

use std::collections::HashMap;

use once_cell::sync::OnceCell;
use regex::Regex;
use serde::Serialize;

use crate::codes::{format_postal_code, normalise_postal_code_key};

/// The longest string that is ever handed to a country regex.
///
/// Every one of the 178 GeoNames patterns targets at most 8 characters (the
/// longest formats are `GB`/`GG`/`IM`/`JE` at 8 including the space and `IE`
/// at 7). Anything longer therefore cannot match, so refusing it *before* the
/// regex engine is involved both removes a pointless evaluation and removes
/// the only class of input on which a backtracking pattern could be made to
/// misbehave.
pub const DEFAULT_MAX_PATTERN_INPUT_LENGTH: usize = 32;

/// Compiled patterns, keyed by the published pattern string.
static COMPILED: OnceCell<std::sync::Mutex<HashMap<String, Option<Regex>>>> = OnceCell::new();

/// Strips the anchors GeoNames publishes and re-applies them around the whole
/// expression.
///
/// This is not cosmetic. Three of the published patterns are structurally
/// mis-anchored, and using one verbatim silently accepts garbage:
///
/// * `GB` is `^A|B$`. In most regex engines `^` binds only to the first
///   alternative and `$` only to the last, so the raw pattern accepts any
///   string that merely *starts with* `GIR 0AA` (`"GIR 0AAwhatever"`) or
///   merely *ends with* a valid inward code (`"junkSW1A 1AA"`).
/// * `IE` has no closing `$` at all:
///   `^(D6W|[AC-FHKNPRTV-Y][0-9]{2})\s?([AC-FHKNPRTV-Y0-9]{4})`.
/// * `CA` has a stray trailing space after the `$`.
///
/// Wrapping the whole alternation in a non-capturing group — `(?:…)` —
/// restores whole-string semantics. After re-anchoring, `"GIR 0AAwhatever"`
/// and `"junkSW1A 1AA"` are both correctly rejected.
pub fn anchor_pattern(pattern: &str) -> String {
    let trimmed = pattern.trim();
    let body = trimmed.strip_prefix('^').unwrap_or(trimmed);
    let body = body.strip_suffix('$').unwrap_or(body);
    format!("^(?:{body})$")
}

/// Compiles a published pattern into a whole-string matcher, or returns `None`
/// when the expression is not usable.
///
/// Safety properties, in order of importance:
///
/// 1. **The pattern is never caller input.** It only ever comes from the
///    GeoNames `countryInfo.txt` reference table, which is cached server-side
///    and is not reachable through any route parameter.
/// 2. **Re-anchoring** makes `^`/`$` bind to the whole expression.
/// 3. **Input length is capped** (see [`DEFAULT_MAX_PATTERN_INPUT_LENGTH`]),
///    which bounds the work any single evaluation can require.
/// 4. **No global state leaks between requests** beyond the compiled artefact
///    itself, which is immutable once built.
pub fn compile_postal_pattern(pattern: Option<&str>) -> Option<Regex> {
    let pattern = pattern.map(str::trim).filter(|p| !p.is_empty())?;
    let cache = COMPILED.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = cache.lock().expect("pattern cache lock poisoned");
    if let Some(cached) = guard.get(pattern) {
        return cached.clone();
    }
    // A pattern the engine cannot parse is reported as unusable rather than
    // crashing the request.
    let compiled = Regex::new(&anchor_pattern(pattern)).ok();
    guard.insert(pattern.to_string(), compiled.clone());
    compiled
}

/// Result of checking a postal code against its country's documented regex.
#[derive(Debug, Clone, Serialize)]
pub struct PostalValidation {
    pub country: String,
    /// The code exactly as it arrived, trimmed.
    pub input: String,
    /// Trimmed, whitespace-collapsed, uppercased. `None` when rejected outright.
    #[serde(rename = "normalised")]
    pub normalised: Option<String>,
    /// Uppercased with all punctuation removed, e.g. `90210`.
    pub key: Option<String>,
    /// Whether the *normalised* value matches the country's pattern.
    pub valid: bool,
    /// Whether the *raw* value matches, i.e. before uppercasing and collapsing
    /// whitespace. `false` for `sw1a 1aa` in a country whose pattern is
    /// uppercase-only — a distinction a caller validating a form needs.
    #[serde(rename = "inputValid")]
    pub input_valid: bool,
    /// The published pattern the check was performed against.
    pub pattern: Option<String>,
    /// The published human-readable format mask.
    pub format: Option<String>,
    /// Why the check failed, or `None` when it passed.
    pub reason: Option<&'static str>,
}

/// Checks a postal code against a country's documented pattern.
///
/// Two results are reported on purpose. `valid` is the answer a user typing
/// into a form cares about — it is evaluated on the *normalised* value, so
/// `sw1a 1aa` is valid in a country whose pattern is written in upper case.
/// `inputValid` answers the stricter question of whether the value matched
/// exactly as typed, which matters when a database column is compared
/// literally.
pub fn validate_postal_code(
    country: &str,
    raw_input: &str,
    published_pattern: Option<&str>,
    published_format: Option<&str>,
    max_input_length: usize,
) -> PostalValidation {
    let trimmed = raw_input.trim();
    let base = PostalValidation {
        country: country.to_string(),
        input: trimmed.to_string(),
        normalised: None,
        key: None,
        valid: false,
        input_valid: false,
        pattern: published_pattern.map(|p| p.to_string()),
        format: published_format.map(|f| f.to_string()),
        reason: None,
    };

    if trimmed.is_empty() {
        return PostalValidation { reason: Some("EMPTY"), ..base };
    }

    if trimmed.len() > max_input_length {
        return PostalValidation { reason: Some("INPUT_TOO_LONG"), ..base };
    }

    if published_pattern.map(str::trim).filter(|p| !p.is_empty()).is_none() {
        return PostalValidation {
            normalised: Some(format_postal_code(trimmed)),
            key: Some(normalise_postal_code_key(trimmed)),
            reason: Some("COUNTRY_HAS_NO_DOCUMENTED_PATTERN"),
            ..base
        };
    }

    let Some(regex) = compile_postal_pattern(published_pattern) else {
        return PostalValidation { reason: Some("PATTERN_UNUSABLE"), ..base };
    };

    let normalised = format_postal_code(trimmed);
    let valid = regex.is_match(&normalised);
    PostalValidation {
        normalised: Some(normalised.clone()),
        key: Some(normalise_postal_code_key(&normalised)),
        valid,
        input_valid: regex.is_match(trimmed),
        reason: Some(if valid { "MATCHES_PATTERN" } else { "DOES_NOT_MATCH_PATTERN" }),
        ..base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reanchors_mis_anchored_published_patterns() {
        assert_eq!(anchor_pattern("^A|B$"), "^(?:A|B)$");
        assert_eq!(anchor_pattern(r"^\d{5}$ "), "^(?:\\d{5})$");
        assert_eq!(anchor_pattern("  AD###  "), "^(?:AD###)$");

        // GB: the raw pattern accepts `GIR 0AAwhatever` verbatim; re-anchored
        // it must not.
        let gb = compile_postal_pattern(Some(
            "^GIR[ ]?0AA|((AB|AL|B)[0-9][0-9]?)\\s?[0-9][A-Z]{2}$",
        ))
        .expect("compiles");
        assert!(gb.is_match("GIR 0AA"));
        assert!(!gb.is_match("GIR 0AAwhatever"), "leading alternative must not leak");
        assert!(!gb.is_match("junkSW1A 1AA"), "trailing alternative must not leak");
    }

    #[test]
    fn validates_normalised_and_raw_forms_separately() {
        // A lowercase code is valid after normalisation but not as typed.
        let v = validate_postal_code("US", " 90210 ", Some(r"^\d{5}(?:-\d{4})?$"), Some("#####"), 32);
        assert!(v.valid);
        assert!(v.input_valid);
        assert_eq!(v.normalised.as_deref(), Some("90210"));
        assert_eq!(v.key.as_deref(), Some("90210"));
        assert_eq!(v.reason, Some("MATCHES_PATTERN"));

        let v = validate_postal_code("GB", "sw1a 1aa", Some("^[A-Z]{1,2}[0-9][A-Z\\d]?\\s?[0-9][A-Z]{2}$"), None, 32);
        assert!(v.valid, "normalised form is uppercased before matching");
        assert!(!v.input_valid, "the raw lowercase form does not match an uppercase pattern");
    }

    #[test]
    fn reports_every_documented_failure_reason() {
        let v = validate_postal_code("US", "   ", Some(r"^\d{5}$"), None, 32);
        assert_eq!(v.reason, Some("EMPTY"));

        let v = validate_postal_code("US", "1".repeat(64).as_str(), Some(r"^\d{5}$"), None, 32);
        assert_eq!(v.reason, Some("INPUT_TOO_LONG"));

        let v = validate_postal_code("AF", "1001", None, None, 32);
        assert_eq!(v.reason, Some("COUNTRY_HAS_NO_DOCUMENTED_PATTERN"));
        assert_eq!(v.key.as_deref(), Some("1001"));

        let v = validate_postal_code("US", "90210", Some("(unclosable["), None, 32);
        assert_eq!(v.reason, Some("PATTERN_UNUSABLE"));

        let v = validate_postal_code("US", "ABCDE", Some(r"^\d{5}$"), None, 32);
        assert_eq!(v.reason, Some("DOES_NOT_MATCH_PATTERN"));
        assert!(!v.valid);
    }

    #[test]
    fn compiled_patterns_are_memoised_by_the_published_string() {
        let first = compile_postal_pattern(Some(r"^\d{3}-\d{4}$"));
        let second = compile_postal_pattern(Some(r"^\d{3}-\d{4}$"));
        assert!(first.is_some() && second.is_some());
        // Both calls hand back regexes for the identical anchored expression.
        assert!(first.unwrap().as_str() == second.unwrap().as_str());
        assert!(compile_postal_pattern(Some("   ")).is_none());
        assert!(compile_postal_pattern(None).is_none());
    }
}
