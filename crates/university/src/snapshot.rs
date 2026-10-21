//! Snapshot: parse the Hipo dataset, assign deterministic ids, build indexes.

use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use unicode_normalization::UnicodeNormalization;

use master_common::error::ApiError;
use master_common::httpx;
use master_common::state::AppState;

use crate::{CODE_PREFIX, UPSTREAM_HIPO};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UniversityRecord {
    pub id: String,
    pub name: String,
    #[serde(rename = "countryCode")]
    pub country_code: String,
    pub country: String,
    #[serde(rename = "stateProvince")]
    pub state_province: Option<String>,
    pub domains: Vec<String>,
    #[serde(rename = "webPages")]
    pub web_pages: Vec<String>,
}

pub struct UniversitySnapshot {
    pub records: Vec<Arc<UniversityRecord>>,
    pub by_id: HashMap<String, Arc<UniversityRecord>>,
    pub by_domain: HashMap<String, Vec<Arc<UniversityRecord>>>,
    pub by_name: HashMap<String, Vec<Arc<UniversityRecord>>>,
    pub countries: Vec<CountrySummary>,
    pub country_names: HashMap<String, String>,
    pub fetched_at: String,
    pub record_count: usize,
    pub domain_count: usize,
    pub country_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CountrySummary {
    pub code: String,
    pub name: String,
    #[serde(rename = "universityCount")]
    pub university_count: usize,
}

struct CacheEntry {
    snapshot: Arc<UniversitySnapshot>,
    created: std::time::Instant,
}

static SNAPSHOT_CACHE: OnceCell<moka::future::Cache<String, Arc<CacheEntry>>> = OnceCell::new();

fn snapshot_cache() -> &'static moka::future::Cache<String, Arc<CacheEntry>> {
    SNAPSHOT_CACHE.get_or_init(|| {
        moka::future::Cache::builder()
            .max_capacity(4)
            .name("UNIVERSITY_SNAPSHOT")
            .build()
    })
}

/// Returns the snapshot, downloading and indexing on first use (single-flight).
pub async fn get_snapshot(state: &AppState) -> Result<Arc<UniversitySnapshot>, ApiError> {
    let ttl = crate::snapshot_ttl(state);
    let cache = snapshot_cache();
    if let Some(entry) = cache.get("dataset").await {
        if entry.created.elapsed() < ttl {
            return Ok(entry.snapshot.clone());
        }
    }
    let built = cache
        .try_get_with("dataset".to_string(), async move {
            let started = std::time::Instant::now();
            let fetched = httpx::get_json(state, UPSTREAM_HIPO, &[], CODE_PREFIX).await?;
            let fetched_at =
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            let snapshot = build_snapshot(&fetched.body, fetched_at)
                .map_err(|m| ApiError::upstream(format!("{CODE_PREFIX}_PARSE_ERROR"), m))?;
            tracing::info!(
                records = snapshot.record_count,
                domains = snapshot.domain_count,
                duration_ms = started.elapsed().as_millis() as u64,
                "university dataset indexed"
            );
            Ok::<Arc<CacheEntry>, ApiError>(Arc::new(CacheEntry {
                snapshot: Arc::new(snapshot),
                created: std::time::Instant::now(),
            }))
        })
        .await
        .map_err(|e: Arc<ApiError>| ApiError::internal(format!("{e}")))?;
    Ok(built.snapshot.clone())
}

fn compare_text(a: &str, b: &str) -> std::cmp::Ordering {
    a.cmp(b)
}

/// NFKD-slugify, dropping accents and punctuation.
pub fn slugify(value: &str) -> String {
    let nfkd: String = value.nfkd().collect();
    let stripped: String = nfkd
        .chars()
        .filter(|c| !is_combining_mark(*c))
        .collect();
    let mut slug = String::new();
    let mut prev_dash = false;
    for c in stripped.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() { "university".to_string() } else { trimmed }
}

fn is_combining_mark(c: char) -> bool {
    // Combining diacritical marks blocks left behind by NFKD decomposition.
    matches!(u32::from(c), 0x0300..=0x036F)
}

fn short_hash(value: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(value.as_bytes());
    let out = hasher.finalize();
    out.iter().map(|b| format!("{b:02x}")).collect::<String>()[..6].to_string()
}

struct ParsedRecord {
    university: UniversityRecord,
    base: String,
    canonical_key: String,
}

fn parse_hipo_record(record: &serde_json::Value) -> Option<ParsedRecord> {
    let name = record.get("name").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())?;
    let raw_code = record
        .get("alpha_two_code")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_uppercase())
        .unwrap_or_default();
    if raw_code.len() != 2 || !raw_code.chars().all(|c| c.is_ascii_uppercase()) {
        return None;
    }
    let country = record
        .get("country")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&raw_code)
        .to_string();
    let state_province = record
        .get("state-province")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);

    let mut domains: Vec<String> = record
        .get("domains")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|d| d.as_str())
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    domains.sort();
    domains.dedup();

    let mut web_pages: Vec<String> = record
        .get("web_pages")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|d| d.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    web_pages.sort();
    web_pages.dedup();

    let base = format!("{}-{}", slugify(name), raw_code.to_lowercase());
    let canonical_key = format!("{name}\u{0}{raw_code}\u{0}{}\u{0}{}", domains.join(","), web_pages.join(","));

    Some(ParsedRecord {
        university: UniversityRecord {
            id: String::new(),
            name: name.to_string(),
            country_code: raw_code,
            country,
            state_province,
            domains,
            web_pages,
        },
        base,
        canonical_key,
    })
}

/// Deterministic, order-independent id assignment.
fn assign_ids(records: Vec<ParsedRecord>) -> Vec<UniversityRecord> {
    let mut base_counts: HashMap<String, usize> = HashMap::new();
    for r in &records {
        *base_counts.entry(r.base.clone()).or_insert(0) += 1;
    }
    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(records.len());
    for r in records {
        let unique_base = base_counts.get(r.base.as_str()).copied().unwrap_or(1) == 1;
        let mut id = if unique_base {
            r.base.clone()
        } else {
            format!("{}-{}", r.base, short_hash(&r.canonical_key))
        };
        let mut bump = 2;
        while taken.contains(&id) {
            id = format!("{}-{}", r.base, short_hash(&format!("{}\u{0}#{}", r.canonical_key, bump)));
            bump += 1;
        }
        taken.insert(id.clone());
        let mut u = r.university;
        u.id = id;
        out.push(u);
    }
    out
}

pub fn build_snapshot(payload: &serde_json::Value, fetched_at: String) -> Result<UniversitySnapshot, ApiErrorMessage> {
    let arr = match payload.as_array() {
        Some(a) => a,
        None => return Err("The university dataset is not a JSON array".to_string()),
    };
    if arr.is_empty() {
        return Err("The university dataset is empty".to_string());
    }

    let parsed: Vec<ParsedRecord> = arr.iter().filter_map(parse_hipo_record).collect();
    let universities = assign_ids(parsed);

    let mut records: Vec<Arc<UniversityRecord>> = universities.into_iter().map(Arc::new).collect();
    records.sort_by(|a, b| {
        compare_text(&a.name, &b.name)
            .then_with(|| compare_text(&a.country_code, &b.country_code))
            .then_with(|| compare_text(&a.id, &b.id))
    });

    let mut by_id = HashMap::new();
    let mut by_domain: HashMap<String, Vec<Arc<UniversityRecord>>> = HashMap::new();
    let mut by_name: HashMap<String, Vec<Arc<UniversityRecord>>> = HashMap::new();
    let mut country_names: HashMap<String, String> = HashMap::new();
    let mut country_counts: HashMap<String, usize> = HashMap::new();

    for u in &records {
        by_id.insert(u.id.clone(), u.clone());
        for d in &u.domains {
            by_domain.entry(d.clone()).or_default().push(u.clone());
        }
        by_name.entry(u.name.to_lowercase()).or_default().push(u.clone());
        country_names.insert(u.country_code.clone(), u.country.clone());
        *country_counts.entry(u.country_code.clone()).or_insert(0) += 1;
    }

    let mut countries: Vec<CountrySummary> = country_counts
        .iter()
        .map(|(code, count)| CountrySummary {
            code: code.clone(),
            name: country_names.get(code).cloned().unwrap_or_else(|| code.clone()),
            university_count: *count,
        })
        .collect();
    countries.sort_by(|a, b| compare_text(&a.name, &b.name).then_with(|| compare_text(&a.code, &b.code)));

    Ok(UniversitySnapshot {
        record_count: records.len(),
        domain_count: by_domain.len(),
        country_count: countries.len(),
        countries,
        country_names,
        by_domain,
        by_name,
        by_id,
        records,
        fetched_at,
    })
}

/// Helper alias so the snapshot builder can return plain messages in tests.
pub type ApiErrorMessage = String;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slugifies_names() {
        assert_eq!(slugify("Massachusetts Institute of Technology"), "massachusetts-institute-of-technology");
        assert_eq!(slugify("École Polytechnique"), "ecole-polytechnique");
        assert_eq!(slugify("!!!"), "university");
    }

    #[test]
    fn assigns_ids_deterministically() {
        let payload = json!([
            { "name": "Cornell College", "alpha_two_code": "US", "country": "United States", "domains": ["cornellcollege.edu"], "web_pages": ["https://www.cornellcollege.edu/"] },
            { "name": "Cornell College", "alpha_two_code": "US", "country": "United States", "domains": ["cornell-iowa.edu"], "web_pages": ["https://cornell-iowa.edu/"] },
            { "name": "University of Zurich", "alpha_two_code": "CH", "country": "Switzerland", "domains": ["uzh.ch"], "web_pages": ["https://www.uzh.ch"] }
        ]);
        let snap = build_snapshot(&payload, "2026-01-01T00:00:00.000Z".into()).unwrap();
        assert_eq!(snap.record_count, 3);
        // Unique base slug -> verbatim id; the colliding pair -> content hashes.
        assert_eq!(snap.by_id.get("university-of-zurich-ch").is_some(), true);
        let cornells: Vec<_> = snap.records.iter().filter(|u| u.name == "Cornell College").collect();
        assert_eq!(cornells.len(), 2);
        assert!(cornells.iter().all(|u| u.id.starts_with("cornell-college-us-")));
        assert_ne!(cornells[0].id, cornells[1].id);
    }

    #[test]
    fn skips_malformed_records() {
        let payload = json!([
            { "name": "", "alpha_two_code": "US" },
            { "alpha_two_code": "US" },
            { "name": "X", "alpha_two_code": "usa" },
            { "name": "Ok University", "alpha_two_code": "DE", "domains": ["ok.de"], "web_pages": [] }
        ]);
        let snap = build_snapshot(&payload, "t".into()).unwrap();
        assert_eq!(snap.record_count, 1);
    }

    #[test]
    fn rejects_bad_payloads() {
        assert!(build_snapshot(&json!({}), "t".into()).is_err());
        assert!(build_snapshot(&json!([]), "t".into()).is_err());
    }
}
