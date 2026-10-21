//! Route handlers for the public-holidays service.

use std::sync::Arc;

use actix_web::web::{self, Json};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use utoipa::{IntoParams, OpenApi};

use master_common::error::ApiError;
use master_common::httpx;
use master_common::state::{cache_names, AppState};
use futures::StreamExt;

use crate::{
    add_days, day_of_week, each_date, is_real_iso_date, is_weekend_date,
    normalise_country_code, normalise_region_code, today_iso, CODE_PREFIX, FIRST_YEAR, LAST_YEAR,
    MAX_FAN_OUT, MAX_LIMIT, MAX_LIST_COUNTRIES, MAX_RANGE_DAYS, SERVICE_PREFIX, UPSTREAM_NAGER,
};

// ------------------------------------------------------------- domain model

/// One holiday in this service's own vocabulary (never the raw upstream shape).
#[derive(Debug, Clone)]
pub struct Holiday {
    pub date: String,
    pub name: String,
    pub country_code: String,
    pub scope: &'static str, // national | subdivision
    pub subdivision_codes: Vec<String>,
    pub types: Vec<String>,
    pub day_of_week: u32,
    pub is_weekend: bool,
}

impl Holiday {
    pub fn to_json(&self) -> Value {
        json!({
            "date": self.date,
            "name": self.name,
            "countryCode": self.country_code,
            "scope": self.scope,
            "subdivisionCodes": self.subdivision_codes,
            "types": self.types,
            "dayOfWeek": self.day_of_week,
            "isWeekend": self.is_weekend,
        })
    }
}

/// The only `holidayTypes` values Nager was observed to emit; unknown values
/// are dropped rather than passed through.
fn to_holiday_type(value: &str) -> Option<String> {
    crate::HOLIDAY_TYPES
        .iter()
        .find(|t| **t == value)
        .map(|t| t.to_string())
}

fn parse_holiday(raw: &Value, fallback_code: &str) -> Result<Holiday, ApiError> {
    let parse_err = |msg: String| ApiError::upstream(format!("{CODE_PREFIX}_PARSE_ERROR"), msg);
    let date = raw.get("date").and_then(|v| v.as_str()).unwrap_or("");
    if !is_real_iso_date(date) {
        return Err(parse_err("Upstream holiday is missing a valid `date`".into()));
    }
    let name = raw.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name.is_empty() {
        return Err(parse_err("Upstream holiday is missing `name`".into()));
    }
    let code = raw
        .get("countryCode")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_uppercase())
        .unwrap_or_else(|| fallback_code.to_string());
    if code.is_empty() {
        return Err(parse_err("Upstream holiday is missing `countryCode`".into()));
    }
    let national = raw
        .get("nationalHoliday")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| parse_err("Upstream holiday is missing a boolean `nationalHoliday`".into()))?;
    let mut codes: Vec<String> = raw
        .get("subdivisionCodes")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(|s| s.to_uppercase()).collect())
        .unwrap_or_default();
    codes.sort();
    codes.dedup();
    let mut types: Vec<String> = raw
        .get("holidayTypes")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .filter_map(to_holiday_type)
                .collect::<Vec<_>>()
        })
        .ok_or_else(|| parse_err("Upstream holiday is missing an array `holidayTypes`".into()))?;
    types.sort();
    types.dedup();
    let dow = day_of_week(date);
    Ok(Holiday {
        date: date.to_string(),
        name: name.to_string(),
        country_code: code,
        scope: if national { "national" } else { "subdivision" },
        subdivision_codes: if national { Vec::new() } else { codes },
        types,
        day_of_week: dow,
        is_weekend: matches!(dow, 0 | 6),
    })
}

fn parse_holiday_list(raw: &Value, fallback_code: &str) -> Result<Vec<Holiday>, ApiError> {
    let arr = raw
        .as_array()
        .ok_or_else(|| ApiError::upstream(format!("{CODE_PREFIX}_PARSE_ERROR"), "Upstream holiday payload is not a JSON array".to_string()))?;
    arr.iter().map(|e| parse_holiday(e, fallback_code)).collect()
}

/// Deterministic total ordering; ties break identically in both directions.
fn sort_holidays(items: Vec<Holiday>, sort: &str) -> Vec<Holiday> {
    let (descending, key) = match sort.strip_prefix('-') {
        Some(k) => (true, k),
        None => (false, sort),
    };
    let mut out = items;
    out.sort_by(|a, b| {
        let ord = match key {
            "country" => a.country_code.cmp(&b.country_code),
            "name" => a.name.cmp(&b.name),
            _ => a.date.cmp(&b.date),
        };
        let tie = a
            .date
            .cmp(&b.date)
            .then_with(|| a.country_code.cmp(&b.country_code))
            .then_with(|| a.name.cmp(&b.name));
        let full = if ord == std::cmp::Ordering::Equal { tie } else { ord };
        if descending { full.reverse() } else { full }
    });
    out
}

/// Case-insensitive type filter; an entry matches if ANY of its types match.
fn filter_by_types(items: Vec<Holiday>, types: &Option<Vec<String>>) -> Vec<Holiday> {
    let Some(types) = types.as_ref().filter(|t| !t.is_empty()) else {
        return items;
    };
    let wanted: Vec<String> = types.iter().map(|t| t.to_lowercase()).collect();
    items
        .into_iter()
        .filter(|h| h.types.iter().any(|t| wanted.contains(&t.to_lowercase())))
        .collect()
}

fn filter_by_date_range(items: Vec<Holiday>, from: &Option<String>, to: &Option<String>) -> Vec<Holiday> {
    items
        .into_iter()
        .filter(|h| from.as_ref().map(|f| h.date >= *f).unwrap_or(true))
        .filter(|h| to.as_ref().map(|t| h.date <= *t).unwrap_or(true))
        .collect()
}

fn known_regions(items: &[Holiday]) -> Vec<String> {
    let mut set: Vec<String> = items
        .iter()
        .flat_map(|h| h.subdivision_codes.iter().cloned())
        .collect();
    set.sort();
    set.dedup();
    set
}

/// A holiday applies in `region` when it is national or listed for it.
fn applies_to_region(h: &Holiday, region: &str) -> bool {
    h.scope == "national" || h.subdivision_codes.iter().any(|c| c == region)
}

// --------------------------------------------------------- upstream client

async fn nager_get(state: &AppState, path: &str) -> Result<Option<Value>, ApiError> {
    let url = format!("{UPSTREAM_NAGER}{path}");
    httpx::get_json_opt(state, &url, &[], CODE_PREFIX).await
}

/// Cached read of one country/year (single-flight via the cache registry).
async fn holidays_cached(state: &AppState, code: &str, year: i32) -> Result<Vec<Holiday>, ApiError> {
    let cache = state.json_cache(cache_names::PUBLIC_HOLIDAYS);
    let key = format!("{code}:{year}");
    if let Some(v) = cache.get(&key).await {
        return holiday_list_from_json(&v);
    }
    let raw = nager_get(state, &format!("/api/v4/Holidays/{code}/{year}")).await?;
    let list = match raw {
        None => Vec::new(), // 204: a supported country with no rules this year
        Some(payload) => parse_holiday_list(&payload, code)?,
    };
    let as_json = Value::Array(list.iter().map(|h| h.to_json()).collect());
    cache.insert(key, Arc::new(as_json)).await;
    Ok(list)
}

fn holiday_list_from_json(v: &Value) -> Result<Vec<Holiday>, ApiError> {
    let arr = match v.as_array() {
        Some(a) => a,
        None => return Ok(Vec::new()),
    };
    Ok(arr
        .iter()
        .map(|h| Holiday {
            date: h.get("date").and_then(|d| d.as_str()).unwrap_or("").into(),
            name: h.get("name").and_then(|d| d.as_str()).unwrap_or("").into(),
            country_code: h.get("countryCode").and_then(|d| d.as_str()).unwrap_or("").into(),
            scope: match h.get("scope").and_then(|s| s.as_str()) {
                Some("national") => "national",
                _ => "subdivision",
            },
            subdivision_codes: h
                .get("subdivisionCodes")
                .and_then(|d| d.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            types: h
                .get("types")
                .and_then(|d| d.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            day_of_week: h.get("dayOfWeek").and_then(|d| d.as_u64()).unwrap_or(0) as u32,
            is_weekend: h.get("isWeekend").and_then(|d| d.as_bool()).unwrap_or(false),
        })
        .collect())
}

async fn upcoming_cached(state: &AppState, code: &str) -> Result<Vec<Holiday>, ApiError> {
    // Deliberately short TTL: the answer changes the moment today does.
    let cache = state.json_cache(cache_names::PUBLIC_HOLIDAYS);
    let key = format!("next:{code}");
    if let Some(v) = cache.get(&key).await {
        return holiday_list_from_json(&v);
    }
    let raw = nager_get(state, &format!("/api/v4/Holidays/{code}/Next")).await?;
    let list = match raw {
        None => Vec::new(),
        Some(payload) => parse_holiday_list(&payload, code)?,
    };
    let as_json = Value::Array(list.iter().map(|h| h.to_json()).collect());
    cache.insert(key, Arc::new(as_json)).await;
    Ok(list)
}

async fn available_countries(state: &AppState) -> Result<Vec<Value>, ApiError> {
    let cache = state.json_cache(cache_names::PUBLIC_HOLIDAYS);
    let key = "countries:all";
    if let Some(v) = cache.get(key).await {
        return Ok(v.as_array().cloned().unwrap_or_default());
    }
    let raw = nager_get(state, "/api/v4/Countries/Available")
        .await?
        .ok_or_else(|| ApiError::upstream(format!("{CODE_PREFIX}_PARSE_ERROR"), "Upstream country list is empty".to_string()))?;
    let arr = raw
        .as_array()
        .ok_or_else(|| ApiError::upstream(format!("{CODE_PREFIX}_PARSE_ERROR"), "Upstream country list is not a JSON array".to_string()))?;
    let mut out = Vec::with_capacity(arr.len());
    for (idx, entry) in arr.iter().enumerate() {
        let code = normalise_country_code(entry.get("countryCode").and_then(|v| v.as_str()).unwrap_or(""));
        let name = entry.get("name").and_then(|v| v.as_str());
        match (code, name) {
            (Some(code), Some(name)) => out.push(json!({
                "countryCode": code,
                "name": name,
                "regionCount": Value::Null,
            })),
            _ => {
                return Err(ApiError::upstream(
                    format!("{CODE_PREFIX}_PARSE_ERROR"),
                    format!("Upstream country entry {idx} is missing `countryCode` or `name`"),
                ));
            }
        }
    }
    cache.insert(key.to_string(), Arc::new(Value::Array(out.clone()))).await;
    Ok(out)
}

/// Bounded fan-out preserving input order.
async fn fetch_many<T, R, F, Fut>(items: Vec<T>, worker: F) -> Vec<R>
where
    T: Send + 'static,
    R: Send + 'static,
    F: Fn(T) -> Fut + Send + 'static + Clone,
    Fut: std::future::Future<Output = R> + Send + 'static,
{
    futures::stream::iter(items.into_iter().map(|item| {
        let w = worker.clone();
        async move { w(item).await }
    }))
    .buffered(MAX_FAN_OUT)
    .collect::<Vec<R>>()
    .await
}

// -------------------------------------------------------------- validation

fn invalid(code: &str, message: impl Into<String>) -> ApiError {
    ApiError::validation(format!("{CODE_PREFIX}_{code}"), message)
}

fn no_holidays(message: impl Into<String>, details: Value) -> ApiError {
    ApiError::not_found_details(format!("{CODE_PREFIX}_NO_DATA"), message, details)
}

fn assert_year(year: i32) -> Result<(), ApiError> {
    if (FIRST_YEAR..=LAST_YEAR).contains(&year) {
        Ok(())
    } else {
        Err(invalid(
            "INVALID_REQUEST",
            format!("Upstream only serves {FIRST_YEAR}-{LAST_YEAR}; {year} is outside the supported window"),
        ))
    }
}

fn current_year() -> i32 {
    let y = chrono::Utc::now().format("%Y").to_string().parse::<i32>().unwrap_or(FIRST_YEAR);
    y.clamp(FIRST_YEAR, LAST_YEAR)
}

fn require_country(raw: &Option<String>) -> Result<String, ApiError> {
    raw.as_deref()
        .and_then(normalise_country_code)
        .ok_or_else(|| {
            invalid(
                "INVALID_REQUEST",
                "`country` is required and must be an ISO 3166-1 alpha-2 code such as AT, DE, US",
            )
        })
}

fn require_date(raw: &str, field: &str) -> Result<String, ApiError> {
    if is_real_iso_date(raw) {
        Ok(raw.to_string())
    } else {
        Err(invalid(
            "INVALID_REQUEST",
            format!("`{field}` must be a real calendar date in YYYY-MM-DD form"),
        ))
    }
}

fn clamp_limit(requested: Option<usize>) -> usize {
    requested
        .map(|v| v.clamp(1, MAX_LIMIT))
        .unwrap_or(crate::DEFAULT_LIMIT)
}

fn window_note(country_count: usize, truncated: bool, year: Option<i32>) -> Value {
    json!({
        "firstYear": FIRST_YEAR,
        "lastYear": LAST_YEAR,
        "countryCount": country_count,
        "countriesTruncated": truncated,
        "note": match year {
            Some(y) => format!(
                "Nager.Date Community v4 serves one country per request. A single call covers up to {MAX_LIST_COUNTRIES} countries for the single year {y}; supported years are {FIRST_YEAR}-{LAST_YEAR}."
            ),
            None => format!(
                "Nager publishes subdivision-level holiday data for a minority of countries; `regionCount` is null unless `include=regionCounts` was requested."
            ),
        },
    })
}

fn generated_at() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

// ------------------------------------------------------------- query DTOs

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CountriesQuery {
    /// Substring match on name or exact code match.
    pub q: Option<String>,
    /// `regionCounts` to compute regionCount per country (extra upstream load).
    pub include: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct WorkingDaysQuery {
    /// ISO 3166-1 alpha-2 code (required).
    pub country: Option<String>,
    /// YYYY-MM-DD (default: today).
    pub from: Option<String>,
    /// YYYY-MM-DD (default: today, or from+30).
    pub to: Option<String>,
    /// ISO 3166-2 subdivision filter, e.g. `US-CA`.
    pub region: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UpcomingQuery {
    /// ISO 3166-1 alpha-2 code (required).
    pub country: Option<String>,
    /// How many upcoming holidays to return (default 10).
    pub count: Option<usize>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct IsHolidayQuery {
    /// ISO 3166-1 alpha-2 code (required).
    pub country: Option<String>,
    /// YYYY-MM-DD (default: today).
    pub date: Option<String>,
    /// ISO 3166-2 subdivision filter, e.g. `AT-2`.
    pub region: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListHolidaysQuery {
    /// Comma-separated ISO codes, e.g. `AT,DE`.
    pub country: Option<String>,
    /// Four-digit year inside the upstream window (default: current year).
    pub year: Option<i32>,
    /// YYYY-MM-DD lower bound.
    pub from: Option<String>,
    /// YYYY-MM-DD upper bound.
    pub to: Option<String>,
    /// Comma-separated holiday types, e.g. `Public,Bank`.
    #[serde(rename = "type")]
    pub holiday_type: Option<String>,
    /// `date`, `-date`, `name`, `-name`, `country`, `-country`.
    pub sort: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CountryHolidaysQuery {
    /// Four-digit year (default: current year).
    pub year: Option<i32>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RegionHolidaysQuery {
    /// ISO 3166-2 subdivision, e.g. `AT-2`. Omit for all subdivision holidays.
    pub region: Option<String>,
    /// Comma-separated holiday types.
    #[serde(rename = "type")]
    pub holiday_type: Option<String>,
    /// `date` (default), `-date`, `name`, `-name`.
    pub sort: Option<String>,
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    let scope = web::scope(SERVICE_PREFIX)
        .route("/countries", web::get().to(countries))
        .route("/working-days", web::get().to(working_days))
        .route("/holidays/upcoming", web::get().to(upcoming))
        .route("/holidays/is-holiday", web::get().to(is_holiday))
        .route("/holidays", web::get().to(list))
        .route("/holidays/{countryCode}/{year}", web::get().to(country_year_region))
        .route("/holidays/{countryCode}", web::get().to(country_year))
        .route("/health/ready", web::get().to(health_ready))
        .route("/health", web::get().to(health));
    cfg.service(scope);
}

// ---------------------------------------------------------------- handlers

/// `GET /public-holidays/holidays` — the windowed list.
#[utoipa::path(get, path = "/public-holidays/holidays", context_path = "/public-holidays", params(ListHolidaysQuery),
    responses((status = 200, description = "Windowed holiday list")))]
async fn list(
    state: web::Data<AppState>,
    query: web::Query<ListHolidaysQuery>,
) -> Result<Json<Value>, ApiError> {
    let sort = query.sort.clone().unwrap_or_else(|| "date".into());
    let limit = clamp_limit(query.limit);
    let offset = query.offset.unwrap_or(0);
    let year = query.year.unwrap_or_else(current_year);
    assert_year(year)?;
    let from = match &query.from {
        Some(v) => Some(require_date(v, "from")?),
        None => None,
    };
    let to = match &query.to {
        Some(v) => Some(require_date(v, "to")?),
        None => None,
    };
    if let (Some(f), Some(t)) = (&from, &to) {
        if f > t {
            return Err(invalid("INVALID_REQUEST", "`from` must not be later than `to`"));
        }
    }

    let all_countries = available_countries(&state).await?;
    let requested: Option<Vec<String>> = match &query.country {
        Some(csv) if !csv.trim().is_empty() => {
            let mut codes = Vec::new();
            for raw in csv.split(',') {
                let code = normalise_country_code(raw).ok_or_else(|| {
                    invalid("INVALID_REQUEST", "`country` must be ISO 3166-1 alpha-2 codes, e.g. AT, DE, US")
                })?;
                if !all_countries.iter().any(|c| c.get("countryCode").and_then(|v| v.as_str()) == Some(code.as_str())) {
                    return Err(ApiError::not_found_details(
                        format!("{CODE_PREFIX}_NOT_FOUND"),
                        "Country is not supported by the upstream holiday service".to_string(),
                        json!({ "countryCode": code }),
                    ));
                }
                codes.push(code);
            }
            Some(codes)
        }
        _ => None,
    };

    let resolved: Vec<String> = match requested {
        Some(codes) => codes,
        None => all_countries
            .iter()
            .filter_map(|c| c.get("countryCode").and_then(|v| v.as_str()).map(String::from))
            .collect(),
    };
    let effective: Vec<String> = resolved.iter().take(MAX_LIST_COUNTRIES).cloned().collect();
    let truncated = resolved.len() > effective.len();

    let st = state.clone();
    let fetched: Vec<Vec<Holiday>> =
        fetch_many(effective.clone(), move |code| {
            let st = st.clone();
            async move { holidays_cached(&st, &code, year).await.unwrap_or_default() }
        })
        .await;
    let merged: Vec<Holiday> = fetched.into_iter().flatten().collect();

    let types = query.holiday_type.as_ref().map(|t| {
        t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>()
    });
    let filtered = filter_by_date_range(filter_by_types(merged, &types), &from, &to);
    let sorted = sort_holidays(filtered, &sort);
    let total = sorted.len();
    let items: Vec<Value> = sorted.into_iter().skip(offset).take(limit).map(|h| h.to_json()).collect();

    Ok(Json(json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
        "sort": sort,
        "window": window_note(effective.len(), truncated, Some(year)),
        "generatedAt": generated_at(),
    })))
}

/// `GET /public-holidays/holidays/{countryCode}`
#[utoipa::path(get, path = "/public-holidays/holidays/{countryCode}", context_path = "/public-holidays",
    params(("countryCode" = String, Path), CountryHolidaysQuery),
    responses((status = 200, description = "One country's year"), (status = 404, description = "No data")))]
async fn country_year(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<CountryHolidaysQuery>,
) -> Result<Json<Value>, ApiError> {
    let raw = path.into_inner();
    let code = require_country(&Some(raw))?;
    let year = query.year.unwrap_or_else(current_year);
    assert_year(year)?;

    let items = holidays_cached(&state, &code, year).await?;
    if items.is_empty() {
        return Err(no_holidays(
            format!("No holiday data published for {code} in {year}"),
            json!({ "countryCode": code, "year": year }),
        ));
    }

    let mut type_counts: Map<String, Value> = Map::new();
    for h in &items {
        for t in &h.types {
            *type_counts.entry(t.clone()).or_insert(json!(0)) = json!(type_counts.get(t).and_then(|v| v.as_u64()).unwrap_or(0) + 1);
        }
    }
    let total = items.len();
    let national = items.iter().filter(|h| h.scope == "national").count();
    let subdivision = items.iter().filter(|h| h.scope == "subdivision").count();
    let sorted: Vec<Value> = sort_holidays(items, "date").into_iter().map(|h| h.to_json()).collect();

    Ok(Json(json!({
        "countryCode": code,
        "year": year,
        "total": total,
        "nationalCount": national,
        "subdivisionCount": subdivision,
        "typeCounts": Value::Object(type_counts),
        "items": sorted,
        "generatedAt": generated_at(),
    })))
}

/// `GET /public-holidays/holidays/{countryCode}/{year}?region=`
#[utoipa::path(get, path = "/public-holidays/holidays/{countryCode}/{year}", context_path = "/public-holidays",
    params(("countryCode" = String, Path), ("year" = i32, Path), RegionHolidaysQuery),
    responses((status = 200, description = "Subdivision-scope holidays"), (status = 404, description = "No data")))]
async fn country_year_region(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    query: web::Query<RegionHolidaysQuery>,
) -> Result<Json<Value>, ApiError> {
    let (raw_code, raw_year) = path.into_inner();
    let code = require_country(&Some(raw_code))?;
    if !(raw_year.len() == 4 && raw_year.chars().all(|c| c.is_ascii_digit())) {
        return Err(invalid("INVALID_REQUEST", "`year` must be a four digit year"));
    }
    let year: i32 = raw_year.parse().unwrap_or(0);
    assert_year(year)?;

    let all = holidays_cached(&state, &code, year).await?;
    if all.is_empty() {
        return Err(no_holidays(
            format!("No holiday data published for {code} in {year}"),
            json!({ "countryCode": code, "year": year }),
        ));
    }

    let regions = known_regions(&all);
    let items: Vec<Holiday>;
    let requested_region: Option<String>;
    if let Some(raw_region) = &query.region {
        let region = normalise_region_code(raw_region).ok_or_else(|| {
            invalid("INVALID_REQUEST", "`region` must be an ISO 3166-2 code such as AT-2 or US-CA")
        })?;
        if !regions.contains(&region) {
            return Err(no_holidays(
                format!("Upstream publishes no holiday data for subdivision {region}"),
                json!({ "countryCode": code, "year": year, "region": region, "knownRegions": regions }),
            ));
        }
        requested_region = Some(region.clone());
        items = all.into_iter().filter(|h| applies_to_region(&h, &region)).collect();
    } else {
        requested_region = None;
        items = all.into_iter().filter(|h| h.scope == "subdivision").collect();
    }

    let types = query.holiday_type.as_ref().map(|t| {
        t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>()
    });
    let filtered = sort_holidays(filter_by_types(items, &types), query.sort.as_deref().unwrap_or("date"));
    let total = filtered.len();
    let national = filtered.iter().filter(|h| h.scope == "national").count();
    let region_only = filtered.iter().filter(|h| h.scope == "subdivision").count();
    let out: Vec<Value> = filtered.into_iter().map(|h| h.to_json()).collect();

    Ok(Json(json!({
        "countryCode": code,
        "year": year,
        "region": requested_region,
        "knownRegions": regions,
        "total": total,
        "nationalCount": national,
        "regionOnlyCount": region_only,
        "items": out,
        "generatedAt": generated_at(),
    })))
}

/// `GET /public-holidays/holidays/upcoming`
#[utoipa::path(get, path = "/public-holidays/holidays/upcoming", context_path = "/public-holidays", params(UpcomingQuery),
    responses((status = 200, description = "Upcoming holidays"), (status = 404, description = "No upcoming holidays")))]
async fn upcoming(
    state: web::Data<AppState>,
    query: web::Query<UpcomingQuery>,
) -> Result<Json<Value>, ApiError> {
    let code = require_country(&query.country)?;
    let count = query.count.unwrap_or(10);
    let as_of = today_iso();

    let all = upcoming_cached(&state, &code).await?;
    if all.is_empty() {
        return Err(no_holidays(
            format!("The upstream service publishes no upcoming holidays for {code}"),
            json!({ "countryCode": code, "asOf": as_of }),
        ));
    }
    let future: Vec<Holiday> = all.into_iter().filter(|h| h.date >= as_of).collect();
    let available = future.len();
    let items: Vec<Value> = future.into_iter().take(count).map(|h| h.to_json()).collect();

    Ok(Json(json!({
        "countryCode": code,
        "asOf": as_of,
        "requested": count,
        "available": available,
        "lookforwardDays": 365,
        "items": items,
        "window": {
            "firstYear": FIRST_YEAR,
            "lastYear": LAST_YEAR,
            "countryCount": 1,
            "countriesTruncated": false,
            "note": "Computed from the upstream 365-day forward window, re-anchored to the caller's current date and trimmed to `count`.",
        },
        "generatedAt": generated_at(),
    })))
}

/// `GET /public-holidays/countries`
#[utoipa::path(get, path = "/public-holidays/countries", context_path = "/public-holidays", params(CountriesQuery),
    responses((status = 200, description = "Supported countries")))]
async fn countries(
    state: web::Data<AppState>,
    query: web::Query<CountriesQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = clamp_limit(query.limit);
    let offset = query.offset.unwrap_or(0);
    let all = available_countries(&state).await?;

    let term = query.q.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    let matched: Vec<Value> = all
        .into_iter()
        .filter(|c| match &term {
            Some(t) => {
                let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                let code = c.get("countryCode").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                name.contains(t) || code == *t
            }
            None => true,
        })
        .collect();

    let region_counts_computed = query.include.as_deref() == Some("regionCounts");
    let matched = if region_counts_computed {
        let year = current_year();
        let st = state.clone();
        let codes: Vec<String> = matched
            .iter()
            .filter_map(|c| c.get("countryCode").and_then(|v| v.as_str()).map(String::from))
            .collect();
        let counts: Vec<usize> = fetch_many(codes.clone(), move |code| {
            let st = st.clone();
            let code = code;
            async move {
                holidays_cached(&st, &code, year)
                    .await
                    .map(|items| known_regions(&items).len())
                    .unwrap_or(0)
            }
        })
        .await;
        matched
            .into_iter()
            .zip(counts.into_iter())
            .map(|(mut c, n)| {
                c["regionCount"] = json!(n);
                c
            })
            .collect()
    } else {
        matched
    };

    let mut sorted = matched;
    sorted.sort_by(|a, b| {
        let ka = a.get("countryCode").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("countryCode").and_then(|v| v.as_str()).unwrap_or("");
        ka.cmp(kb)
    });
    let total = sorted.len();
    let items: Vec<Value> = sorted.into_iter().skip(offset).take(limit).collect();

    Ok(Json(json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
        "sort": "countryCode",
        "window": window_note(total, false, None),
        "regionCountsComputed": region_counts_computed,
        "generatedAt": generated_at(),
    })))
}

/// `GET /public-holidays/holidays/is-holiday`
#[utoipa::path(get, path = "/public-holidays/holidays/is-holiday", context_path = "/public-holidays", params(IsHolidayQuery),
    responses((status = 200, description = "Holiday check")))]
async fn is_holiday(
    state: web::Data<AppState>,
    query: web::Query<IsHolidayQuery>,
) -> Result<Json<Value>, ApiError> {
    let code = require_country(&query.country)?;
    let date = match &query.date {
        Some(d) => require_date(d, "date")?,
        None => today_iso(),
    };
    let region = match &query.region {
        Some(r) => Some(normalise_region_code(r).ok_or_else(|| {
            invalid("INVALID_REQUEST", "`region` must be an ISO 3166-2 code such as AT-2 or US-CA")
        })?),
        None => None,
    };

    let year: i32 = date[..4].parse().unwrap_or(0);
    assert_year(year)?;
    let all = holidays_cached(&state, &code, year).await?;
    let candidates: Vec<Holiday> = match &region {
        Some(r) => all.into_iter().filter(|h| applies_to_region(h, r)).collect(),
        None => all,
    };
    let matches: Vec<Holiday> = candidates.into_iter().filter(|h| h.date == date).collect();
    let weekend = is_weekend_date(&date);
    let national = matches.iter().find(|h| h.scope == "national");
    let is_holiday = !matches.is_empty();
    let scope = national
        .map(|_| "national")
        .or_else(|| matches.first().map(|m| m.scope));

    Ok(Json(json!({
        "countryCode": code,
        "date": date,
        "dayOfWeek": day_of_week(&date),
        "isWeekend": weekend,
        "isHoliday": is_holiday,
        "isWorkingDay": !is_holiday && !weekend,
        "scope": scope,
        "matches": sort_holidays(matches, "name").into_iter().map(|h| h.to_json()).collect::<Vec<_>>(),
        "generatedAt": generated_at(),
    })))
}

/// `GET /public-holidays/working-days`
#[utoipa::path(get, path = "/public-holidays/working-days", context_path = "/public-holidays", params(WorkingDaysQuery),
    responses((status = 200, description = "Working day summary")))]
async fn working_days(
    state: web::Data<AppState>,
    query: web::Query<WorkingDaysQuery>,
) -> Result<Json<Value>, ApiError> {
    let code = require_country(&query.country)?;
    let to = match &query.to {
        Some(t) => require_date(t, "to")?,
        None => today_iso(),
    };
    let from = match &query.from {
        Some(f) => require_date(f, "from")?,
        None => add_days(&to, -30),
    };
    if from > to {
        return Err(invalid("INVALID_REQUEST", "`from` must not be later than `to`"));
    }
    let span = each_date(&from, &to);
    if span.len() > MAX_RANGE_DAYS {
        return Err(invalid(
            "INVALID_REQUEST",
            format!("Date range is too wide; the maximum is {MAX_RANGE_DAYS} days"),
        ));
    }
    let region = match &query.region {
        Some(r) => Some(normalise_region_code(r).ok_or_else(|| {
            invalid("INVALID_REQUEST", "`region` must be an ISO 3166-2 code such as AT-2 or US-CA")
        })?),
        None => None,
    };

    let start_year: i32 = from[..4].parse().unwrap_or(FIRST_YEAR);
    let end_year: i32 = to[..4].parse().unwrap_or(LAST_YEAR);
    let mut years: Vec<i32> = Vec::new();
    for y in start_year..=end_year {
        if (FIRST_YEAR..=LAST_YEAR).contains(&y) {
            years.push(y);
        }
    }
    let st = state.clone();
    let code_for_fanout = code.clone();
    let fetched: Vec<Vec<Holiday>> = fetch_many(years.clone(), move |year| {
        let st = st.clone();
        let code = code_for_fanout.clone();
        async move { holidays_cached(&st, &code, year).await.unwrap_or_default() }
    })
    .await;
    let mut all: Vec<Holiday> = fetched.into_iter().flatten().collect();
    if let Some(r) = &region {
        all.retain(|h| applies_to_region(h, r));
    }
    let all = sort_holidays(all, "date");

    let mut by_date: std::collections::HashMap<String, Vec<&Holiday>> = std::collections::HashMap::new();
    for h in &all {
        by_date.entry(h.date.clone()).or_default().push(h);
    }

    let mut working_day_dates: Vec<String> = Vec::new();
    let mut weekend_holiday_dates: Vec<String> = Vec::new();
    let mut weekend_days = 0usize;
    let mut holiday_days = 0usize;
    for date in &span {
        let weekend = is_weekend_date(date);
        let hits = by_date.get(date).map(|v| v.len()).unwrap_or(0);
        if weekend {
            weekend_days += 1;
            if hits > 0 {
                weekend_holiday_dates.push(date.clone());
            }
            continue;
        }
        if hits > 0 {
            holiday_days += 1;
            continue;
        }
        working_day_dates.push(date.clone());
    }

    Ok(Json(json!({
        "countryCode": code,
        "from": from,
        "to": to,
        "calendarDays": span.len(),
        "weekendDays": weekend_days,
        "holidayDays": holiday_days,
        "workingDays": working_day_dates.len(),
        "workingDayDates": working_day_dates,
        "weekendHolidayDates": weekend_holiday_dates,
        "holidays": all.iter().filter(|h| h.date >= from && h.date <= to).map(|h| h.to_json()).collect::<Vec<_>>(),
        "window": {
            "firstYear": FIRST_YEAR,
            "lastYear": LAST_YEAR,
            "countryCount": 1,
            "countriesTruncated": false,
            "note": format!("Walked {} of Nager data (upstream has no range endpoint).", years.iter().map(|y| y.to_string()).collect::<Vec<_>>().join(", ")),
        },
        "generatedAt": generated_at(),
    })))
}

// ------------------------------------------------------------------ health

async fn health_report(state: &AppState) -> Value {
    let probe_key = "holidays_probe";
    let probe_cache = state.json_cache(cache_names::HEALTH_PROBES);
    let cached = probe_cache.get(probe_key).await;
    let (probe, was_cached) = match cached {
        Some(v) => (v.as_ref().clone(), true),
        None => {
            let report: Value = match nager_get(&state, "/api/v4/Versions").await {
                Ok(Some(v)) => json!({
                    "reachable": true,
                    "name": v.get("name").and_then(|x| x.as_str()).unwrap_or("Nager.Date"),
                    "version": v.get("version").and_then(|x| x.as_str()),
                }),
                Ok(None) => json!({ "reachable": false, "error": "Upstream returned no version payload" }),
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
        "upstream": { "name": "nager.date", "probe": probe, "cached": was_cached },
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
}

/// `GET /public-holidays/health`
#[utoipa::path(get, path = "/public-holidays/health", context_path = "/public-holidays",
    responses((status = 200, description = "Liveness plus upstream probe")))]
async fn health(state: web::Data<AppState>) -> Json<Value> {
    Json(health_report(&state).await)
}

/// `GET /public-holidays/health/ready`
#[utoipa::path(get, path = "/public-holidays/health/ready", context_path = "/public-holidays",
    responses((status = 200, description = "Upstream reachable"), (status = 503, description = "Upstream down")))]
async fn health_ready(state: web::Data<AppState>) -> Result<Json<Value>, ApiError> {
    let report = health_report(&state).await;
    if report.get("upstream").and_then(|u| u.get("probe")).and_then(|p| p.get("reachable")).and_then(|v| v.as_bool()) != Some(true) {
        return Err(ApiError::unavailable(
            format!("{CODE_PREFIX}_UPSTREAM_UNAVAILABLE"),
            "The Nager.Date upstream is unreachable".to_string(),
        ));
    }
    Ok(Json(json!({
        "status": "ready",
        "upstream": "nager.date",
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })))
}

// ---------------------------------------------------------------- OpenAPI

#[derive(OpenApi)]
#[openapi(
    info(title = "public-holidays-api", version = "1.0.0", description = "Worldwide public holidays, working days and upcoming-holiday windows over Nager.Date Community v4."),
    paths(countries, working_days, upcoming, is_holiday, list, country_year, country_year_region, health, health_ready)
)]
pub struct HolidaysApi;

pub fn openapi_doc() -> utoipa::openapi::OpenApi {
    let mut doc = HolidaysApi::openapi();
    doc.info.title = "public-holidays-api".into();
    doc.servers = Some(vec![utoipa::openapi::Server::new(SERVICE_PREFIX)]);
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nager_holiday_entries() {
        let raw = json!({
            "date": "2026-12-09",
            "name": "Independence Day",
            "countryCode": "TZ",
            "nationalHoliday": true,
            "subdivisionCodes": null,
            "holidayTypes": ["Public", "Public"]
        });
        let h = parse_holiday(&raw, "TZ").unwrap();
        assert_eq!(h.scope, "national");
        assert!(h.subdivision_codes.is_empty());
        assert_eq!(h.types, vec!["Public"]);
        assert_eq!(h.day_of_week, 3); // Wednesday
    }

    #[test]
    fn parses_subdivision_holiday() {
        let raw = json!({
            "date": "2026-03-19",
            "name": "Regional holiday",
            "countryCode": "AT",
            "nationalHoliday": false,
            "subdivisionCodes": ["at-2", "AT-9"],
            "holidayTypes": ["Bank", "UnknownType"]
        });
        let h = parse_holiday(&raw, "AT").unwrap();
        assert_eq!(h.scope, "subdivision");
        assert_eq!(h.subdivision_codes, vec!["AT-2", "AT-9"]);
        // Unknown types are dropped, per the original's vocabulary contract.
        assert_eq!(h.types, vec!["Bank"]);
    }

    #[test]
    fn sorts_with_total_ties() {
        let mk = |date: &str, name: &str, cc: &str| Holiday {
            date: date.into(),
            name: name.into(),
            country_code: cc.into(),
            scope: "national",
            subdivision_codes: vec![],
            types: vec!["Public".into()],
            day_of_week: 0,
            is_weekend: false,
        };
        let sorted = sort_holidays(
            vec![mk("2026-10-12", "B", "US"), mk("2026-10-12", "A", "US"), mk("2026-01-01", "Z", "AT")],
            "date",
        );
        assert_eq!(sorted[0].name, "Z");
        assert_eq!(sorted[1].name, "A");
        assert_eq!(sorted[2].name, "B");
    }

    #[test]
    fn region_logic() {
        let h = json!({
            "date": "2026-03-19", "name": "X", "countryCode": "AT",
            "nationalHoliday": false, "subdivisionCodes": ["AT-2"], "holidayTypes": ["Bank"]
        });
        let items = parse_holiday_list(&json!([h]), "AT").unwrap();
        assert!(applies_to_region(&items[0], "AT-2"));
        assert!(!applies_to_region(&items[0], "AT-9"));
        assert_eq!(known_regions(&items), vec!["AT-2"]);
    }
}
