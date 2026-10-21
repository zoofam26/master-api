//! Route handlers for the weather service.

use std::sync::Arc;

use actix_web::web::{self, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use utoipa::{IntoParams, OpenApi};

use master_common::envelope::Envelope;
use master_common::error::ApiError;
use master_common::httpx;
use master_common::state::{cache_names, AppState};

use crate::{
    unit_meta, unit_params, wmo_condition, CODE_PREFIX, ELEVATION_PATH, FORECAST_PATH,
    ARCHIVE_PATH, GEOCODING_GET_PATH, GEOCODING_SEARCH_PATH, GEOCODE_SORTS, SERVICE_PREFIX,
    UNIT_SYSTEMS,
    UPSTREAM_OPEN_METEO, UPSTREAM_OPEN_METEO_ARCHIVE, UPSTREAM_OPEN_METEO_GEOCODING,
};

// ------------------------------------------------------------------ helpers

fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b',' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn validate_coords(latitude: f64, longitude: f64) -> Result<(), ApiError> {
    if !(-90.0..=90.0).contains(&latitude) {
        return Err(ApiError::validation(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "latitude must be a number between -90 and 90",
        ));
    }
    if !(-180.0..=180.0).contains(&longitude) {
        return Err(ApiError::validation(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "longitude must be a number between -180 and 180",
        ));
    }
    Ok(())
}

fn resolved_location(body: &Value) -> Value {
    json!({
        "latitude": body.get("latitude").cloned().unwrap_or(Value::Null),
        "longitude": body.get("longitude").cloned().unwrap_or(Value::Null),
        "elevation": body.get("elevation").cloned().unwrap_or(Value::Null),
        "timezone": body.get("timezone").cloned().unwrap_or(Value::Null),
        "timezone_abbreviation": body.get("timezone_abbreviation").cloned().unwrap_or(Value::Null),
        "utc_offset_seconds": body.get("utc_offset_seconds").cloned().unwrap_or(Value::Null),
    })
}

async fn cached_get(state: &AppState, cache: &str, key: &str, url: String) -> Result<(Value, bool), ApiError> {
    let cache = state.json_cache(cache);
    if let Some(v) = cache.get(key).await {
        return Ok((v.as_ref().clone(), true));
    }
    let body = httpx::get_json(state, &url, &[], CODE_PREFIX).await?.body;
    cache.insert(key.to_string(), Arc::new(body.clone())).await;
    Ok((body, false))
}

fn parse_var_list(csv: &Option<String>, allowed: &[&str], field: &str, max: usize) -> Result<Vec<String>, ApiError> {
    let Some(csv) = csv.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Vec::new());
    };
    let vars: Vec<String> = csv.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if vars.len() > max {
        return Err(ApiError::validation(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            format!("{field} accepts at most {max} variables"),
        ));
    }
    for v in &vars {
        if !allowed.contains(&v.as_str()) {
            return Err(ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_REQUEST"),
                format!("Unknown {field} variable \"{v}\""),
                json!({ "variable": v, "allowed": allowed }),
            ));
        }
    }
    Ok(vars)
}

fn time_series(body: &Value, kind: &str, vars: &[String], with_conditions: bool) -> Value {
    let time: Vec<Value> = body
        .pointer(&format!("/{kind}/time"))
        .and_then(|v| v.as_array())
        .map(|a| a.clone())
        .unwrap_or_default();
    let units = body.pointer(&format!("/{kind}_units")).cloned().unwrap_or(json!({}));
    let mut values = serde_json::Map::new();
    for var in vars {
        values.insert(var.clone(), body.pointer(&format!("/{kind}/{var}")).cloned().unwrap_or(Value::Null));
    }
    let mut out = json!({
        "time": time,
        "values": Value::Object(values),
        "units": units,
    });
    if with_conditions {
        let codes = body.pointer(&format!("/{kind}/weather_code")).and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let conditions: Vec<Value> = codes
            .iter()
            .map(|c| wmo_condition(c.as_i64()))
            .collect();
        out["conditions"] = Value::Array(conditions);
    }
    out
}

// -------------------------------------------------------------- query DTOs

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CurrentQuery {
    /// Latitude, -90..90.
    pub latitude: f64,
    /// Longitude, -180..180.
    pub longitude: f64,
    /// `standard` (default), `metric` or `imperial`.
    pub units: Option<String>,
    /// IANA timezone (`auto` by default).
    pub timezone: Option<String>,
    /// Comma-separated current variables.
    pub current: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ForecastQuery {
    pub latitude: f64,
    pub longitude: f64,
    pub units: Option<String>,
    pub timezone: Option<String>,
    pub current: Option<String>,
    /// Forecast days (0..=16).
    pub days: Option<u32>,
    pub past_days: Option<u32>,
    /// Comma-separated hourly variables.
    pub hourly: Option<String>,
    /// Comma-separated daily variables.
    pub daily: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GeocodeSearchQuery {
    /// Place name to search (required unless `name` given).
    pub q: Option<String>,
    /// Alias for `q`.
    pub name: Option<String>,
    /// 1..=100 (default 10).
    pub count: Option<usize>,
    pub offset: Option<usize>,
    /// ISO 639-1 language code (default `en`).
    pub language: Option<String>,
    /// Two-letter country filter.
    pub country_code: Option<String>,
    /// `relevance` (default), `population` or `name`.
    pub sort: Option<String>,
    pub min_population: Option<i64>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PlaceQuery {
    /// ISO 639-1 language code (default `en`).
    pub language: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ElevationQuery {
    /// Comma-separated latitudes.
    pub latitude: String,
    /// Comma-separated longitudes (same count as latitudes).
    pub longitude: String,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GridQuery {
    pub min_latitude: f64,
    pub min_longitude: f64,
    pub max_latitude: f64,
    pub max_longitude: f64,
    /// Grid step in degrees (default 1, min 0.25).
    pub resolution: Option<f64>,
    pub units: Option<String>,
    pub timezone: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ArchiveQuery {
    pub latitude: f64,
    pub longitude: f64,
    pub units: Option<String>,
    pub timezone: Option<String>,
    /// Archive start date (YYYY-MM-DD; default 1990-01-01).
    pub start_date: Option<String>,
    /// Archive end date (YYYY-MM-DD; default today-5).
    pub end_date: Option<String>,
    pub hourly: Option<String>,
    pub daily: Option<String>,
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    let scope = web::scope(SERVICE_PREFIX)
        .route("/current", web::get().to(current))
        .route("/forecast", web::get().to(forecast))
        .route("/geocode/search", web::get().to(geocode_search))
        .route("/places/{id}", web::get().to(place))
        .route("/elevation", web::get().to(elevation))
        .route("/grid", web::get().to(grid))
        .route("/archive", web::get().to(archive))
        .route("/health/ready", web::get().to(health_ready))
        .route("/health", web::get().to(health));
    cfg.service(scope);
}

// ---------------------------------------------------------------- handlers

/// `GET /weather/current`
#[utoipa::path(get, path = "/weather/current", context_path = "/weather", params(CurrentQuery),
    responses((status = 200, description = "Current conditions")))]
async fn current(
    state: web::Data<AppState>,
    query: web::Query<CurrentQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    validate_coords(query.latitude, query.longitude)?;
    let system = query.units.as_deref().unwrap_or("standard");
    if !UNIT_SYSTEMS.contains(&system) {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            format!("units must be one of: {}", UNIT_SYSTEMS.join(", ")),
            json!({ "units": system, "allowed": UNIT_SYSTEMS }),
        ));
    }
    let vars = parse_var_list(&query.current, crate::CURRENT_VARIABLES, "current", 15)?;
    let current_vars: Vec<&str> = if vars.is_empty() {
        crate::CURRENT_VARIABLES.to_vec()
    } else {
        vars.iter().map(String::as_str).collect()
    };

    let key = format!("current:{:?}:{:?}:{:?}:{:?}", query.latitude, query.longitude, system, current_vars);
    let url = format!(
        "{UPSTREAM_OPEN_METEO}{FORECAST_PATH}?latitude={}&longitude={}&current={}&timezone={}&{}",
        query.latitude,
        query.longitude,
        enc(&current_vars.join(",")),
        enc(query.timezone.as_deref().unwrap_or("auto")),
        unit_params(system).iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&"),
    );
    let (body, cached) = cached_get(&state, cache_names::WEATHER, &key, url).await?;
    let current = body.get("current").cloned().unwrap_or(json!({}));
    let code = current.get("weather_code").and_then(|v| v.as_i64());

    let data = json!({
        "location": resolved_location(&body),
        "observed_at": current.get("time").cloned().unwrap_or(Value::Null),
        "interval_seconds": current.get("interval").and_then(|v| v.as_i64()),
        "units": unit_meta(Some(system)),
        "temperature": current.get("temperature_2m").cloned().unwrap_or(Value::Null),
        "apparent_temperature": current.get("apparent_temperature").cloned().unwrap_or(Value::Null),
        "relative_humidity": current.get("relative_humidity_2m").cloned().unwrap_or(Value::Null),
        "precipitation": current.get("precipitation").cloned().unwrap_or(Value::Null),
        "is_day": current.get("is_day").and_then(|v| v.as_i64()).map(|v| v != 0),
        "cloud_cover": current.get("cloud_cover").cloned().unwrap_or(Value::Null),
        "pressure_msl": current.get("pressure_msl").cloned().unwrap_or(Value::Null),
        "wind_speed": current.get("wind_speed_10m").cloned().unwrap_or(Value::Null),
        "wind_direction": current.get("wind_direction_10m").cloned().unwrap_or(Value::Null),
        "wind_gusts": current.get("wind_gusts_10m").cloned().unwrap_or(Value::Null),
        "weather": wmo_condition(code),
    });
    let body = Envelope::ok(
        data,
        json!({
            "durationMs": started.elapsed().as_millis() as u64,
            "cached": cached,
            "upstream": UPSTREAM_OPEN_METEO,
        }),
    );
    Ok(Json(body.to_json()))
}

/// `GET /weather/forecast`
#[utoipa::path(get, path = "/weather/forecast", context_path = "/weather", params(ForecastQuery),
    responses((status = 200, description = "Hourly/daily forecast")))]
async fn forecast(
    state: web::Data<AppState>,
    query: web::Query<ForecastQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    validate_coords(query.latitude, query.longitude)?;
    let system = query.units.as_deref().unwrap_or("standard");
    if !UNIT_SYSTEMS.contains(&system) {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            format!("units must be one of: {}", UNIT_SYSTEMS.join(", ")),
            json!({ "units": system, "allowed": UNIT_SYSTEMS }),
        ));
    }
    let days = query.days.unwrap_or(7).min(16);
    let past_days = query.past_days.unwrap_or(0).min(92);
    let hourly_vars = parse_var_list(&query.hourly, crate::FORECAST_HOURLY_VARIABLES, "hourly", 40)?;
    let daily_vars = parse_var_list(&query.daily, crate::FORECAST_DAILY_VARIABLES, "daily", 25)?;

    let key = format!(
        "forecast:{:?}:{:?}:{:?}:{days}:{past_days}:{:?}:{:?}",
        query.latitude, query.longitude, system, hourly_vars, daily_vars
    );
    let mut url = format!(
        "{UPSTREAM_OPEN_METEO}{FORECAST_PATH}?latitude={}&longitude={}&forecast_days={days}&past_days={past_days}&timezone={}&{}",
        query.latitude,
        query.longitude,
        enc(query.timezone.as_deref().unwrap_or("auto")),
        unit_params(system).iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&"),
    );
    let current_vars: Vec<String> = match &query.current {
        Some(_) => {
            let vars = parse_var_list(&query.current, crate::CURRENT_VARIABLES, "current", 15)?;
            if vars.is_empty() {
                crate::CURRENT_VARIABLES.iter().map(|s| s.to_string()).collect()
            } else {
                vars
            }
        }
        None => Vec::new(),
    };
    if !current_vars.is_empty() {
        url.push_str(&format!("&current={}", enc(&current_vars.join(","))));
    }
    if !hourly_vars.is_empty() {
        url.push_str(&format!("&hourly={}", enc(&hourly_vars.join(","))));
    }
    if !daily_vars.is_empty() {
        url.push_str(&format!("&daily={}", enc(&daily_vars.join(","))));
    }

    let (body, cached) = cached_get(&state, cache_names::WEATHER, &key, url).await?;
    let times: Vec<Value> = body
        .pointer("/hourly/time")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let range = json!({
        "start": times.first().cloned().unwrap_or(Value::Null),
        "end": times.last().cloned().unwrap_or(Value::Null),
    });

    let data = json!({
        "location": resolved_location(&body),
        "units": unit_meta(Some(system)),
        "generated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "range": range,
        "hourly": if hourly_vars.is_empty() { Value::Null } else { time_series(&body, "hourly", &hourly_vars, true) },
        "daily": if daily_vars.is_empty() { Value::Null } else { time_series(&body, "daily", &daily_vars, true) },
    });
    let body = Envelope::ok(
        data,
        json!({
            "durationMs": started.elapsed().as_millis() as u64,
            "cached": cached,
            "upstream": UPSTREAM_OPEN_METEO,
        }),
    );
    Ok(Json(body.to_json()))
}

/// `GET /weather/geocode/search`
#[utoipa::path(get, path = "/weather/geocode/search", context_path = "/weather", params(GeocodeSearchQuery),
    responses((status = 200, description = "Place search")))]
async fn geocode_search(
    state: web::Data<AppState>,
    query: web::Query<GeocodeSearchQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let term = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| query.name.as_deref().map(str::trim).filter(|s| !s.is_empty()));
    let Some(term) = term else {
        return Err(ApiError::validation(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "q is required, e.g. /weather/geocode/search?q=Dar es Salaam",
        ));
    };
    let sort = match query.sort.as_deref() {
        None => "relevance".to_string(),
        Some(s) if GEOCODE_SORTS.contains(&s) => s.to_string(),
        Some(s) => {
            return Err(ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_REQUEST"),
                format!("sort must be one of: {}", GEOCODE_SORTS.join(", ")),
                json!({ "sort": s, "allowed": GEOCODE_SORTS }),
            ));
        }
    };
    let count = query.count.unwrap_or(10).clamp(1, 100);
    let offset = query.offset.unwrap_or(0);

    let key = format!("geocode:{term}:{:?}:{:?}:{:?}:{:?}", query.language, query.country_code, query.min_population, sort);
    let url = format!(
        "{UPSTREAM_OPEN_METEO_GEOCODING}{GEOCODING_SEARCH_PATH}?name={}&count=100&language={}&format=json",
        enc(term),
        enc(query.language.as_deref().unwrap_or("en")),
    );
    let (body, cached) = cached_get(&state, cache_names::GEOCODING, &key, url).await?;
    let mut results: Vec<Value> = body
        .get("results")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(place_record).collect())
        .unwrap_or_default();

    // Client-side filters + sorting (the upstream has no server-side sort).
    if let Some(cc) = query.country_code.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let upper = cc.to_uppercase();
        results.retain(|p| p.get("country_code").and_then(|v| v.as_str()) == Some(upper.as_str()));
    }
    if let Some(min) = query.min_population {
        results.retain(|p| p.get("population").and_then(|v| v.as_i64()).map(|v| v >= min).unwrap_or(false));
    }
    match sort.as_str() {
        "population" => results.sort_by(|a, b| {
            let pa = a.get("population").and_then(|v| v.as_i64()).unwrap_or(0);
            let pb = b.get("population").and_then(|v| v.as_i64()).unwrap_or(0);
            pb.cmp(&pa).then_with(|| {
                let na = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let nb = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
                na.cmp(nb)
            })
        }),
        "name" => results.sort_by(|a, b| {
            let na = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let nb = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
            na.cmp(nb)
        }),
        _ => {}
    }

    let matched = results.len();
    let page: Vec<Value> = results.into_iter().skip(offset).take(count).collect();
    let returned = page.len();

    let data = json!({
        "query": term,
        "results": page,
        "pagination": {
            "offset": offset,
            "limit": count,
            "returned": returned,
            "matched": matched,
            "has_more": offset + returned < matched,
            "upstream_cap": 100,
        },
        "sort": sort,
    });
    let body = Envelope::ok(
        data,
        json!({
            "durationMs": started.elapsed().as_millis() as u64,
            "cached": cached,
            "upstream": UPSTREAM_OPEN_METEO_GEOCODING,
        }),
    );
    Ok(Json(body.to_json()))
}

fn place_record(p: &Value) -> Value {
    json!({
        "id": p.get("id").cloned().unwrap_or(Value::Null),
        "name": p.get("name").cloned().unwrap_or(Value::Null),
        "latitude": p.get("latitude").cloned().unwrap_or(Value::Null),
        "longitude": p.get("longitude").cloned().unwrap_or(Value::Null),
        "elevation": p.get("elevation").cloned().unwrap_or(Value::Null),
        "timezone": p.get("timezone").cloned().unwrap_or(Value::Null),
        "feature_code": p.get("feature_code").cloned().unwrap_or(Value::Null),
        "country": p.get("country").cloned().unwrap_or(Value::Null),
        "country_code": p.get("country_code").cloned().unwrap_or(Value::Null),
        "admin1": p.get("admin1").cloned().unwrap_or(Value::Null),
        "admin2": p.get("admin2").cloned().unwrap_or(Value::Null),
        "admin3": p.get("admin3").cloned().unwrap_or(Value::Null),
        "population": p.get("population").cloned().unwrap_or(Value::Null),
        "postcodes": p.get("postcodes").cloned().unwrap_or(json!([])),
    })
}

/// `GET /weather/places/{id}`
#[utoipa::path(get, path = "/weather/places/{id}", context_path = "/weather",
    params(("id" = i64, Path, description = "GeoNames id, e.g. 160263 (Dar es Salaam)"), PlaceQuery),
    responses((status = 200, description = "One place"), (status = 404, description = "Unknown place")))]
async fn place(
    state: web::Data<AppState>,
    path: web::Path<i64>,
    query: web::Query<PlaceQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let id = path.into_inner();
    let key = format!("place:{id}:{:?}", query.language);
    let url = format!(
        "{UPSTREAM_OPEN_METEO_GEOCODING}{GEOCODING_GET_PATH}?id={id}&language={}&format=json",
        enc(query.language.as_deref().unwrap_or("en")),
    );
    let (body, cached) = cached_get(&state, cache_names::GEOCODING, &key, url).await?;
    if body.get("id").is_none() {
        return Err(ApiError::not_found_details(
            format!("{CODE_PREFIX}_NOT_FOUND"),
            format!("No place with GeoNames id {id}"),
            json!({ "id": id }),
        ));
    }
    let body = Envelope::ok(
        place_record(&body),
        json!({
            "durationMs": started.elapsed().as_millis() as u64,
            "cached": cached,
            "upstream": UPSTREAM_OPEN_METEO_GEOCODING,
        }),
    );
    Ok(Json(body.to_json()))
}

/// `GET /weather/elevation`
#[utoipa::path(get, path = "/weather/elevation", context_path = "/weather", params(ElevationQuery),
    responses((status = 200, description = "Elevation lookup")))]
async fn elevation(
    state: web::Data<AppState>,
    query: web::Query<ElevationQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let lats: Vec<f64> = query
        .latitude
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let lons: Vec<f64> = query
        .longitude
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    if lats.is_empty() || lons.is_empty() || lats.len() != lons.len() {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "latitude and longitude must be equal-length comma-separated lists of coordinates",
            json!({ "latitudes": lats.len(), "longitudes": lons.len() }),
        ));
    }
    for (lat, lon) in lats.iter().zip(lons.iter()) {
        validate_coords(*lat, *lon)?;
    }
    if lats.len() > 100 {
        return Err(ApiError::validation(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "at most 100 points per request",
        ));
    }

    let key = format!("elevation:{lats:?}:{lons:?}");
    let lat_params: Vec<String> = lats.iter().map(|v| v.to_string()).collect();
    let lon_params: Vec<String> = lons.iter().map(|v| v.to_string()).collect();
    let url = format!(
        "{UPSTREAM_OPEN_METEO}{ELEVATION_PATH}?latitude={}&longitude={}",
        lat_params.join(","), // commas pass through enc()
        lon_params.join(","),
    );
    let (body, cached) = cached_get(&state, cache_names::WEATHER, &key, url).await?;
    let elevations = body.get("elevation").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let points: Vec<Value> = lats
        .iter()
        .zip(lons.iter())
        .enumerate()
        .map(|(i, (lat, lon))| json!({
            "latitude": lat,
            "longitude": lon,
            "elevation": elevations.get(i).cloned().unwrap_or(Value::Null),
        }))
        .collect();

    let body = Envelope::ok(
        json!({ "points": points, "units": { "elevation": "m" } }),
        json!({
            "durationMs": started.elapsed().as_millis() as u64,
            "cached": cached,
            "upstream": UPSTREAM_OPEN_METEO,
        }),
    );
    Ok(Json(body.to_json()))
}

/// `GET /weather/grid` — current conditions across a bbox grid.
#[utoipa::path(get, path = "/weather/grid", context_path = "/weather", params(GridQuery),
    responses((status = 200, description = "Grid of current conditions")))]
async fn grid(
    state: web::Data<AppState>,
    query: web::Query<GridQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let resolution = query.resolution.unwrap_or(1.0);
    if !(0.25..=10.0).contains(&resolution) {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "resolution must be between 0.25 and 10 degrees",
            json!({ "resolution": resolution }),
        ));
    }
    if query.min_latitude > query.max_latitude || query.min_longitude > query.max_longitude {
        return Err(ApiError::validation(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "min values must not exceed max values",
        ));
    }
    validate_coords(query.min_latitude, query.min_longitude)?;
    validate_coords(query.max_latitude, query.max_longitude)?;

    let system = query.units.as_deref().unwrap_or("standard");
    if !UNIT_SYSTEMS.contains(&system) {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            format!("units must be one of: {}", UNIT_SYSTEMS.join(", ")),
            json!({ "units": system, "allowed": UNIT_SYSTEMS }),
        ));
    }

    let mut lats = Vec::new();
    let mut lat = query.min_latitude;
    while lat <= query.max_latitude + 1e-9 {
        lats.push((lat * 1000.0).round() / 1000.0);
        lat += resolution;
    }
    let mut lons = Vec::new();
    let mut lon = query.min_longitude;
    while lon <= query.max_longitude + 1e-9 {
        lons.push((lon * 1000.0).round() / 1000.0);
        lon += resolution;
    }
    let count = lats.len() * lons.len();
    if count == 0 || count > 100 {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            format!("the requested grid would cover {count} points; the maximum is 100 — widen resolution or narrow the bbox"),
            json!({ "count": count }),
        ));
    }

    let key = format!("grid:{:?}:{:?}:{:?}:{:?}:{:?}", lats, lons, system, resolution, query.timezone);
    let cache = state.json_cache(cache_names::WEATHER);
    if let Some(v) = cache.get(&key).await {
        let body = Envelope::ok(
            v.as_ref().clone(),
            json!({
                "durationMs": started.elapsed().as_millis() as u64,
                "cached": true,
                "upstream": UPSTREAM_OPEN_METEO,
            }),
        );
        return Ok(Json(body.to_json()));
    }

    // Bounded sequential fan-out keeps us inside Open-Meteo's free-tier norms.
    let mut points: Vec<Value> = Vec::with_capacity(count);
    for lat in &lats {
        for lon in &lons {
            let url = format!(
                "{UPSTREAM_OPEN_METEO}{FORECAST_PATH}?latitude={lat}&longitude={lon}&current=temperature_2m,weather_code&timezone={}&{}",
                enc(query.timezone.as_deref().unwrap_or("auto")),
                unit_params(system).iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&"),
            );
            match httpx::get_json(&state, &url, &[], CODE_PREFIX).await {
                Ok(t) => {
                    let current = t.body.get("current").cloned().unwrap_or(json!({}));
                    points.push(json!({
                        "latitude": lat,
                        "longitude": lon,
                        "grid_latitude": t.body.get("latitude").cloned().unwrap_or(Value::Null),
                        "grid_longitude": t.body.get("longitude").cloned().unwrap_or(Value::Null),
                        "elevation": t.body.get("elevation").cloned().unwrap_or(Value::Null),
                        "timezone": t.body.get("timezone").cloned().unwrap_or(Value::Null),
                        "observed_at": current.get("time").cloned().unwrap_or(Value::Null),
                        "temperature": current.get("temperature_2m").cloned().unwrap_or(Value::Null),
                        "weather": wmo_condition(current.get("weather_code").and_then(|v| v.as_i64())),
                    }));
                }
                Err(_) => points.push(json!({
                    "latitude": lat, "longitude": lon,
                    "grid_latitude": Value::Null, "grid_longitude": Value::Null,
                    "elevation": Value::Null, "timezone": Value::Null,
                    "observed_at": Value::Null, "temperature": Value::Null,
                    "weather": wmo_condition(None),
                })),
            }
        }
    }

    let data = json!({
        "bbox": {
            "min_latitude": query.min_latitude,
            "min_longitude": query.min_longitude,
            "max_latitude": query.max_latitude,
            "max_longitude": query.max_longitude,
        },
        "resolution_degrees": resolution,
        "units": unit_meta(Some(system)),
        "count": points.len(),
        "points": points,
    });
    cache.insert(key, Arc::new(data.clone())).await;
    let body = Envelope::ok(
        data,
        json!({
            "durationMs": started.elapsed().as_millis() as u64,
            "cached": false,
            "upstream": UPSTREAM_OPEN_METEO,
        }),
    );
    Ok(Json(body.to_json()))
}

/// `GET /weather/archive`
#[utoipa::path(get, path = "/weather/archive", context_path = "/weather", params(ArchiveQuery),
    responses((status = 200, description = "ERA5/IFS reanalysis archive")))]
async fn archive(
    state: web::Data<AppState>,
    query: web::Query<ArchiveQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    validate_coords(query.latitude, query.longitude)?;
    let system = query.units.as_deref().unwrap_or("standard");
    if !UNIT_SYSTEMS.contains(&system) {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            format!("units must be one of: {}", UNIT_SYSTEMS.join(", ")),
            json!({ "units": system, "allowed": UNIT_SYSTEMS }),
        ));
    }
    let start = query.start_date.clone().unwrap_or_else(|| "1990-01-01".into());
    let end = query.end_date.clone().unwrap_or_else(|| {
        (chrono::Utc::now() - chrono::Duration::days(5)).format("%Y-%m-%d").to_string()
    });
    for (v, field) in [(&start, "start_date"), (&end, "end_date")] {
        if chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").is_err() {
            return Err(ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_REQUEST"),
                format!("{field} must be a real calendar date formatted YYYY-MM-DD"),
                json!({ field: v }),
            ));
        }
    }
    if start > end {
        return Err(ApiError::validation(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "start_date must not be after end_date",
        ));
    }
    let hourly_vars = parse_var_list(&query.hourly, crate::ARCHIVE_HOURLY_VARIABLES, "hourly", 40)?;
    let daily_vars = parse_var_list(&query.daily, crate::ARCHIVE_DAILY_VARIABLES, "daily", 25)?;

    let key = format!("archive:{:?}:{:?}:{:?}:{start}:{end}:{:?}:{:?}", query.latitude, query.longitude, system, hourly_vars, daily_vars);
    let mut url = format!(
        "{UPSTREAM_OPEN_METEO_ARCHIVE}{ARCHIVE_PATH}?latitude={}&longitude={}&start_date={start}&end_date={end}&timezone={}&{}",
        query.latitude,
        query.longitude,
        enc(query.timezone.as_deref().unwrap_or("auto")),
        unit_params(system).iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&"),
    );
    if !hourly_vars.is_empty() {
        url.push_str(&format!("&hourly={}", enc(&hourly_vars.join(","))));
    }
    if !daily_vars.is_empty() {
        url.push_str(&format!("&daily={}", enc(&daily_vars.join(","))));
    }

    let (body, cached) = cached_get(&state, cache_names::WEATHER, &key, url).await?;
    let daily_times: Vec<Value> = body
        .pointer("/daily/time")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let data = json!({
        "location": resolved_location(&body),
        "units": unit_meta(Some(system)),
        "generated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "range": { "start_date": start, "end_date": end },
        "hourly": if hourly_vars.is_empty() { Value::Null } else { time_series(&body, "hourly", &hourly_vars, true) },
        "daily": if daily_vars.is_empty() { Value::Null } else { time_series(&body, "daily", &daily_vars, true) },
    });
    let _ = daily_times;
    let body = Envelope::ok(
        data,
        json!({
            "durationMs": started.elapsed().as_millis() as u64,
            "cached": cached,
            "upstream": UPSTREAM_OPEN_METEO_ARCHIVE,
        }),
    );
    Ok(Json(body.to_json()))
}

// ------------------------------------------------------------------ health

async fn health_report(state: &AppState) -> Value {
    let probe_key = "weather_probe";
    let probe_cache = state.json_cache(cache_names::HEALTH_PROBES);
    let cached = probe_cache.get(probe_key).await;
    let (probe, was_cached) = match cached {
        Some(v) => (v.as_ref().clone(), true),
        None => {
            // Cheapest real call: elevation for one point.
            let url = format!("{UPSTREAM_OPEN_METEO}{ELEVATION_PATH}?latitude=46.9480&longitude=7.4474");
            let report: Value = match httpx::get_json(state, &url, &[], CODE_PREFIX).await {
                Ok(t) => json!({
                    "reachable": true,
                    "probeElevationM": t.body.get("elevation").and_then(|v| v.as_array()).and_then(|a| a.first().cloned()).unwrap_or(Value::Null),
                }),
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
        "upstream": { "name": "open-meteo", "probe": probe, "cached": was_cached },
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
}

/// `GET /weather/health`
#[utoipa::path(get, path = "/weather/health", context_path = "/weather",
    responses((status = 200, description = "Liveness plus upstream probe")))]
async fn health(state: web::Data<AppState>) -> Json<Value> {
    Json(health_report(&state).await)
}

/// `GET /weather/health/ready`
#[utoipa::path(get, path = "/weather/health/ready", context_path = "/weather",
    responses((status = 200, description = "Upstream reachable"), (status = 503, description = "Upstream down")))]
async fn health_ready(state: web::Data<AppState>) -> Result<Json<Value>, ApiError> {
    let report = health_report(&state).await;
    if report.get("upstream").and_then(|u| u.get("probe")).and_then(|p| p.get("reachable")).and_then(|v| v.as_bool()) != Some(true) {
        return Err(ApiError::unavailable(
            format!("{CODE_PREFIX}_UPSTREAM_UNAVAILABLE"),
            "The Open-Meteo upstream is unreachable".to_string(),
        ));
    }
    Ok(Json(json!({
        "status": "ready",
        "upstream": "open-meteo",
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })))
}

// ---------------------------------------------------------------- OpenAPI

#[derive(OpenApi)]
#[openapi(
    info(title = "weather-api", version = "1.0.0", description = "Weather forecast, archive, geocoding, elevation and grid over Open-Meteo."),
    paths(current, forecast, geocode_search, place, elevation, grid, archive, health, health_ready)
)]
pub struct WeatherApi;

pub fn openapi_doc() -> utoipa::openapi::OpenApi {
    let mut doc = WeatherApi::openapi();
    doc.info.title = "weather-api".into();
    doc.servers = Some(vec![utoipa::openapi::Server::new(SERVICE_PREFIX)]);
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_systems() {
        let m = unit_meta(Some("metric"));
        assert_eq!(m["wind_speed"], "m/s");
        let imp = unit_meta(Some("imperial"));
        assert_eq!(imp["temperature"], "°F");
        let std = unit_meta(None);
        assert_eq!(std["wind_speed"], "km/h");
        assert_eq!(unit_meta(Some("bogus"))["system"], "standard");
    }

    #[test]
    fn wmo_codes() {
        assert_eq!(wmo_condition(Some(0))["description"], "Clear sky");
        assert_eq!(wmo_condition(Some(95))["description"], "Thunderstorm");
        assert_eq!(wmo_condition(Some(9999))["description"], "Unknown");
        assert_eq!(wmo_condition(None)["code"], Value::Null);
    }

    #[test]
    fn variable_validation() {
        assert!(parse_var_list(&Some("temperature_2m,weather_code".into()), crate::CURRENT_VARIABLES, "current", 15).is_ok());
        assert!(parse_var_list(&Some("bogus".into()), crate::CURRENT_VARIABLES, "current", 15).is_err());
        assert!(parse_var_list(&None, crate::CURRENT_VARIABLES, "current", 15).unwrap().is_empty());
    }

    #[test]
    fn place_shape_has_all_documented_fields() {
        let p = place_record(&json!({ "id": 160263, "name": "Dar es Salaam", "latitude": -6.8, "longitude": 39.3 }));
        for field in ["id", "name", "latitude", "longitude", "elevation", "timezone", "feature_code", "country", "country_code", "admin1", "admin2", "admin3", "population", "postcodes"] {
            assert!(p.get(field).is_some(), "missing {field}");
        }
    }
}
