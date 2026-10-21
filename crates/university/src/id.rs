//! University domain model, upstream-record parsing and the exact id
//! derivation ported from `university-api/src/universities/parsers/
//! hipolabs.parser.ts` (`slugify`, `parseHipoRecord`, `assignUniversityIds`).
//!
//! Upstream records never leave the parser, so a change to the Hipo schema is
//! contained in one file.

use serde::Serialize;
use serde_json::Value;
use sha1::{Digest, Sha1};
use unicode_normalization::UnicodeNormalization;

use master_common::error::ApiError;

use crate::code;

/// 502 — the upstream answered, but the payload did not match the documented
/// shape (original `ParseError`).
fn parse_error(message: impl Into<String>, details: serde_json::Value) -> ApiError {
    ApiError::Upstream {
        code: code("PARSE_ERROR"),
        message: message.into(),
        details: Some(details),
    }
}

/// The service's own view of a university.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct University {
    /// Stable, deterministic identifier: `<slugified name>-<iso2 country code>`,
    /// with a short content hash appended when that slug is already taken by a
    /// different record.
    pub id: String,
    pub name: String,
    /// ISO 3166-1 alpha-2, e.g. `GB`.
    pub country_code: String,
    /// Country display name, e.g. `United Kingdom`.
    pub country: String,
    /// State, province or region, or `null` when the dataset has no value.
    pub state_province: Option<String>,
    /// Registrable domains, lower-cased, de-duplicated and sorted.
    pub domains: Vec<String>,
    /// Homepage URLs exactly as published upstream.
    pub web_pages: Vec<String>,
}

/// One entry of the countries reference list.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CountrySummary {
    pub code: String,
    pub name: String,
    pub university_count: usize,
}

/// Turns a display name into an id-safe slug, dropping accents and punctuation.
///
/// Port of the original `slugify`: NFKD-decompose, strip the combining marks
/// the decomposition leaves behind, lower-case, map every non-`[a-z0-9]` run
/// to a single `-`, trim, and fall back to `university`.
pub fn slugify(value: &str) -> String {
    let decomposed: String = value.nfkd().collect();
    let stripped: String = decomposed
        .chars()
        .filter(|c| !('\u{0300}'..='\u{036F}').contains(c))
        .collect();
    let lower = stripped.to_lowercase();
    let mut slug = String::with_capacity(lower.len());
    let mut pending_dash = false;
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(c);
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }
    if slug.is_empty() {
        "university".to_string()
    } else {
        slug
    }
}

fn short_hash(value: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex[..6].to_string()
}

/// A record parsed but not yet identified.
///
/// Ids are assigned in a second pass because the obvious
/// `<slugified name>-<country code>` is not unique: the dataset contains 23
/// such collisions (two distinct `Cornell College` records in the US, for
/// example). Deciding inside a single pass would make the result depend on the
/// order the rows happen to arrive in, so the base slug and the content key
/// are both carried out and the decision is made once the whole set is known.
#[derive(Debug, Clone)]
pub struct ParsedRecord {
    /// Everything but the id.
    pub university: UniversityCore,
    /// `<slug>-<cc>`, the readable id when it is unique.
    pub base: String,
    /// Content-addressed, so an id never depends on a record's position.
    pub canonical_key: String,
}

/// A university without its (not yet assigned) id.
#[derive(Debug, Clone)]
pub struct UniversityCore {
    pub name: String,
    pub country_code: String,
    pub country: String,
    pub state_province: Option<String>,
    pub domains: Vec<String>,
    pub web_pages: Vec<String>,
}

fn as_trimmed_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Maps one upstream record onto our own type, minus the id.
///
/// Returns `None` only for records that cannot be identified at all (no name,
/// or a country code that is not two letters). Everything else is coerced and
/// cleaned here so that a slightly odd row in the dump can never take down the
/// whole snapshot.
pub fn parse_hipo_record(record: &Value) -> Option<ParsedRecord> {
    let name = as_trimmed_string(record.get("name"))?;

    let raw_code = as_trimmed_string(record.get("alpha_two_code"))
        .map(|c| c.to_uppercase())
        .unwrap_or_default();
    let bytes = raw_code.as_bytes();
    if bytes.len() != 2 || !bytes.iter().all(|b| b.is_ascii_uppercase()) {
        return None;
    }

    let country =
        as_trimmed_string(record.get("country")).unwrap_or_else(|| raw_code.clone());
    let state_province = as_trimmed_string(record.get("state-province"));

    let empty = Vec::new();
    let domains_raw = record.get("domains").and_then(Value::as_array).unwrap_or(&empty);
    let domains = unique_sorted(
        domains_raw
            .iter()
            .filter_map(|d| as_trimmed_string(d).map(|d| d.to_lowercase()))
            .filter(|d| !d.is_empty())
            .collect(),
    );
    let web_pages_raw = record.get("web_pages").and_then(Value::as_array).unwrap_or(&empty);
    let web_pages = unique_sorted(
        web_pages_raw
            .iter()
            .filter_map(|p| as_trimmed_string(p))
            .filter(|p| !p.is_empty())
            .collect(),
    );

    Some(ParsedRecord {
        base: format!("{}-{}", slugify(&name), raw_code.to_lowercase()),
        canonical_key: format!(
            "{}\u{0}{}\u{0}{}\u{0}{}",
            name,
            raw_code,
            domains.join(","),
            web_pages.join(",")
        ),
        university: UniversityCore {
            name,
            country_code: raw_code,
            country,
            state_province,
            domains,
            web_pages,
        },
    })
}

/// Assigns ids to a whole parsed set.
///
/// Deterministic and order-independent: a base slug used by exactly one record
/// becomes that record's id verbatim, and every record in a colliding group
/// gets a content hash — including the "first" one, so no record's id can
/// change just because the upstream file was re-sorted.
pub fn assign_university_ids(records: Vec<ParsedRecord>) -> Vec<University> {
    let mut base_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for record in &records {
        *base_counts.entry(record.base.clone()).or_insert(0) += 1;
    }

    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    records
        .into_iter()
        .map(|record| {
            let mut id = if base_counts.get(&record.base).copied().unwrap_or(0) == 1 {
                record.base.clone()
            } else {
                format!("{}-{}", record.base, short_hash(&record.canonical_key))
            };
            // Two different records hashing to the same 6 hex digits is not
            // going to happen, but "not going to happen" is not a uniqueness
            // guarantee.
            let mut bump = 2;
            while taken.contains(&id) {
                id = format!(
                    "{}-{}",
                    record.base,
                    short_hash(&format!("{}#{}", record.canonical_key, bump))
                );
                bump += 1;
            }
            taken.insert(id.clone());
            University {
                id,
                name: record.university.name,
                country_code: record.university.country_code,
                country: record.university.country,
                state_province: record.university.state_province,
                domains: record.university.domains,
                web_pages: record.university.web_pages,
            }
        })
        .collect()
}

/// Validates the top-level shape of the upstream payload and coerces every
/// object entry into the raw record shape the parser expects.
///
/// This is the only place a malformed dump is turned into a `ParseError`; the
/// upstream contract is a JSON array of objects, and anything else means we
/// would be serving nonsense.
pub fn assert_hipo_payload(payload: &Value) -> Result<Vec<Value>, ApiError> {
    let Some(entries) = payload.as_array() else {
        return Err(parse_error(
            "The university dataset is not a JSON array",
            serde_json::json!({ "received": if payload.is_array() { "array" } else { "other" } }),
        ));
    };
    if entries.is_empty() {
        return Err(parse_error("The university dataset is empty", serde_json::json!({})));
    }

    let mut records: Vec<Value> = Vec::new();
    let mut junk = 0usize;
    for entry in entries {
        let Some(object) = entry.as_object() else {
            junk += 1;
            continue;
        };
        let domains: Vec<Value> = object
            .get("domains")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter(|v| v.as_str().is_some()).cloned().collect())
            .unwrap_or_default();
        let web_pages: Vec<Value> = object
            .get("web_pages")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter(|v| v.as_str().is_some()).cloned().collect())
            .unwrap_or_default();
        records.push(serde_json::json!({
            "name": object.get("name").and_then(Value::as_str).unwrap_or(""),
            "country": object.get("country").and_then(Value::as_str).unwrap_or(""),
            "alpha_two_code": object.get("alpha_two_code").and_then(Value::as_str).unwrap_or(""),
            "state-province": object.get("state-province").and_then(Value::as_str).map(Value::from).unwrap_or(Value::Null),
            "domains": domains,
            "web_pages": web_pages,
        }));
    }

    if records.is_empty() {
        return Err(parse_error(
            "No usable records in the university dataset",
            serde_json::json!({ "received": entries.len() }),
        ));
    }
    // A handful of unusable rows is tolerable — one bad row must not take down
    // a 10,000-record snapshot — but if the junk outnumbers the good, the dump
    // is not what we think it is.
    if junk > records.len() {
        return Err(parse_error(
            "Most of the university dataset is malformed",
            serde_json::json!({ "parsed": records.len(), "junk": junk, "received": entries.len() }),
        ));
    }
    Ok(records)
}

fn unique_sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slugify_drops_accents_and_punctuation_like_the_original() {
        assert_eq!(slugify("Queen's University Belfast"), "queen-s-university-belfast");
        assert_eq!(slugify("Universität Wien"), "universitat-wien");
        assert_eq!(slugify("Åbo Akademi"), "abo-akademi");
        assert_eq!(slugify("  École Polytechnique  "), "ecole-polytechnique");
        assert_eq!(slugify("---***---"), "university");
        assert_eq!(slugify(""), "university");
        // Runs of separators collapse to one dash.
        assert_eq!(slugify("A -- B"), "a-b");
    }

    #[test]
    fn sha1_matches_the_reference_vectors() {
        // The first six hex digits must match JS `createHash('sha1')` output.
        assert_eq!(short_hash("abc"), "a9993e");
        assert_eq!(short_hash("hello"), "aaf4c6");
        assert_eq!(short_hash(""), "da39a3");
    }

    #[test]
    fn unique_base_becomes_the_id_and_collisions_get_a_content_hash() {
        let a = json!({
            "name": "Cornell College", "country": "United States", "alpha_two_code": "US",
            "state-province": "Iowa",
            "domains": ["cornellcollege.edu"], "web_pages": ["https://www.cornellcollege.edu"]
        });
        let b = json!({
            "name": "Cornell College", "country": "United States", "alpha_two_code": "US",
            "state-province": null,
            "domains": ["cornellcollege.edu"], "web_pages": ["http://www.cornellcollege.edu"]
        });
        let parsed: Vec<ParsedRecord> = [&a, &b].iter().filter_map(|v| parse_hipo_record(v)).collect();
        assert_eq!(parsed.len(), 2);
        let assigned = assign_university_ids(parsed);
        assert_eq!(assigned.len(), 2);
        // Same base slug, but distinct content-hashed ids.
        assert!(assigned[0].id.starts_with("cornell-college-us-"));
        assert!(assigned[1].id.starts_with("cornell-college-us-"));
        assert_ne!(assigned[0].id, assigned[1].id);
    }

    #[test]
    fn ids_are_order_independent() {
        let a = json!({
            "name": "City University", "country": "Bangladesh", "alpha_two_code": "BD",
            "state-province": null, "domains": ["cityuniversity.edu.bd"], "web_pages": []
        });
        let b = json!({
            "name": "City University", "country": "United Kingdom", "alpha_two_code": "GB",
            "state-province": null, "domains": ["city.ac.uk"], "web_pages": []
        });
        let forward: Vec<University> =
            assign_university_ids([&a, &b].iter().filter_map(|v| parse_hipo_record(v)).collect());
        let backward: Vec<University> =
            assign_university_ids([&b, &a].iter().filter_map(|v| parse_hipo_record(v)).collect());
        let mut ids_forward: Vec<&str> = forward.iter().map(|u| u.id.as_str()).collect();
        let mut ids_backward: Vec<&str> = backward.iter().map(|u| u.id.as_str()).collect();
        ids_forward.sort();
        ids_backward.sort();
        assert_eq!(ids_forward, ids_backward);
    }

    #[test]
    fn unusable_records_are_dropped_not_fatal() {
        assert_eq!(parse_hipo_record(&json!({ "name": "  " })), None, "no name");
        assert_eq!(parse_hipo_record(&json!({ "name": "X", "alpha_two_code": "usa" })), None, "bad code");
        let ok = parse_hipo_record(&json!({
            "name": " Technical University of Munich ", "alpha_two_code": "de",
            "country": "  ", "state-province": "  ",
            "domains": ["TUM.de", "tum.de", " "], "web_pages": ["https://www.tum.de"]
        }))
        .expect("usable record");
        assert_eq!(ok.university.name, "Technical University of Munich");
        assert_eq!(ok.university.country_code, "DE");
        assert_eq!(ok.university.country, "DE", "blank country falls back to the code");
        assert_eq!(ok.university.state_province, None);
        assert_eq!(ok.university.domains, vec!["tum.de".to_string()]);
        assert_eq!(ok.university.web_pages, vec!["https://www.tum.de".to_string()]);
    }

    #[test]
    fn payload_validation_mirrors_the_original_thresholds() {
        let err = assert_hipo_payload(&json!({ "not": "an array" })).unwrap_err();
        assert!(err.to_string().contains("not a JSON array"));
        let err = assert_hipo_payload(&json!([])).unwrap_err();
        assert!(err.to_string().contains("empty"));
        // One junk row among good ones is tolerated.
        let ok = assert_hipo_payload(&json!([null, [{ "name": "A", "alpha_two_code": "AA" }]]))
            .expect("one junk row is tolerated");
        assert_eq!(ok.len(), 1);
        // Junk outnumbering good records is a parse error.
        let err = assert_hipo_payload(&json!([1, 2, 3, { "name": "A", "alpha_two_code": "AA" }]))
            .unwrap_err();
        assert!(err.to_string().contains("malformed"));
    }
}
