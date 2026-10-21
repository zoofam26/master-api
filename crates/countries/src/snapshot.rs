//! Snapshot builder: joins the embedded mledoze dataset with GeoNames
//! `countryInfo.txt` and builds every lookup index the read routes need.

use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::OnceCell;
use serde_json::{json, Map, Value};

use master_common::{error::ApiError, httpx, state::AppState};

use crate::model::{normalise_text, Country, Currency, Language, NativeName, CONTINENT_NAMES};
use crate::{CODE_PREFIX, UPSTREAM_COUNTRIES, UPSTREAM_GEONAMES};

/// The read model every route is answered from.
pub struct Snapshot {
    pub countries: Vec<Arc<Country>>,
    pub by_alpha2: HashMap<String, Arc<Country>>,
    pub by_alpha3: HashMap<String, Arc<Country>>,
    pub by_numeric: HashMap<String, Arc<Country>>,
    pub by_ioc: HashMap<String, Arc<Country>>,
    pub by_name: HashMap<String, Arc<Country>>,
    pub neighbours: HashMap<String, Vec<Arc<Country>>>,
    pub regions: Vec<Value>,
    pub subregions: Vec<Value>,
    pub continents: Vec<Value>,
    pub region_names: Vec<String>,
    pub subregion_names: Vec<String>,
    pub continent_names: Vec<String>,
    pub currency_codes: Vec<String>,
    pub language_codes: Vec<String>,
    pub totals: Value,
    pub fetched_at: String,
    pub duration_ms: u64,
    pub sources: Value,
}

static SNAPSHOT_CACHE: OnceCell<moka::future::Cache<String, Arc<Snapshot>>> = OnceCell::new();

fn snapshot_cache() -> &'static moka::future::Cache<String, Arc<Snapshot>> {
    SNAPSHOT_CACHE.get_or_init(|| {
        moka::future::Cache::builder()
            .time_to_live(std::time::Duration::from_secs(24 * 3600))
            .max_capacity(2)
            .name("COUNTRIES_SNAPSHOT")
            .build()
    })
}

/// Returns the current snapshot, refreshing it on a cache miss. Concurrent
/// callers share one refresh (single-flight via moka `try_get_with`).
pub async fn get_snapshot(state: &AppState) -> Result<Arc<Snapshot>, ApiError> {
    snapshot_cache()
        .try_get_with("dataset".to_string(), async move {
            build_snapshot(state).await
        })
        .await
        .map_err(|e: Arc<ApiError>| ApiError::internal(format!("snapshot refresh failed: {e}")))
}

/// Parse the embedded mledoze dataset + fetch GeoNames countryInfo, build indexes.
async fn build_snapshot(state: &AppState) -> Result<Arc<Snapshot>, ApiError> {
    let started = std::time::Instant::now();
    let raw = dataset_raw();
    let records = raw
        .as_array()
        .ok_or_else(|| ApiError::internal("embedded countries dataset is not an array"))?;

    // GeoNames countryInfo: fetch with TTL; a failure degrades the optional
    // fields (population, continent, geonameId) to null instead of failing
    // the whole snapshot, so the core routes stay available.
    let geonames: HashMap<String, GeoRow> = match fetch_geonames(state).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "geonames countryInfo unavailable; continuing without enrichment");
            HashMap::new()
        }
    };

    let mut countries: Vec<Country> = Vec::with_capacity(records.len());
    for record in records {
        countries.push(normalise_country(record, geonames.get(record.get("cca2").and_then(|v| v.as_str()).unwrap_or("").to_uppercase().as_str())));
    }
    countries.sort_by(|a, b| {
        crate::model::compare_text(&a.name_common, &b.name_common)
            .then_with(|| crate::model::compare_text(&a.alpha3, &b.alpha3))
    });

    let mut by_alpha2 = HashMap::new();
    let mut by_alpha3 = HashMap::new();
    let mut by_numeric = HashMap::new();
    let mut by_ioc = HashMap::new();
    let mut by_name = HashMap::new();
    let mut currency_codes = std::collections::BTreeSet::new();
    let mut language_codes = std::collections::BTreeSet::new();

    let arced: Vec<Arc<Country>> = countries.into_iter().map(Arc::new).collect();
    for c in &arced {
        by_alpha2.insert(c.alpha2.clone(), c.clone());
        by_alpha3.insert(c.alpha3.clone(), c.clone());
        if let Some(num) = &c.numeric {
            by_numeric.insert(num.clone(), c.clone());
        }
        if let Some(ioc) = &c.ioc {
            by_ioc.insert(ioc.clone(), c.clone());
        }
        for name in std::iter::once(&c.name_common)
            .chain(std::iter::once(&c.name_official))
            .chain(c.alt_spellings.iter())
        {
            let key = normalise_text(name);
            if !key.is_empty() {
                by_name.entry(key).or_insert_with(|| c.clone());
            }
        }
        for cur in &c.currencies {
            currency_codes.insert(cur.code.to_lowercase());
        }
        for lang in &c.languages {
            language_codes.insert(lang.code.to_lowercase());
        }
    }

    // Neighbour resolution: mledoze spells borders as alpha-3 (alpha-2 fallback).
    let mut neighbours: HashMap<String, Vec<Arc<Country>>> = HashMap::new();
    for c in &arced {
        let mut resolved = Vec::new();
        for code in &c.borders {
            if let Some(n) = by_alpha3.get(code).or_else(|| by_alpha2.get(code)) {
                if n.alpha2 != c.alpha2 {
                    resolved.push(n.clone());
                }
            }
        }
        resolved.sort_by(|a, b| crate::model::compare_text(&a.name_common, &b.name_common));
        neighbours.insert(c.alpha2.clone(), resolved);
    }

    // Aggregates.
    fn group_count<'a, F: Fn(&'a Country) -> Option<&'a str>>(items: &'a [Arc<Country>], key: F) -> Vec<Value> {
        let mut counts: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
        for c in items {
            if let Some(k) = key(c) {
                if !k.is_empty() {
                    *counts.entry(k.to_string()).or_insert(0) += 1;
                }
            }
        }
        counts
            .into_iter()
            .map(|(name, count)| json!({ "name": name, "count": count }))
            .collect()
    }
    let regions = group_count(&arced, |c| Some(c.region.as_str()));
    let subregions = group_count(&arced, |c| c.subregion.as_deref());
    let continents = group_count(&arced, |c| c.continent_name.as_deref());

    let un_members = arced.iter().filter(|c| c.un_member).count();
    let landlocked = arced.iter().filter(|c| c.landlocked).count();
    let totals = json!({
        "countries": arced.len(),
        "unMembers": un_members,
        "landlocked": landlocked,
        "currencies": currency_codes.len(),
        "languages": language_codes.len(),
    });

    let snapshot = Snapshot {
        region_names: regions.iter().filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(String::from)).collect(),
        subregion_names: subregions.iter().filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(String::from)).collect(),
        continent_names: continents.iter().filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(String::from)).collect(),
        currency_codes: currency_codes.into_iter().collect(),
        language_codes: language_codes.into_iter().collect(),
        by_alpha2,
        by_alpha3,
        by_numeric,
        by_ioc,
        by_name,
        neighbours,
        regions,
        subregions,
        continents,
        totals,
        countries: arced,
        fetched_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        duration_ms: started.elapsed().as_millis() as u64,
        sources: json!({ "countries": UPSTREAM_COUNTRIES, "geonames": UPSTREAM_GEONAMES }),
    };
    Ok(Arc::new(snapshot))
}

/// The mledoze dataset is embedded at build time — zero cold-start latency
/// for the primary source.
fn dataset_raw() -> Value {
    const RAW: &str = include_str!("../data_countries.json");
    serde_json::from_str(RAW).expect("embedded countries dataset must be valid JSON")
}

struct GeoRow {
    population: Option<i64>,
    continent_code: Option<String>,
    geoname_id: Option<i64>,
}

async fn fetch_geonames(state: &AppState) -> Result<HashMap<String, GeoRow>, ApiError> {
    let cache = state.json_cache(master_common::state::cache_names::COUNTRIES);
    let cached = cache.get("geonames_countryinfo").await;
    let text: String = match cached {
        Some(v) => v.as_str().unwrap_or("").to_string(),
        None => {
            let (bytes, _ms) = httpx::get_bytes(state, UPSTREAM_GEONAMES, CODE_PREFIX).await?;
            let body = String::from_utf8_lossy(&bytes).to_string();
            cache
                .insert(
                    "geonames_countryinfo".to_string(),
                    Arc::new(Value::String(body.clone())),
                )
                .await;
            body
        }
    };
    Ok(parse_geonames_rows(&text))
}

fn parse_geonames_rows(text: &str) -> HashMap<String, GeoRow> {
    let mut map = HashMap::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 17 || cols[0].is_empty() {
            continue;
        }
        map.insert(
            cols[0].to_uppercase(),
            GeoRow {
                population: cols[7].parse::<i64>().ok(),
                continent_code: Some(cols[8].to_uppercase()).filter(|s| !s.is_empty()),
                geoname_id: cols[16].parse::<i64>().ok(),
            },
        );
    }
    map
}

/// Flatten one mledoze record plus its GeoNames row into a `Country`.
fn normalise_country(record: &Value, geo: Option<&GeoRow>) -> Country {
    let name = record.get("name").cloned().unwrap_or(Value::Null);
    let native = name.get("native").cloned().unwrap_or_else(|| Value::Object(Map::new()));
    let mut native_names = Vec::new();
    if let Some(obj) = native.as_object() {
        for (lang, val) in obj {
            native_names.push(NativeName {
                language: lang.clone(),
                common: val.get("common").and_then(|v| v.as_str()).map(String::from),
                official: val.get("official").and_then(|v| v.as_str()).map(String::from),
            });
        }
    }

    let idd = record.get("idd").cloned().unwrap_or(Value::Null);
    let root = idd.get("root").and_then(|v| v.as_str()).unwrap_or("").trim_matches('+').to_string();
    let suffixes = idd
        .get("suffixes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let calling_codes: Vec<String> = if root.is_empty() {
        suffixes.iter().filter_map(|s| s.as_str()).map(String::from).collect()
    } else {
        suffixes
            .iter()
            .filter_map(|s| s.as_str())
            .map(|s| format!("{root}{s}"))
            .collect()
    };

    let currencies = record
        .get("currencies")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(code, val)| Currency {
                    code: code.clone(),
                    name: val.get("name").and_then(|v| v.as_str()).map(String::from),
                    symbol: val.get("symbol").and_then(|v| v.as_str()).map(String::from),
                })
                .collect()
        })
        .unwrap_or_default();

    let languages = record
        .get("languages")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(code, name)| Language {
                    code: code.clone(),
                    name: name.as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default();

    let latlng = record.get("latlng").and_then(|v| v.as_array()).and_then(|arr| {
        match (arr.first().and_then(|v| v.as_f64()), arr.get(1).and_then(|v| v.as_f64())) {
            (Some(lat), Some(lon)) => Some((lat, lon)),
            _ => None,
        }
    });

    let continent_code = geo.and_then(|g| g.continent_code.clone());
    let continent_name = continent_code.as_deref().and_then(|code| {
        CONTINENT_NAMES
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, name)| name.to_string())
    });

    Country {
        name_common: name.get("common").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        name_official: name.get("official").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        alpha2: record.get("cca2").and_then(|v| v.as_str()).unwrap_or("").to_uppercase(),
        alpha3: record.get("cca3").and_then(|v| v.as_str()).unwrap_or("").to_uppercase(),
        numeric: record
            .get("ccn3")
            .and_then(|v| v.as_str())
            .map(String::from)
            .filter(|s| !s.is_empty()),
        ioc: record
            .get("cioc")
            .and_then(|v| v.as_str())
            .map(String::from)
            .filter(|s| !s.is_empty()),
        capitals: record
            .get("capital")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).map(String::from).collect())
            .unwrap_or_default(),
        alt_spellings: record
            .get("altSpellings")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).map(String::from).collect())
            .unwrap_or_default(),
        region: record.get("region").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        subregion: record
            .get("subregion")
            .and_then(|v| v.as_str())
            .map(String::from)
            .filter(|s| !s.is_empty()),
        continent_code: continent_code.clone(),
        continent_name,
        population: geo.and_then(|g| g.population),
        area: record.get("area").and_then(|v| v.as_f64()),
        landlocked: record.get("landlocked").and_then(|v| v.as_bool()).unwrap_or(false),
        independent: record.get("independent").and_then(|v| v.as_bool()),
        un_member: record.get("unMember").and_then(|v| v.as_bool()).unwrap_or(false),
        status: record.get("status").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        un_regional_group: record
            .get("unRegionalGroup")
            .and_then(|v| v.as_str())
            .map(String::from)
            .filter(|s| !s.is_empty()),
        borders: record
            .get("borders")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).map(String::from).collect())
            .unwrap_or_default(),
        tlds: record
            .get("tld")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).map(String::from).collect())
            .unwrap_or_default(),
        calling_codes,
        currencies,
        languages,
        latlng,
        flag_emoji: record.get("flag").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        geoname_id: geo.and_then(|g| g.geoname_id),
        native_names,
        translations: record
            .get("translations")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default(),
        demonyms: record
            .get("demonyms")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_embeds_250_countries() {
        let raw = dataset_raw();
        assert!(raw.as_array().unwrap().len() >= 250);
    }

    #[test]
    fn parses_austria_from_embedded_dataset() {
        let raw = dataset_raw();
        let at = raw
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r.get("cca2").and_then(|v| v.as_str()) == Some("AT"))
            .expect("Austria present");
        let c = normalise_country(at, None);
        assert_eq!(c.alpha2, "AT");
        assert_eq!(c.alpha3, "AUT");
        assert_eq!(c.name_common, "Austria");
        assert_eq!(c.calling_codes, vec!["43"]);
        assert!(c.currencies.iter().any(|cur| cur.code == "EUR"));
        assert!(c.landlocked, "Austria is landlocked in the dataset");
        assert!(c.native_names.iter().any(|n| n.language == "bar"), "Austria's native name is Bavarian in the dataset");
    }

    #[test]
    fn parses_germany_from_embedded_dataset() {
        let raw = dataset_raw();
        let de = raw
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r.get("cca2").and_then(|v| v.as_str()) == Some("DE"))
            .expect("Germany present");
        let c = normalise_country(de, None);
        assert_eq!(c.alpha2, "DE");
        assert_eq!(c.name_common, "Germany");
        assert_eq!(c.capitals, vec!["Berlin"]);
        // Faithful to the dataset: mledoze marks Germany landlocked = false.
        assert!(!c.landlocked);
    }

    #[test]
    fn parses_geonames_rows() {
        let sample = concat!(
            "#ISO\tISO3\tISO-Numeric\tfips\tCountry\tCapital\tArea\tPopulation\tContinent\t",
            "tld\tCurrencyCode\tCurrencyName\tPhone\tPostalFormat\tPostalRegex\tLanguages\tgeonameid\tneighbours\n",
            "tz\tTZA\t834\tTZ\tTanzania\tDodoma\t947300\t61731329\tAF\t.tz\tTZS\tTanzanian shilling\t255\t\t\tsw,en\t149426\tKE,MZ,ZM\n"
        );
        let rows = parse_geonames_rows(sample);
        let tz = rows.get("TZ").expect("TZ row");
        assert_eq!(tz.population, Some(61_731_329));
        assert_eq!(tz.continent_code.as_deref(), Some("AF"));
        assert_eq!(tz.geoname_id, Some(149_426));
    }
}
