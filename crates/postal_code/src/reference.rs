//! The GeoNames country reference table (`countryInfo.txt`) and the ZIP
//! directory listing that says which countries publish a postal dataset at
//! all, joined into one immutable snapshot.
//!
//! Ported 1:1 from `postal-code-api/src/postal/client/country-info.parser.ts`,
//! `client/dataset-index.parser.ts` and `country-reference.service.ts`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use once_cell::sync::OnceCell;
use serde::Serialize;
use serde_json::{json, Value};

use master_common::error::ApiError;
use master_common::httpx;
use master_common::state::{cache_names, AppState};

use crate::codes::text_or_null;
use crate::{CODE_PREFIX, UPSTREAM_GEONAMES, UPSTREAM_GEONAMES_ZIP};

/// One country as documented by GeoNames, with its postal-code format rules.
#[derive(Debug, Clone, Serialize)]
pub struct CountryReference {
    /// ISO 3166-1 alpha-2, e.g. `US`.
    pub code: String,
    /// ISO 3166-1 alpha-3, e.g. `USA`.
    pub iso3: String,
    pub name: String,
    pub capital: Option<String>,
    pub continent: Option<String>,
    /// ISO 4217 alphabetic code, e.g. `USD`.
    #[serde(rename = "currencyCode")]
    pub currency_code: Option<String>,
    #[serde(rename = "currencyName")]
    pub currency_name: Option<String>,
    /// Human-readable postal-code mask using `#` for a digit and `@` for a
    /// letter, e.g. `#####-####` for the United States. `None` when GeoNames
    /// documents no format for the country.
    #[serde(rename = "postalFormat")]
    pub postal_format: Option<String>,
    /// The regular expression GeoNames documents for this country's postal
    /// codes, exactly as published. It is stored verbatim and never executed
    /// as-is: see `pattern.rs` for the re-anchoring and the safety cap.
    #[serde(rename = "postalPattern")]
    pub postal_pattern: Option<String>,
    /// Whether GeoNames publishes a downloadable postal dataset for the country.
    #[serde(rename = "datasetAvailable")]
    pub dataset_available: bool,
}

/// A snapshot of both reference documents, plus the URLs they came from.
pub struct ReferenceSnapshot {
    pub countries: HashMap<String, CountryReference>,
    pub loaded_at: String,
    pub reference_url: String,
    pub index_url: String,
    /// Number of countries GeoNames publishes a postal dataset for.
    pub dataset_countries: usize,
}

/* --------------------------------------------------------- countryInfo.txt */

/// Column positions in `countryInfo.txt`, per the GeoNames documentation of
/// the file. Every data row has exactly 19 tab-separated fields; the `#` lines
/// at the top of the file are comments.
const COL_ISO: usize = 0;
const COL_ISO3: usize = 1;
const COL_NAME: usize = 4;
const COL_CAPITAL: usize = 5;
const COL_CONTINENT: usize = 8;
const COL_CURRENCY_CODE: usize = 10;
const COL_CURRENCY_NAME: usize = 11;
const COL_POSTAL_FORMAT: usize = 13;
const COL_POSTAL_PATTERN: usize = 14;
const EXPECTED_FIELDS: usize = 19;

/// Parses the GeoNames country reference table.
///
/// This is the only source of the per-country postal **format mask** and
/// **regular expression** the service exposes, and of the ISO 4217 currency
/// code. Patterns are carried through verbatim — re-anchoring and the safety
/// cap are the concern of `pattern.rs`, not of this parser.
pub fn parse_country_info(body: &str) -> Result<HashMap<String, CountryReference>, ApiError> {
    if body.contains("<!DOCTYPE html") || body.contains("<html") {
        return Err(ApiError::Upstream {
            code: "PARSE_ERROR".to_string(),
            message: "The upstream country reference table is an HTML error page".to_string(),
            details: None,
        });
    }

    // `countryInfo.txt` has no header row: it opens with `#` comment lines and
    // then tab-separated data. Comment lines are exempt from the 19-field
    // layout check, exactly like the original parser.
    let data = body
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");

    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        // GeoNames publishes raw, unquoted TSV: a `"` in a name is data.
        .quoting(false)
        .has_headers(false)
        .flexible(true)
        .from_reader(data.as_bytes());

    let mut by_code = HashMap::new();
    for record in reader.records() {
        let fields = record.map_err(|e| {
            ApiError::Upstream {
                code: "PARSE_ERROR".to_string(),
                message: "The upstream country reference table could not be parsed".to_string(),
                details: Some(json!({ "cause": e.to_string() })),
            }
        })?;
        if fields.len() != EXPECTED_FIELDS {
            return Err(ApiError::Upstream {
                code: "PARSE_ERROR".to_string(),
                message: format!(
                    "Unexpected country reference layout: {} fields, expected {EXPECTED_FIELDS}",
                    fields.len()
                ),
                details: Some(json!({
                    "sample": fields.iter().take(6).collect::<Vec<_>>().join("\t"),
                })),
            });
        }

        let code = fields[COL_ISO].trim().to_uppercase();
        let name = fields[COL_NAME].trim().to_string();
        if code.len() != 2
            || !code.bytes().all(|b| b.is_ascii_uppercase())
            || name.is_empty()
        {
            continue;
        }

        by_code.insert(
            code.clone(),
            CountryReference {
                code,
                iso3: fields[COL_ISO3].trim().to_string(),
                name,
                capital: text_or_null(Some(fields[COL_CAPITAL].as_str())),
                continent: text_or_null(Some(fields[COL_CONTINENT].as_str())),
                currency_code: text_or_null(Some(fields[COL_CURRENCY_CODE].as_str())),
                currency_name: text_or_null(Some(fields[COL_CURRENCY_NAME].as_str())),
                postal_format: text_or_null(Some(fields[COL_POSTAL_FORMAT].as_str())),
                postal_pattern: text_or_null(Some(fields[COL_POSTAL_PATTERN].as_str())),
                dataset_available: false,
            },
        );
    }

    if by_code.is_empty() {
        return Err(ApiError::Upstream {
            code: "PARSE_ERROR".to_string(),
            message: "The upstream country reference table contained no usable rows"
                .to_string(),
            details: None,
        });
    }
    Ok(by_code)
}

/* ----------------------------------------------------------- ZIP index */

/// Extracts the set of countries GeoNames actually publishes a postal dataset
/// for, from the Apache directory listing at `https://download.geonames.org/export/zip/`.
///
/// Deriving this from the listing rather than hardcoding a list means the
/// supported set cannot drift from the upstream: a country added to GeoNames
/// appears here on the next cache refresh, and one withdrawn disappears.
///
/// `allCountries.zip` and the `CA_full` / `NL_full` / `GB_full` variants are
/// excluded by the strict `{ISO}.zip` shape: the first is the whole world in
/// one file, the others are the untruncated forms of three countries whose
/// standard subsets this service uses.
pub fn parse_dataset_index(html: &str) -> Result<HashSet<String>, ApiError> {
    if html.contains("Not Found") && !html.contains(".zip") {
        return Err(ApiError::Upstream {
            code: "PARSE_ERROR".to_string(),
            message: "The upstream dataset index could not be read".to_string(),
            details: None,
        });
    }

    let mut codes: HashSet<String> = HashSet::new();
    let mut rest = html;
    while let Some(pos) = rest.find("href=\"") {
        let after = &rest[pos + 6..];
        let bytes = after.as_bytes();
        if bytes.len() >= 7
            && bytes[0].is_ascii_uppercase()
            && bytes[1].is_ascii_uppercase()
            && bytes[2] == b'.'
            && bytes[3] == b'z'
            && bytes[4] == b'i'
            && bytes[5] == b'p'
            && bytes[6] == b'"'
        {
            codes.insert(after[..2].to_string());
        }
        rest = after;
    }

    if codes.is_empty() {
        return Err(ApiError::Upstream {
            code: "PARSE_ERROR".to_string(),
            message: "The upstream dataset index listed no country datasets".to_string(),
            details: None,
        });
    }
    Ok(codes)
}

/* ------------------------------------------------------------- snapshot */

static REFERENCE_CACHE: OnceCell<moka::future::Cache<String, Arc<ReferenceSnapshot>>> =
    OnceCell::new();

fn reference_cache(state: &AppState) -> &'static moka::future::Cache<String, Arc<ReferenceSnapshot>> {
    REFERENCE_CACHE.get_or_init(|| {
        // Reference data is regenerated by GeoNames a few times a year; the
        // feed TTL keeps it fresh at the deployment's chosen cadence.
        moka::future::Cache::builder()
            .time_to_live(state.settings.feed_cache_ttl)
            .max_capacity(2)
            .name("POSTAL_REFERENCE")
            .build()
    })
}

/// Fetches a text document through the POSTAL_CODE JSON cache (descriptive key,
/// single-flight) so a snapshot rebuild inside the TTL window re-uses the raw
/// bytes instead of re-downloading.
async fn fetch_text_cached(
    state: &AppState,
    key: &str,
    url: &str,
) -> Result<String, ApiError> {
    let cache = state.json_cache(cache_names::POSTAL_CODE);
    let owned_key = key.to_string();
    let owned_url = url.to_string();
    let value = cache
        .try_get_with(owned_key, async move {
            let (bytes, _ms) = httpx::get_bytes(state, &owned_url, CODE_PREFIX).await?;
            Ok::<Value, ApiError>(serde_json::Value::String(
                String::from_utf8_lossy(&bytes).to_string(),
            ))
        })
        .await
        .map_err(|e: Arc<ApiError>| ApiError::internal(format!("{e}")))?;
    Ok(value.as_str().unwrap_or("").to_string())
}

/// Returns the current reference snapshot, refreshing it on a cache miss.
/// Concurrent callers share one refresh (single-flight via moka `try_get_with`).
pub async fn get_snapshot(state: &AppState) -> Result<Arc<ReferenceSnapshot>, ApiError> {
    reference_cache(state)
        .try_get_with("reference".to_string(), async move {
            build_snapshot(state).await
        })
        .await
        .map_err(|e: Arc<ApiError>| ApiError::internal(format!("reference refresh failed: {e}")))
}

async fn build_snapshot(state: &AppState) -> Result<Arc<ReferenceSnapshot>, ApiError> {
    let reference_url = state.settings.upstream("GEONAMES", UPSTREAM_GEONAMES);
    // The index is the Apache listing of the ZIP directory.
    let index_base = state
        .settings
        .upstream("GEONAMES_ZIP", UPSTREAM_GEONAMES_ZIP)
        .trim_end_matches('/')
        .to_string();
    let index_url = format!("{index_base}/");

    let reference_body = fetch_text_cached(state, "geonames_countryinfo", &reference_url).await?;
    let index_body = fetch_text_cached(state, "geonames_zip_index", &index_url).await?;

    let mut countries = parse_country_info(&reference_body)?;
    let with_datasets = parse_dataset_index(&index_body)?;

    for (code, country) in countries.iter_mut() {
        country.dataset_available = with_datasets.contains(code);
    }

    tracing::info!(
        countries = countries.len(),
        with_datasets = with_datasets.len(),
        "Loaded GeoNames reference"
    );

    Ok(Arc::new(ReferenceSnapshot {
        dataset_countries: with_datasets.len(),
        countries,
        loaded_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        reference_url,
        index_url,
    }))
}

/// Resolves a validated country code to its reference record.
///
/// Two distinct failures are raised on purpose, because a caller can act on
/// them differently: a code GeoNames has never documented, versus a code it
/// documents but for which it publishes no postal dataset.
pub async fn require_country(state: &AppState, code: &str) -> Result<CountryReference, ApiError> {
    let snapshot = get_snapshot(state).await?;
    snapshot.countries.get(code).cloned().ok_or_else(|| {
        ApiError::not_found_details(
            "COUNTRY_NOT_SUPPORTED",
            format!("GeoNames documents no postal-code format for country \"{code}\""),
            json!({ "country": code, "documentedCountries": snapshot.countries.len() }),
        )
    })
}

/// Same as [`require_country`] but also requires a downloadable dataset.
pub async fn require_country_with_dataset(
    state: &AppState,
    code: &str,
) -> Result<CountryReference, ApiError> {
    let country = require_country(state, code).await?;
    if !country.dataset_available {
        return Err(ApiError::not_found_details(
            "COUNTRY_DATASET_UNAVAILABLE",
            format!("GeoNames publishes no postal-code dataset for country \"{}\"", country.code),
            json!({ "country": country.code, "name": country.name }),
        ));
    }
    Ok(country)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_COUNTRY_INFO: &str = concat!(
        "#ISO\tISO3\tISO-Numeric\tfips\tCountry\tCapital\tArea\tPopulation\tContinent\t",
        "tld\tCurrencyCode\tCurrencyName\tPhone\tPostal Format\tPostal Regex\tLanguages\t",
        "geonameid\tneighbours\tEquivalentFipsCode\r\n",
        "AD\tAND\t020\tAN\tAndorra\tAndorra la Vella\t468\t82904\tEU\t.ad\tEUR\tEuro\t376\t",
        "AD###\t^(?:AD)*(\\d{3})$\tca\t3041565\tES,FR\t\r\n",
        "AF\tAFG\t004\tAF\tAfghanistan\tKabul\t647500\t43844111\tAS\t.af\tAFN\tAfghani\t93\t",
        "\t\tfa-AF,ps,uz-AF,tk\t1149361\tTM,CN,IR,TJ,PK,UZ\t\r\n",
    );

    #[test]
    fn parses_country_info_rows_with_the_documented_layout() {
        let countries = parse_country_info(SAMPLE_COUNTRY_INFO).expect("parses");
        assert_eq!(countries.len(), 2);
        let ad = countries.get("AD").expect("Andorra");
        assert_eq!(ad.iso3, "AND");
        assert_eq!(ad.name, "Andorra");
        assert_eq!(ad.capital.as_deref(), Some("Andorra la Vella"));
        assert_eq!(ad.continent.as_deref(), Some("EU"));
        assert_eq!(ad.currency_code.as_deref(), Some("EUR"));
        assert_eq!(ad.postal_format.as_deref(), Some("AD###"));
        assert_eq!(ad.postal_pattern.as_deref(), Some("^(?:AD)*(\\d{3})$"));
        assert!(!ad.dataset_available, "the index join happens later");
        // AF documents no postal format: both columns are empty.
        let af = countries.get("AF").expect("Afghanistan");
        assert!(af.postal_format.is_none());
        assert!(af.postal_pattern.is_none());
    }

    #[test]
    fn rejects_an_html_error_page_and_a_broken_layout() {
        let err = parse_country_info("<html><body>oops</body></html>").unwrap_err();
        assert_eq!(
            err.to_string(),
            "The upstream country reference table is an HTML error page"
        );
        let err = parse_country_info("XX\tXXX\t1\tX\tName\tCap\t1\t2\tEU\t.xx\tX\tY\t1\tF\tR\tlang\t1\tN").unwrap_err();
        assert!(err.to_string().contains("Unexpected country reference layout: 18 fields"));
    }

    #[test]
    fn extracts_two_letter_datasets_from_the_zip_listing() {
        let html = concat!(
            "<html><body><table>",
            "<tr><td align=\"right\">2025-01-05 12:00</td><td><a href=\"AD.zip\">AD.zip</a></td></tr>",
            "<tr><td align=\"right\">2025-01-05 12:00</td><td><a href=\"GB_full.csv.zip\">GB_full.csv.zip</a></td></tr>",
            "<tr><td align=\"right\">2025-01-05 12:00</td><td><a href=\"allCountries.zip\">allCountries.zip</a></td></tr>",
            "<tr><td align=\"right\">2025-01-05 12:00</td><td><a href=\"US.zip\">US.zip</a></td></tr>",
            "<tr><td align=\"right\">2025-01-05 12:00</td><td><a href=\"readme.txt\">readme.txt</a></td></tr>",
            "</table></body></html>",
        );
        let codes = parse_dataset_index(html).expect("parses");
        assert!(codes.contains("AD"));
        assert!(codes.contains("US"));
        // `GB_full.csv.zip` and `allCountries.zip` are not `{ISO}.zip` entries.
        assert_eq!(codes.len(), 2);
    }

    #[test]
    fn rejects_an_index_with_no_datasets() {
        let err = parse_dataset_index("<html><body>Not Found</body></html>").unwrap_err();
        assert_eq!(err.to_string(), "The upstream dataset index could not be read");
        let err = parse_dataset_index("<html><body>empty</body></html>").unwrap_err();
        assert_eq!(err.to_string(), "The upstream dataset index listed no country datasets");
    }
}
