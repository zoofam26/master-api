//! Downloads, unzips and parses the per-country GeoNames postal datasets
//! (`{ISO}.zip` → `{ISO}.txt`), and caches the parsed snapshots.
//!
//! Ported 1:1 from `postal-code-api/src/postal/client/zip.ts`,
//! `client/postal-dataset.parser.ts` and `postal-dataset.service.ts`: the
//! inflated TSV is kept as one `String` addressed by a table of row offsets
//! (plus the punctuation-free key index), rather than as an array of arrays —
//! for Japan, the largest dataset at ~146k rows / ~12 MB inflated, that keeps
//! the resident footprint at the text plus a small index instead of one
//! allocation per field.

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::sync::Arc;

use once_cell::sync::OnceCell;
use serde::Serialize;
use serde_json::json;

use master_common::error::ApiError;
use master_common::httpx;
use master_common::state::AppState;

use crate::codes::{normalise_postal_code_key, parse_accuracy, parse_coordinate, text_or_null};
use crate::{CODE_PREFIX, UPSTREAM_GEONAMES_ZIP};

/// One record from a GeoNames postal dataset: a code, its places and coordinates.
#[derive(Debug, Clone, Serialize)]
pub struct PostalPlace {
    /// The postal code exactly as published, e.g. `100-0001`.
    #[serde(rename = "postalCode")]
    pub postal_code: String,
    /// Uppercased, punctuation-free form used for matching, e.g. `1000001`.
    #[serde(rename = "postalCodeKey")]
    pub postal_code_key: String,
    #[serde(rename = "placeName")]
    pub place_name: String,
    /// 1st order subdivision, i.e. state / region.
    #[serde(rename = "admin1Name")]
    pub admin1_name: Option<String>,
    #[serde(rename = "admin1Code")]
    pub admin1_code: Option<String>,
    /// 2nd order subdivision, i.e. county / province.
    #[serde(rename = "admin2Name")]
    pub admin2_name: Option<String>,
    #[serde(rename = "admin2Code")]
    pub admin2_code: Option<String>,
    /// 3rd order subdivision, i.e. community.
    #[serde(rename = "communityName")]
    pub community_name: Option<String>,
    #[serde(rename = "communityCode")]
    pub community_code: Option<String>,
    /// WGS84 estimate. `None` when the dataset has no coordinate for the record.
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// GeoNames confidence in the coordinate: `1` estimated, `4` matched to a
    /// geonameid, `6` centroid of addresses or a shape. `None` when unstated.
    pub accuracy: Option<i64>,
}

/// An in-memory snapshot of one country's postal codes.
pub struct CountryDataset {
    pub country: String,
    text: String,
    offsets: Vec<u32>,
    pub row_count: usize,
    /// Normalised postal code → row index of its first occurrence.
    first_by_key: HashMap<String, u32>,
    /// Normalised postal code → any further occurrences.
    extra_by_key: HashMap<String, Vec<u32>>,
    /// Identifies the exact bytes this snapshot came from.
    pub source_url: String,
    pub bytes: usize,
}

/// Column positions in the GeoNames postal dataset, as documented in
/// `readme.txt`. Every row has all 12 fields, though several are frequently
/// the empty string.
const EXPECTED_FIELDS: usize = 12;

impl CountryDataset {
    /// Returns the raw tab-delimited line at `row`.
    ///
    /// `build_country_dataset` strips the file's single trailing newline
    /// before building the offset table, so `text.len()` is exactly the end of
    /// the last row and this slice never picks up a stray terminator.
    pub fn raw_line(&self, row: usize) -> &str {
        let start = self.offsets[row] as usize;
        let end = if row + 1 < self.offsets.len() {
            self.offsets[row + 1] as usize - 1
        } else {
            self.text.len()
        };
        self.text.get(start..end).unwrap_or("")
    }
}

/// Reads every row index belonging to a normalised postal code.
pub fn rows_for_key(dataset: &CountryDataset, key: &str) -> Vec<usize> {
    let first = dataset.first_by_key.get(key);
    let mut rows: Vec<usize> = Vec::new();
    if let Some(first) = first {
        rows.push(*first as usize);
        if let Some(extra) = dataset.extra_by_key.get(key) {
            rows.extend(extra.iter().map(|r| *r as usize));
        }
    }
    rows
}

/// Maps one tab-delimited dataset row onto the service's own place type.
pub fn map_place_row(dataset: &CountryDataset, row: usize) -> Result<PostalPlace, ApiError> {
    let line = dataset.raw_line(row);
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() < EXPECTED_FIELDS {
        return Err(ApiError::Upstream {
            code: "PARSE_ERROR".to_string(),
            message: "Unexpected postal dataset layout".to_string(),
            details: Some(json!({ "country": dataset.country, "fields": fields.len() })),
        });
    }
    Ok(PostalPlace {
        postal_code: fields[1].to_string(),
        postal_code_key: normalise_postal_code_key(fields[1]),
        place_name: fields[2].to_string(),
        admin1_name: text_or_null(Some(fields[3])),
        admin1_code: text_or_null(Some(fields[4])),
        admin2_name: text_or_null(Some(fields[5])),
        admin2_code: text_or_null(Some(fields[6])),
        community_name: text_or_null(Some(fields[7])),
        community_code: text_or_null(Some(fields[8])),
        latitude: parse_coordinate(Some(fields[9])),
        longitude: parse_coordinate(Some(fields[10])),
        accuracy: parse_accuracy(Some(fields[11])),
    })
}

/// Extracts the `{ISO}.txt` entry from the archive bytes.
fn read_zip_entry(country: &str, archive: &[u8]) -> Result<String, ApiError> {
    let parse_error = |message: String, details: serde_json::Value| ApiError::Upstream {
        code: "PARSE_ERROR".to_string(),
        message,
        details: Some(details),
    };

    let mut zip = zip::ZipArchive::new(Cursor::new(archive)).map_err(|e| {
        parse_error(
            "The upstream response is not a ZIP archive".to_string(),
            json!({ "country": country, "cause": e.to_string() }),
        )
    })?;

    // The archive holds `readme.txt` and `{ISO}.txt`; match the entry by name,
    // case-sensitively, exactly like the original `^{ISO}\.txt$` pattern.
    let mut entry_name: Option<String> = None;
    for i in 0..zip.len() {
        let file = zip.by_index_raw(i).map_err(|e| {
            parse_error(
                "The upstream ZIP archive has a malformed central directory".to_string(),
                json!({ "country": country, "cause": e.to_string() }),
            )
        })?;
        if file.name() == format!("{country}.txt") {
            entry_name = Some(file.name().to_string());
            break;
        }
    }

    let name = entry_name.ok_or_else(|| {
        parse_error(
            format!("The upstream archive for {country} has no {country}.txt entry"),
            json!({ "country": country }),
        )
    })?;

    let mut entry = zip.by_name(&name).map_err(|e| {
        parse_error(
            format!("The upstream ZIP entry \"{name}\" could not be opened"),
            json!({ "country": country, "cause": e.to_string() }),
        )
    })?;
    let mut raw = Vec::new();
    entry.read_to_end(&mut raw).map_err(|e| {
        parse_error(
            format!("The upstream ZIP entry \"{name}\" could not be decompressed"),
            json!({ "country": country, "cause": e.to_string() }),
        )
    })?;
    // The original inflates to a UTF-8 string with replacement characters for
    // any stray byte; mirror that instead of failing the whole country.
    Ok(String::from_utf8_lossy(&raw).to_string())
}

/// Builds the searchable snapshot from the raw bytes of a `{ISO}.zip`.
pub fn build_country_dataset(
    country: &str,
    archive: &[u8],
    source_url: &str,
) -> Result<CountryDataset, ApiError> {
    // A ZIP archive starts with the local file header signature "PK\x03\x04";
    // an Apache error page served with a 200 starts with "<!DOCTYPE".
    if archive.len() < 4 || &archive[..4] != b"PK\x03\x04" {
        return Err(ApiError::Upstream {
            code: "PARSE_ERROR".to_string(),
            message: "The upstream data source returned an unexpected payload".to_string(),
            details: Some(json!({ "country": country, "sourceUrl": source_url })),
        });
    }

    let inflated = read_zip_entry(country, archive)?;
    if inflated.contains("<!DOCTYPE html") || inflated.contains("<html") {
        return Err(ApiError::Upstream {
            code: "PARSE_ERROR".to_string(),
            message: format!("The upstream archive for {country} contains an HTML error page"),
            details: Some(json!({ "country": country })),
        });
    }

    // Strip the single trailing line terminator, if any, so that `text.len()`
    // is the end of the last row. Without this, `raw_line` on the final row
    // would slice in a trailing newline and the last column would carry it.
    let bytes = inflated.as_bytes();
    let mut end = inflated.len();
    if end >= 1 && bytes[end - 1] == b'\n' {
        end -= 1;
        if end >= 1 && bytes[end - 1] == b'\r' {
            end -= 1;
        }
    }
    let text = inflated[..end].to_string();

    let rows: Vec<&str> = text.split('\n').collect();
    if rows.iter().all(|row| row.is_empty()) {
        return Err(ApiError::Upstream {
            code: "PARSE_ERROR".to_string(),
            message: format!("The upstream archive for {country} is empty"),
            details: Some(json!({ "country": country })),
        });
    }

    let mut offsets = Vec::with_capacity(rows.len());
    let mut first_by_key: HashMap<String, u32> = HashMap::new();
    let mut extra_by_key: HashMap<String, Vec<u32>> = HashMap::new();

    let mut position: u32 = 0;
    for (row, line) in rows.iter().enumerate() {
        offsets.push(position);
        position += line.len() as u32 + 1;

        if line.is_empty() {
            continue;
        }
        let Some(first_tab) = line.find('\t') else { continue };
        let Some(second_tab) = line[first_tab + 1..].find('\t').map(|i| first_tab + 1 + i)
        else {
            continue;
        };
        // Longer than a postal code can be.
        if second_tab - first_tab - 1 > 24 {
            continue;
        }

        let key = normalise_postal_code_key(&line[first_tab + 1..second_tab]);
        if key.is_empty() {
            continue;
        }

        match first_by_key.get(&key) {
            Some(_) => {
                // The file is not reliably sorted and rows sharing a code are
                // not reliably contiguous, so every occurrence is indexed.
                extra_by_key.entry(key).or_default().push(row as u32);
            }
            None => {
                first_by_key.insert(key, row as u32);
            }
        }
    }

    Ok(CountryDataset {
        country: country.to_string(),
        text,
        offsets,
        row_count: rows.len(),
        first_by_key,
        extra_by_key,
        source_url: source_url.to_string(),
        bytes: archive.len(),
    })
}

/* ---------------------------------------------------------------- handle */

/// A dataset plus how it was obtained, so responses can report honestly.
pub struct DatasetHandle {
    pub dataset: Arc<CountryDataset>,
    /// True when this call had to download and parse the country.
    pub downloaded: bool,
    /// Wall time of the download, or 0 on a cache hit.
    pub load_ms: u64,
}

static DATASET_CACHE: OnceCell<moka::future::Cache<String, Arc<CountryDataset>>> = OnceCell::new();

fn dataset_cache(
    state: &AppState,
) -> &'static moka::future::Cache<String, Arc<CountryDataset>> {
    DATASET_CACHE.get_or_init(|| {
        moka::future::Cache::builder()
            .time_to_live(state.settings.feed_cache_ttl)
            // Each resident country costs roughly 5–40 MB, so the default is
            // deliberately small; the working set of most clients is one or
            // two countries, and an evicted country costs one download.
            .max_capacity(3)
            .name("POSTAL_DATASETS")
            .build()
    })
}

/// The URL of one country dataset, honouring the `UPSTREAM_GEONAMES_ZIP` alias.
pub fn dataset_url(state: &AppState, country: &str) -> String {
    let base = state
        .settings
        .upstream("GEONAMES_ZIP", UPSTREAM_GEONAMES_ZIP)
        .trim_end_matches('/')
        .to_string();
    format!("{base}/{country}.zip")
}

/// Returns the dataset for `country`, downloading it if it is not cached.
///
/// The caller is responsible for having already checked the reference table;
/// this method only builds the URL. Concurrent requests for the same country
/// share one download (single-flight via moka `try_get_with`).
pub async fn get_dataset(state: &AppState, country: &str) -> Result<DatasetHandle, ApiError> {
    let cache = dataset_cache(state);
    if let Some(dataset) = cache.get(country).await {
        return Ok(DatasetHandle { dataset, downloaded: false, load_ms: 0 });
    }

    let started = std::time::Instant::now();
    let owned = country.to_string();
    let dataset = cache
        .try_get_with(owned.clone(), async move {
            download(state, &owned).await
        })
        .await
        .map_err(|e: Arc<ApiError>| ApiError::internal(format!("dataset refresh failed: {e}")))?;
    Ok(DatasetHandle {
        dataset,
        downloaded: true,
        load_ms: started.elapsed().as_millis() as u64,
    })
}

async fn download(state: &AppState, country: &str) -> Result<Arc<CountryDataset>, ApiError> {
    let url = dataset_url(state, country);
    let (bytes, elapsed_ms) = httpx::get_bytes(state, &url, CODE_PREFIX).await?;
    let dataset = build_country_dataset(country, &bytes, &url)?;
    tracing::info!(
        country,
        rows = dataset.row_count,
        kB = bytes.len() / 1024,
        elapsed_ms,
        "Loaded postal dataset"
    );
    Ok(Arc::new(dataset))
}

/// Row count of a resident dataset, or `None` when it is not in memory.
pub async fn resident_row_count(state: &AppState, country: &str) -> Option<usize> {
    dataset_cache(state).get(country).await.map(|d| d.row_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Builds an in-memory ZIP with the same layout GeoNames publishes
    /// (`readme.txt` + `{ISO}.txt`), so the parser is exercised end-to-end.
    fn fixture_zip(country: &str, body: &str) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("readme.txt", options).unwrap();
            zip.write_all(b"GeoNames postal data\r\n").unwrap();
            zip.start_file(format!("{country}.txt"), options).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    const FIXTURE_ROWS: &str = concat!(
        "AD\tAD100\tCanillo\tCanillo\t02\t\t\t\t\t42.5833\t1.6667\t6\n",
        "AD\tAD100\tCanillo\tCanillo\t02\t\t\t\t\t42.5\t1.5\t4\n",
        "AD\tAD101\tEncamp\tEncamp\t03\t\t\t\t\t42.5333\t1.5833\t1\n",
        "AD\tAD102\t!\t!\t04\t\t\t\t\t\t\t\n",
    );

    #[test]
    fn builds_the_dataset_and_indexes_every_occurrence_of_a_key() {
        let zip = fixture_zip("AD", FIXTURE_ROWS);
        let dataset = build_country_dataset("AD", &zip, "https://example.test/AD.zip").unwrap();

        assert_eq!(dataset.row_count, 4);
        assert_eq!(dataset.source_url, "https://example.test/AD.zip");
        assert_eq!(dataset.bytes, zip.len());

        // AD100 appears twice and the rows are not guaranteed contiguous, so
        // both occurrences must be indexed.
        let rows = rows_for_key(&dataset, "AD100");
        assert_eq!(rows, vec![0, 1]);
        assert_eq!(rows_for_key(&dataset, "AD101"), vec![2]);
        assert!(rows_for_key(&dataset, "ad100").is_empty(), "keys are uppercase");

        let place = map_place_row(&dataset, 0).unwrap();
        assert_eq!(place.postal_code, "AD100");
        assert_eq!(place.postal_code_key, "AD100");
        assert_eq!(place.place_name, "Canillo");
        assert_eq!(place.admin1_name.as_deref(), Some("Canillo"));
        assert_eq!(place.admin1_code.as_deref(), Some("02"));
        assert!(place.admin2_name.is_none());
        assert_eq!(place.latitude, Some(42.5833));
        assert_eq!(place.longitude, Some(1.6667));
        assert_eq!(place.accuracy, Some(6));
    }

    #[test]
    fn the_last_row_is_not_polluted_by_the_trailing_newline() {
        let zip = fixture_zip("AD", FIXTURE_ROWS);
        let dataset = build_country_dataset("AD", &zip, "https://example.test/AD.zip").unwrap();
        let place = map_place_row(&dataset, 3).unwrap();
        assert_eq!(place.place_name, "!");
        assert!(place.latitude.is_none(), "empty coordinate columns parse as null");
        assert!(place.accuracy.is_none());
        assert_eq!(dataset.raw_line(3), "AD\tAD102\t!\t!\t04\t\t\t\t\t\t\t");
    }

    #[test]
    fn rejects_non_zip_payloads_and_missing_entries() {
        let html = b"<!DOCTYPE html><html><body>Not Found</body></html>";
        let err = build_country_dataset("AD", html, "https://example.test/AD.zip").unwrap_err();
        assert_eq!(
            err.to_string(),
            "The upstream data source returned an unexpected payload"
        );

        let zip = fixture_zip("AD", FIXTURE_ROWS);
        let err = build_country_dataset("XX", &zip, "https://example.test/XX.zip").unwrap_err();
        assert_eq!(err.to_string(), "The upstream archive for XX has no XX.txt entry");
    }

    #[test]
    fn normalised_keys_match_hyphenated_and_spaced_input() {
        let zip = fixture_zip("JP", "JP\t100-0001\tChiyoda\tTokyo\t13\t\t\t\t\t35.6833\t139.75\t4\n");
        let dataset = build_country_dataset("JP", &zip, "https://example.test/JP.zip").unwrap();
        assert_eq!(rows_for_key(&dataset, "1000001").len(), 1);
        assert!(rows_for_key(&dataset, "9999999").is_empty());
        let place = map_place_row(&dataset, 0).unwrap();
        assert_eq!(place.postal_code, "100-0001");
        assert_eq!(place.postal_code_key, "1000001");
    }
}
