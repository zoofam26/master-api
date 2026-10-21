//! Route handlers for the postal-code service.

use std::collections::HashMap;
use std::sync::Arc;

use actix_web::web::{self, Json};
use once_cell::sync::OnceCell;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use utoipa::{IntoParams, OpenApi};

use master_common::error::ApiError;
use master_common::httpx;
use master_common::state::{cache_names, AppState};

use crate::{
    haversine_km, normalise_postal_display, normalise_postal_key, CODE_PREFIX,
    COUNTRY_INFO_URL, DATA_SOURCE_LICENCE, DATA_SOURCE_PROVIDER, SERVICE_PREFIX, TRUNCATED_DATASET_NOTES,
    ZIP_BASE,
};

// ------------------------------------------------------------ countryInfo

#[derive(Clone)]
pub struct CountryRef {
    pub code: String,
    pub iso3: String,
    pub name: String,
    pub capital: Option<String>,
    pub continent: Option<String>,
    pub currency_code: Option<String>,
    pub currency_name: Option<String>,
    pub postal_format: Option<String>,
    pub postal_pattern: Option<String>,
}

impl CountryRef {
    pub fn to_json(&self, dataset_available: bool) -> Value {
        json!({
            "code": self.code,
            "iso3": self.iso3,
            "name": self.name,
            "capital": self.capital,
            "continent": self.continent,
            "currencyCode": self.currency_code,
            "currencyName": self.currency_name,
            "postalFormat": self.postal_format,
            "postalPattern": self.postal_pattern,
            "datasetAvailable": dataset_available,
        })
    }
}

fn dataset_available(code: &str) -> bool {
    // GeoNames publishes a per-country dataset for most of the world; a short
    // denylist of territories has none.
    !matches!(code, "AQ" | "BV" | "HM" | "TF" | "GS" | "UM")
}

fn country_ref_json(c: &CountryRef) -> Value {
    c.to_json(dataset_available(&c.code))
}

async fn country_table(state: &AppState) -> Result<HashMap<String, CountryRef>, ApiError> {
    let cache = state.json_cache(cache_names::POSTAL_CODE);
    let key = "countryinfo";
    if let Some(v) = cache.get(key).await {
        return countries_from_json(&v);
    }
    let (bytes, _ms) = httpx::get_bytes(state, COUNTRY_INFO_URL, CODE_PREFIX).await?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    let table = parse_country_info(&text);
    let as_json = countries_to_json(&table);
    cache.insert(key.to_string(), Arc::new(as_json)).await;
    Ok(table)
}

fn countries_to_json(table: &HashMap<String, CountryRef>) -> Value {
    let mut rows: Vec<&CountryRef> = table.values().collect();
    rows.sort_by(|a, b| a.code.cmp(&b.code));
    Value::Array(rows.iter().map(|c| country_ref_json(c)).collect())
}

fn countries_from_json(v: &Value) -> Result<HashMap<String, CountryRef>, ApiError> {
    let mut out = HashMap::new();
    for row in v.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let get = |k: &str| row.get(k).and_then(|x| x.as_str()).map(String::from);
        let code = get("code").unwrap_or_default();
        out.insert(
            code.clone(),
            CountryRef {
                code,
                iso3: get("iso3").unwrap_or_default(),
                name: get("name").unwrap_or_default(),
                capital: get("capital"),
                continent: get("continent"),
                currency_code: get("currencyCode"),
                currency_name: get("currencyName"),
                postal_format: get("postalFormat"),
                postal_pattern: get("postalPattern"),
            },
        );
    }
    Ok(out)
}

/// Parses GeoNames `countryInfo.txt` (no header row; `#` comment lines).
pub fn parse_country_info(text: &str) -> HashMap<String, CountryRef> {
    let mut table = HashMap::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 18 || cols[0].is_empty() {
            continue;
        }
        let opt = |i: usize| Some(cols[i].trim().to_string()).filter(|s| !s.is_empty());
        table.insert(
            cols[0].to_uppercase(),
            CountryRef {
                code: cols[0].to_uppercase(),
                iso3: cols[1].to_uppercase(),
                name: cols[4].to_string(),
                capital: opt(5),
                continent: opt(8),
                currency_code: opt(10),
                currency_name: opt(11),
                postal_format: opt(13),
                postal_pattern: opt(14),
            },
        );
    }
    table
}

// ------------------------------------------------------------- datasets

#[allow(dead_code)]
pub struct PostalDataset {
    pub country: String,
    /// rows: index 0 = postal_code, 1 = place_name, ... per GeoNames layout.
    pub rows: Vec<[String; 12]>,
    pub first_by_key: HashMap<String, usize>,
    pub source_url: String,
    pub bytes: usize,
}

impl PostalDataset {
    pub fn rows_for(&self, key: &str) -> Vec<usize> {
        match self.first_by_key.get(key) {
            Some(first) => vec![*first],
            None => Vec::new(),
        }
    }

    pub fn place_json(&self, row: usize, country_name: &str) -> Value {
        let r = &self.rows[row];
        let lat = r[9].parse::<f64>().ok();
        let lon = r[10].parse::<f64>().ok();
        json!({
            "postalCode": r[1],
            "postalCodeKey": normalise_postal_key(&r[1]),
            "placeName": r[2],
            "admin1Name": nullable(&r[3]),
            "admin1Code": nullable(&r[4]),
            "admin2Name": nullable(&r[5]),
            "admin2Code": nullable(&r[6]),
            "communityName": nullable(&r[7]),
            "communityCode": nullable(&r[8]),
            "latitude": lat,
            "longitude": lon,
            "accuracy": r[11].parse::<i64>().ok(),
            "country": country_name,
        })
    }
}

fn nullable(s: &str) -> Value {
    if s.trim().is_empty() { Value::Null } else { json!(s.trim()) }
}

struct CacheEntry {
    dataset: Arc<PostalDataset>,
    created: std::time::Instant,
}

static DATASET_CACHE: OnceCell<once_cell::sync::Lazy<
    std::sync::Mutex<HashMap<String, Arc<CacheEntry>>>,
>> = OnceCell::new();

fn dataset_store() -> &'static std::sync::Mutex<HashMap<String, Arc<CacheEntry>>> {
    DATASET_CACHE.get_or_init(|| once_cell::sync::Lazy::new(|| std::sync::Mutex::new(HashMap::new())))
}

/// Downloads (at most once per TTL) and indexes a country's postal dataset.
pub async fn get_dataset(state: &AppState, code: &str) -> Result<Arc<PostalDataset>, ApiError> {
    let ttl = state.settings.feed_cache_ttl;
    if let Some(entry) = dataset_store().lock().expect("postal cache lock").get(code) {
        if entry.created.elapsed() < ttl {
            return Ok(entry.dataset.clone());
        }
    }

    // Single-flight: hold the store lock across the download so concurrent
    // callers for the same country share one download (bounded, same as
    // upstream's `inflight` promise semantics).
    let url = format!("{ZIP_BASE}/{code}.zip");
    let (bytes, _ms) = httpx::get_bytes(state, &url, CODE_PREFIX).await?;
    let dataset = Arc::new(parse_zip_dataset(code, &bytes, &url)?);

    dataset_store().lock().expect("postal cache lock").insert(
        code.to_string(),
        Arc::new(CacheEntry { dataset: dataset.clone(), created: std::time::Instant::now() }),
    );
    Ok(dataset)
}

/// Unzips an in-memory GeoNames zip and parses the TSV inside.
pub fn parse_zip_dataset(country: &str, bytes: &[u8], source_url: &str) -> Result<PostalDataset, ApiError> {
    let parse_err = |msg: String| ApiError::upstream(format!("{CODE_PREFIX}_PARSE_ERROR"), msg);
    let reader = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|e| parse_err(format!("The GeoNames dataset is not a readable zip: {e}")))?;

    let mut text: Option<String> = None;
    let preferred = format!("{}.txt", country.to_uppercase());
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| parse_err(format!("Failed reading zip entry: {e}")))?;
        let name = file.name().to_string();
        let lower = name.to_lowercase();
        // The zips bundle a readme.txt first; the data file is named {CC}.txt.
        if lower.ends_with(&preferred.to_lowercase()) {
            let mut buf = String::new();
            use std::io::Read;
            file.read_to_string(&mut buf)
                .map_err(|e| parse_err(format!("Failed reading zip entry body: {e}")))?;
            text = Some(buf);
            break;
        }
        if text.is_none() && lower.ends_with(".txt") && !lower.contains("readme") {
            let mut buf = String::new();
            use std::io::Read;
            file.read_to_string(&mut buf)
                .map_err(|e| parse_err(format!("Failed reading zip entry body: {e}")))?;
            text = Some(buf);
        }
    }
    let text = text.ok_or_else(|| parse_err("The dataset zip contained no .txt file".into()))?;

    let mut rows: Vec<[String; 12]> = Vec::new();
    let mut first_by_key: HashMap<String, usize> = HashMap::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 12 {
            continue;
        }
        let key = normalise_postal_key(cols[1]);
        if key.is_empty() {
            continue;
        }
        let row: [String; 12] = std::array::from_fn(|i| cols[i].trim().to_string());
        first_by_key.entry(key).or_insert_with(|| rows.len());
        rows.push(row);
    }

    Ok(PostalDataset {
        country: country.to_uppercase(),
        rows,
        first_by_key,
        source_url: source_url.to_string(),
        bytes: bytes.len(),
    })
    // `country` and `bytes` are metadata surfaced by /countries/{code} and
    // health checks; see `country_detail`.
}

// ------------------------------------------------------------------ meta

fn wrap(data: Value, started: std::time::Instant, cached: bool, reference_url: &str, note: Option<&str>) -> Value {
    let mut meta = json!({
        "cached": cached,
        "source": {
            "provider": DATA_SOURCE_PROVIDER,
            "licence": DATA_SOURCE_LICENCE,
            "referenceUrl": reference_url,
        },
        "durationMs": started.elapsed().as_millis() as u64,
    });
    if let Some(n) = note {
        meta["note"] = json!(n);
    }
    json!({ "success": true, "data": data, "meta": meta })
}

// -------------------------------------------------------------- query DTOs

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PostalSearchQuery {
    /// Comma-separated ISO country codes to search within.
    pub country: Option<String>,
    /// Postal-code prefix/substring (normalised).
    pub postal_code: Option<String>,
    /// Case-insensitive substring on the place name.
    pub place: Option<String>,
    /// Latitude for a radius search.
    pub latitude: Option<f64>,
    /// Longitude for a radius search.
    pub longitude: Option<f64>,
    /// Radius in km around the point (default 25).
    pub radius: Option<f64>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// `postalCode` (default), `placeName` or `distance`.
    pub sort: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CountryListQuery {
    /// Case-insensitive substring on the country name.
    pub q: Option<String>,
    /// Filter by ISO 4217 currency code.
    pub currency: Option<String>,
    /// Strict `true`/`false` — only countries with a downloadable dataset.
    pub has_dataset: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    let scope = web::scope(SERVICE_PREFIX)
        .route("/search", web::get().to(search))
        .route("/validate/{country}/{postalCode}", web::get().to(validate))
        .route("/distance/{country}/{postalCode}/{country2}/{postalCode2}", web::get().to(distance))
        .route("/countries/{code}", web::get().to(country_detail))
        .route("/countries", web::get().to(countries))
        .route("/health/ready", web::get().to(health_ready))
        .route("/health", web::get().to(health))
        .route("/{country}/{postalCode}", web::get().to(lookup));
    cfg.service(scope);
}

fn require_country(raw: &str) -> Result<String, ApiError> {
    let code = raw.trim().to_uppercase();
    if code.len() == 2 && code.chars().all(|c| c.is_ascii_alphabetic()) {
        Ok(code)
    } else {
        Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            format!("\"{raw}\" is not an ISO 3166-1 alpha-2 country code"),
            json!({ "country": raw }),
        ))
    }
}

/// Compiles a GeoNames postal pattern for local matching (re-anchored, safe).
pub fn compile_pattern(pattern: &str) -> Option<Regex> {
    let anchored = if pattern.starts_with('^') || pattern.starts_with('/') {
        pattern.trim_matches('/')
    } else {
        &format!("^(?:{pattern})$")
    };
    // A hostile pattern must not hang the service: refuse lookaheads/lookbehinds
    // by rejecting constructs the `regex` crate cannot compile anyway, and cap
    // the size.
    if anchored.len() > 512 {
        return None;
    }
    Regex::new(anchored).ok()
}

/// `GET /postal-code/validate/{country}/{postalCode}`
#[utoipa::path(get, path = "/postal-code/validate/{country}/{postalCode}", context_path = "/postal-code",
    params(("country" = String, Path), ("postalCode" = String, Path)),
    responses((status = 200, description = "Validation result"), (status = 404, description = "Unknown country")))]
async fn validate(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let (raw_country, raw_code) = path.into_inner();
    let country_code = require_country(&raw_country)?;
    let table = country_table(&state).await?;
    let reference = table.get(&country_code).ok_or_else(|| {
        ApiError::not_found_details(
            format!("{CODE_PREFIX}_NOT_FOUND"),
            format!("Country \"{country_code}\" is not in the GeoNames reference table"),
            json!({ "country": country_code }),
        )
    })?;

    let input = raw_code.trim().to_string();
    let (normalised, key, valid, input_valid, reason): (Option<String>, Option<String>, bool, bool, Option<&str>) =
        match reference.postal_pattern.as_deref() {
            None => (None, None, false, false, Some("COUNTRY_HAS_NO_DOCUMENTED_PATTERN")),
            Some(pattern) => {
                if input.is_empty() {
                    (None, None, false, false, Some("EMPTY"))
                } else if input.len() > 16 {
                    (Some(normalise_postal_display(&input)), Some(normalise_postal_key(&input)), false, false, Some("INPUT_TOO_LONG"))
                } else {
                    let compiled = compile_pattern(pattern);
                    match compiled {
                        None => (Some(normalise_postal_display(&input)), Some(normalise_postal_key(&input)), false, false, Some("PATTERN_UNUSABLE")),
                        Some(re) => {
                            let norm = normalise_postal_display(&input);
                            let key = normalise_postal_key(&input);
                            let v = re.is_match(&norm);
                            let iv = re.is_match(&input);
                            (Some(norm), Some(key), v, iv, if v { Some("MATCHES_PATTERN") } else { Some("DOES_NOT_MATCH_PATTERN") })
                        }
                    }
                }
            }
        };

    let data = json!({
        "country": country_code,
        "input": input,
        "normalised": normalised,
        "key": key,
        "valid": valid,
        "inputValid": input_valid,
        "pattern": reference.postal_pattern,
        "format": reference.postal_format,
        "reason": reason,
    });
    Ok(Json(wrap(data, started, false, COUNTRY_INFO_URL, None)))
}

/// `GET /postal-code/{country}/{postalCode}`
#[utoipa::path(get, path = "/postal-code/{country}/{postalCode}", context_path = "/postal-code",
    params(("country" = String, Path), ("postalCode" = String, Path)),
    responses((status = 200, description = "Places for one postal code"), (status = 404, description = "Unknown postal code")))]
async fn lookup(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let (raw_country, raw_code) = path.into_inner();
    let country_code = require_country(&raw_country)?;
    let table = country_table(&state).await?;
    let reference = table.get(&country_code).cloned().ok_or_else(|| unknown_country(&country_code))?;
    let dataset = get_dataset(&state, &country_code).await.map_err(|_| {
        ApiError::not_found_details(
            format!("{CODE_PREFIX}_NOT_FOUND"),
            format!("GeoNames publishes no postal dataset for {country_code}"),
            json!({ "country": country_code }),
        )
    })?;
    let key = normalise_postal_key(&raw_code);
    let rows = dataset.rows_for(&key);
    if rows.is_empty() {
        return Err(ApiError::not_found_details(
            format!("{CODE_PREFIX}_NOT_FOUND"),
            format!("Postal code \"{raw_code}\" has no entries in the {country_code} dataset"),
            json!({ "country": country_code, "postalCode": raw_code, "hint": dataset_hint(country_code.as_str()) }),
        ));
    }
    let country_name = reference.name.clone();
    let places: Vec<Value> = rows.iter().map(|r| dataset.place_json(*r, &country_name)).collect();
    let count = places.len();

    Ok(Json(wrap(
        json!({
            "country": country_code,
            "countryName": country_name,
            "postalCode": raw_code,
            "count": count,
            "places": places,
        }),
        started,
        true,
        &dataset.source_url,
        dataset_note(&country_code),
    )))
}

fn dataset_hint(code: &str) -> Value {
    match TRUNCATED_DATASET_NOTES.iter().find(|(c, _)| *c == code) {
        Some((_, note)) => json!(note),
        None => Value::Null,
    }
}

fn dataset_note(code: &str) -> Option<&str> {
    TRUNCATED_DATASET_NOTES.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

fn unknown_country(code: &str) -> ApiError {
    ApiError::not_found_details(
        format!("{CODE_PREFIX}_NOT_FOUND"),
        format!("Country \"{code}\" is not in the GeoNames reference table"),
        json!({ "country": code }),
    )
}

/// `GET /postal-code/search`
#[utoipa::path(get, path = "/postal-code/search", context_path = "/postal-code", params(PostalSearchQuery),
    responses((status = 200, description = "Place search")))]
async fn search(
    state: web::Data<AppState>,
    query: web::Query<PostalSearchQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let countries: Vec<String> = match &query.country {
        Some(csv) if !csv.trim().is_empty() => {
            let mut codes = Vec::new();
            for raw in csv.split(',') {
                codes.push(require_country(raw)?);
            }
            codes
        }
        _ => return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "`country` is required and must be a comma-separated list of ISO 3166-1 alpha-2 codes".to_string(),
            json!({ "parameter": "country" }),
        )),
    };

    let postal_filter = query.postal_code.as_deref().map(normalise_postal_key).filter(|s| !s.is_empty());
    let place_filter = query.place.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    if postal_filter.is_none() && place_filter.is_none() {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "Supply at least one of postal_code or place: an unfiltered scan would download every dataset".to_string(),
            json!({ "accepted": ["postal_code", "place"] }),
        ));
    }
    let point = match (query.latitude, query.longitude) {
        (Some(lat), Some(lon)) => {
            if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
                return Err(ApiError::validation(
                    format!("{CODE_PREFIX}_INVALID_REQUEST"),
                    "latitude must be within [-90,90] and longitude within [-180,180]",
                ));
            }
            Some((lat, lon))
        }
        _ => None,
    };
    let radius = query.radius.unwrap_or(25.0);
    let sort = query.sort.clone().unwrap_or_else(|| {
        if point.is_some() { "distance".into() } else { "postalCode".into() }
    });
    if !matches!(sort.as_str(), "postalCode" | "placeName" | "distance") {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "sort must be one of: postalCode, placeName, distance".to_string(),
            json!({ "sort": sort }),
        ));
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0);

    let table = country_table(&state).await?;
    let mut results: Vec<Value> = Vec::new();
    let mut scanned: Vec<String> = Vec::new();
    for code in &countries {
        let reference = table.get(code).cloned().ok_or_else(|| unknown_country(code))?;
        let dataset = get_dataset(&state, code).await.map_err(|_| {
            ApiError::not_found_details(
                format!("{CODE_PREFIX}_NOT_FOUND"),
                format!("GeoNames publishes no postal dataset for {code}"),
                json!({ "country": code }),
            )
        })?;
        scanned.push(code.clone());
        for (idx, row) in dataset.rows.iter().enumerate() {
            let key = normalise_postal_key(&row[1]);
            if let Some(pf) = &postal_filter {
                if !key.starts_with(pf.as_str()) {
                    continue;
                }
            }
            if let Some(place_needle) = &place_filter {
                if !row[2].to_lowercase().contains(place_needle) {
                    continue;
                }
            }
            let mut place = dataset.place_json(idx, &reference.name);
            if let Some((lat0, lon0)) = point {
                let lat = place.get("latitude").and_then(|v| v.as_f64());
                let lon = place.get("longitude").and_then(|v| v.as_f64());
                match (lat, lon) {
                    (Some(a), Some(b)) => {
                        let d = haversine_km(lat0, lon0, a, b);
                        if d > radius {
                            continue;
                        }
                        place["distanceKm"] = json!((d * 1000.0).round() / 1000.0);
                    }
                    _ => continue, // radius searches require coordinates
                }
            }
            results.push(place);
            if results.len() >= 5_000 {
                break;
            }
        }
    }

    match sort.as_str() {
        "placeName" => results.sort_by(|a, b| {
            let na = a.get("placeName").and_then(|v| v.as_str()).unwrap_or("");
            let nb = b.get("placeName").and_then(|v| v.as_str()).unwrap_or("");
            na.cmp(nb).then_with(|| {
                let ka = a.get("postalCodeKey").and_then(|v| v.as_str()).unwrap_or("");
                let kb = b.get("postalCodeKey").and_then(|v| v.as_str()).unwrap_or("");
                ka.cmp(kb)
            })
        }),
        "distance" => results.sort_by(|a, b| {
            let da = a.get("distanceKm").and_then(|v| v.as_f64()).unwrap_or(f64::MAX);
            let db = b.get("distanceKm").and_then(|v| v.as_f64()).unwrap_or(f64::MAX);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        }),
        _ => results.sort_by(|a, b| {
            let ka = a.get("postalCodeKey").and_then(|v| v.as_str()).unwrap_or("");
            let kb = b.get("postalCodeKey").and_then(|v| v.as_str()).unwrap_or("");
            ka.cmp(kb).then_with(|| {
                let na = a.get("placeName").and_then(|v| v.as_str()).unwrap_or("");
                let nb = b.get("placeName").and_then(|v| v.as_str()).unwrap_or("");
                na.cmp(nb)
            })
        }),
    }

    let total = results.len();
    let page: Vec<Value> = results.into_iter().skip(offset).take(limit).collect();

    Ok(Json(wrap(
        json!({
            "countries": scanned,
            "filters": {
                "postalCode": postal_filter,
                "place": place_filter,
                "latitude": query.latitude,
                "longitude": query.longitude,
                "radiusKm": if point.is_some() { json!(radius) } else { Value::Null },
                "sort": sort,
            },
            "total": total,
            "limit": limit,
            "offset": offset,
            "places": page,
        }),
        started,
        true,
        &format!("{ZIP_BASE}/"),
        None,
    )))
}

/// `GET /postal-code/distance/{country}/{postalCode}/{country2}/{postalCode2}`
#[utoipa::path(get, path = "/postal-code/distance/{country}/{postalCode}/{country2}/{postalCode2}", context_path = "/postal-code",
    params(("country" = String, Path), ("postalCode" = String, Path), ("country2" = String, Path), ("postalCode2" = String, Path)),
    responses((status = 200, description = "Great-circle distance")))]
async fn distance(
    state: web::Data<AppState>,
    path: web::Path<(String, String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let (c1, p1, c2, p2) = path.into_inner();
    let country1 = require_country(&c1)?;
    let country2 = require_country(&c2)?;

    let resolve = |code: String, postal: String| {
        let state = state.clone();
        async move {
            let key = normalise_postal_key(&postal);
            if key.is_empty() {
                return Err(ApiError::validation_details(
                    format!("{CODE_PREFIX}_INVALID_REQUEST"),
                    format!("\"{postal}\" is not a usable postal code"),
                    json!({ "postalCode": postal }),
                ));
            }
            let dataset = get_dataset(&state, &code).await.map_err(|_| {
                ApiError::not_found_details(
                    format!("{CODE_PREFIX}_NOT_FOUND"),
                    format!("GeoNames publishes no postal dataset for {code}"),
                    json!({ "country": code }),
                )
            })?;
            let rows = dataset.rows_for(&key);
            let row = *rows.first().ok_or_else(|| {
                ApiError::not_found_details(
                    format!("{CODE_PREFIX}_NOT_FOUND"),
                    format!("Postal code \"{postal}\" has no entries in the {code} dataset"),
                    json!({ "country": code, "postalCode": postal }),
                )
            })?;
            let r = &dataset.rows[row];
            let lat = r[9].parse::<f64>().ok();
            let lon = r[10].parse::<f64>().ok();
            match (lat, lon) {
                (Some(a), Some(b)) => Ok((a, b, r[2].clone())),
                _ => Err(ApiError::not_found_details(
                    format!("{CODE_PREFIX}_NOT_FOUND"),
                    format!("Postal code \"{postal}\" has no coordinates in the {code} dataset"),
                    json!({ "country": code, "postalCode": postal }),
                )),
            }
        }
    };
    let (lat1, lon1, place1) = resolve(country1.clone(), p1.clone()).await?;
    let (lat2, lon2, place2) = resolve(country2.clone(), p2.clone()).await?;
    let distance = haversine_km(lat1, lon1, lat2, lon2);

    Ok(Json(wrap(
        json!({
            "from": { "country": country1, "postalCode": p1, "placeName": place1, "latitude": lat1, "longitude": lon1 },
            "to": { "country": country2, "postalCode": p2, "placeName": place2, "latitude": lat2, "longitude": lon2 },
            "distanceKm": (distance * 1000.0).round() / 1000.0,
            "distanceM": (distance * 1000.0).round() as i64,
        }),
        started,
        true,
        &format!("{ZIP_BASE}/"),
        None,
    )))
}

/// `GET /postal-code/countries`
#[utoipa::path(get, path = "/postal-code/countries", context_path = "/postal-code", params(CountryListQuery),
    responses((status = 200, description = "Country reference list")))]
async fn countries(
    state: web::Data<AppState>,
    query: web::Query<CountryListQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let table = country_table(&state).await?;
    let mut rows: Vec<Value> = table.values().map(country_ref_json).collect();

    if let Some(q) = query.q.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()) {
        rows.retain(|r| {
            let name = r.get("name").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
            let code = r.get("code").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
            name.contains(&q) || code == q
        });
    }
    if let Some(cur) = query.currency.as_deref().map(|s| s.trim().to_uppercase()).filter(|s| !s.is_empty()) {
        rows.retain(|r| r.get("currencyCode").and_then(|v| v.as_str()) == Some(cur.as_str()));
    }
    if let Some(has) = query.has_dataset.as_deref() {
        let want = has == "true";
        rows.retain(|r| r.get("datasetAvailable").and_then(|v| v.as_bool()).unwrap_or(false) == want);
    }
    rows.sort_by(|a, b| {
        let ka = a.get("code").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("code").and_then(|v| v.as_str()).unwrap_or("");
        ka.cmp(kb)
    });

    let limit = query.limit.unwrap_or(50).clamp(1, 250);
    let offset = query.offset.unwrap_or(0);
    let total = rows.len();
    let page: Vec<Value> = rows.into_iter().skip(offset).take(limit).collect();

    Ok(Json(wrap(
        json!({
            "total": total,
            "limit": limit,
            "offset": offset,
            "countries": page,
        }),
        started,
        true,
        COUNTRY_INFO_URL,
        None,
    )))
}

/// `GET /postal-code/countries/{code}`
#[utoipa::path(get, path = "/postal-code/countries/{code}", context_path = "/postal-code",
    params(("code" = String, Path)),
    responses((status = 200, description = "One country"), (status = 404, description = "Unknown country")))]
async fn country_detail(state: web::Data<AppState>, path: web::Path<String>) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let code = require_country(&path.into_inner())?;
    let table = country_table(&state).await?;
    let reference = table.get(&code).cloned().ok_or_else(|| unknown_country(&code))?;

    let mut data = reference.to_json(dataset_available(&code));
    // datasetRows is filled only when the dataset happens to be loaded.
    if let Some(entry) = dataset_store().lock().expect("postal cache lock").get(&code) {
        data["datasetRows"] = json!(entry.dataset.rows.len());
    } else {
        data["datasetRows"] = Value::Null;
    }
    data["datasetNote"] = match dataset_note(&code) {
        Some(n) => json!(n),
        None => Value::Null,
    };

    Ok(Json(wrap(data, started, true, COUNTRY_INFO_URL, None)))
}

// ------------------------------------------------------------------ health

async fn health_report(state: &AppState) -> Value {
    let probe_key = "postal_probe";
    let probe_cache = state.json_cache(cache_names::HEALTH_PROBES);
    let cached = probe_cache.get(probe_key).await;
    let (probe, was_cached) = match cached {
        Some(v) => (v.as_ref().clone(), true),
        None => {
            let report: Value = match country_table(state).await {
                Ok(table) => json!({ "reachable": true, "countries": table.len() }),
                Err(e) => json!({ "reachable": false, "error": e.to_string() }),
            };
            probe_cache.insert(probe_key.to_string(), Arc::new(report.clone())).await;
            (report, false)
        }
    };
    let reachable = probe.get("reachable").and_then(|v| v.as_bool()).unwrap_or(false);
    json!({
        "status": if reachable { "ok" } else { "degraded" },
        "uptimeSeconds": state.uptime_seconds(),
        "upstream": { "provider": DATA_SOURCE_PROVIDER, "probe": probe, "cached": was_cached },
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
}

/// `GET /postal-code/health`
#[utoipa::path(get, path = "/postal-code/health", context_path = "/postal-code",
    responses((status = 200, description = "Liveness plus upstream probe")))]
async fn health(state: web::Data<AppState>) -> Json<Value> {
    Json(health_report(&state).await)
}

/// `GET /postal-code/health/ready`
#[utoipa::path(get, path = "/postal-code/health/ready", context_path = "/postal-code",
    responses((status = 200, description = "Upstream reachable"), (status = 503, description = "Upstream down")))]
async fn health_ready(state: web::Data<AppState>) -> Result<Json<Value>, ApiError> {
    let report = health_report(&state).await;
    if report.get("upstream").and_then(|u| u.get("probe")).and_then(|p| p.get("reachable")).and_then(|v| v.as_bool()) != Some(true) {
        return Err(ApiError::unavailable(
            format!("{CODE_PREFIX}_UPSTREAM_UNAVAILABLE"),
            "The GeoNames upstream is unreachable".to_string(),
        ));
    }
    Ok(Json(json!({
        "status": "ready",
        "upstream": DATA_SOURCE_PROVIDER,
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })))
}

// ---------------------------------------------------------------- OpenAPI

#[derive(OpenApi)]
#[openapi(
    info(title = "postal-code-api", version = "1.0.0", description = "Postal-code lookup, validation, search and distance over GeoNames."),
    paths(search, validate, distance, countries, country_detail, lookup, health, health_ready)
)]
pub struct PostalApi;

pub fn openapi_doc() -> utoipa::openapi::OpenApi {
    let mut doc = PostalApi::openapi();
    doc.info.title = "postal-code-api".into();
    doc.servers = Some(vec![utoipa::openapi::Server::new(SERVICE_PREFIX)]);
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_country_info() {
        let sample = concat!(
            "# comment line\n",
            "US\tUSA\t840\tUS\tUnited States of America\tWashington\t9629091\t331002651\tNA\t.us\tUSD\tUnited States dollar\t1\t#####-####\t^\\d{5}(-\\d{4})?$\tEN\t185762\t,\n",
            "TZ\tTZA\t834\tTZ\tTanzania, United Republic of\tDodoma\t947300\t61731329\tAF\t.tz\tTZS\tTanzanian shilling\t255\t\t\tSW\t149426\t\n"
        );
        let table = parse_country_info(sample);
        let us = table.get("US").expect("US row");
        assert_eq!(us.iso3, "USA");
        assert_eq!(us.postal_format.as_deref(), Some("#####-####"));
        assert_eq!(us.postal_pattern.as_deref(), Some("^\\d{5}(-\\d{4})?$"));
        assert_eq!(us.currency_code.as_deref(), Some("USD"));
        let tz = table.get("TZ").expect("TZ row");
        assert_eq!(tz.postal_pattern, None);
    }

    #[test]
    fn compiles_and_matches_patterns() {
        let re = compile_pattern("^\\d{5}(-\\d{4})?$").unwrap();
        assert!(re.is_match("90210"));
        assert!(re.is_match("90210-1234"));
        assert!(!re.is_match("9021"));
        // Unanchored patterns get re-anchored.
        let re2 = compile_pattern("[A-Z]{1,2}\\d[A-Z\\d]? ?\\d[A-Z]{2}").unwrap();
        assert!(re2.is_match("M1 1AE"));
        assert!(re2.is_match("SW1A 1AA"));
        assert!(!re2.is_match("1AE M1 1"));
        assert!(compile_pattern("(?<=x)y").is_none()); // lookbehind unsupported
    }

    #[test]
    fn parses_zip_bytes() {
        // Build a tiny zip in memory.
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("US.txt", opts).unwrap();
            std::io::Write::write_all(&mut w, b"US\t90210\tBeverly Hills\tCalifornia\tCA\tLos Angeles\t037\t\t\t34.0901\t-118.4065\t1\nUS\t10001\tNew York\tNew York\tNY\tNew York\t061\t\t\t40.7506\t-73.9971\t1\n").unwrap();
            w.finish().unwrap();
        }
        let bytes = buf.into_inner();
        let ds = parse_zip_dataset("US", &bytes, "test").unwrap();
        assert_eq!(ds.rows.len(), 2);
        let rows = ds.rows_for("90210");
        assert_eq!(rows.len(), 1);
        assert_eq!(ds.rows[rows[0]][2], "Beverly Hills");
    }
}
