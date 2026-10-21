//! Open-Meteo air-quality sub-service (`/earthquake/air-quality`,
//! `/earthquake/air-quality/aqi`), including the EEA/EPA AQI band tables.

use actix_web::web::{self, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use utoipa::IntoParams;

use master_common::envelope::Envelope;
use master_common::error::ApiError;
use master_common::httpx;
use master_common::state::{cache_names, AppState};

use crate::{CODE_PREFIX, UPSTREAM_OPEN_METEO_AQ};

pub const EUROPEAN_AQI_BANDS: &[(f64, &str, &str)] = &[
    (20.0, "good", "Air quality is good: air pollution poses little or no risk."),
    (40.0, "fair", "Air quality is fair: air pollution is tolerable and poses a small risk."),
    (60.0, "moderate", "Air quality is moderate: air pollution may cause discomfort for sensitive groups."),
    (80.0, "poor", "Air quality is poor: air pollution may cause problems for sensitive groups."),
    (100.0, "very poor", "Air quality is very poor: health effects are more likely, especially for sensitive groups."),
    (f64::INFINITY, "extremely poor", "Air quality is extremely poor: health effects are very likely for everyone."),
];

pub const US_AQI_BANDS: &[(f64, &str, &str)] = &[
    (50.0, "good", "Air quality is satisfactory and poses little or no risk."),
    (100.0, "moderate", "Air quality is acceptable; unusually sensitive people may be affected."),
    (150.0, "unhealthy-for-sensitive-groups", "Sensitive groups may experience health effects."),
    (200.0, "unhealthy", "Everyone may begin to experience health effects."),
    (300.0, "very unhealthy", "Health alert: the risk of health effects is increased for everyone."),
    (f64::INFINITY, "hazardous", "Health warning of emergency conditions: everyone is more likely to be affected."),
];

/// Classifies one index value into a named band; `None` when no value exists
/// (Europe-only indices outside the European domain).
pub fn classify_aqi(index: Option<f64>, scheme: &str) -> Option<Value> {
    let index = index.filter(|v| v.is_finite())?;
    let bands = if scheme == "european" { EUROPEAN_AQI_BANDS } else { US_AQI_BANDS };
    let (upper, band, description) = bands
        .iter()
        .find(|(ub, _, _)| index <= *ub)
        .unwrap_or_else(|| bands.last().unwrap());
    Some(json!({
        "index": index,
        "band": band,
        "bandDescription": description,
        "upperBound": if upper.is_finite() { json!(upper) } else { Value::Null },
    }))
}

/// Percent-encode a URL component.
pub fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub const DEFAULT_VARIABLES: &[&str] = &[
    "pm10", "pm2_5", "carbon_monoxide", "nitrogen_dioxide", "sulphur_dioxide", "ozone",
    "aerosol_optical_depth", "dust", "uv_index", "uv_index_clear_sky",
    "ammonia", "methane", "nitrogen_monoxide", "non_methane_vocs", "organic_gaseous_carbon",
    "reactive_gaseous_carbon", "sulphur_dioxide_aod", "european_aqi", "us_aqi",
];

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AirQualityQuery {
    /// Latitude, -90..90.
    pub latitude: f64,
    /// Longitude, -180..180.
    pub longitude: f64,
    /// Comma-separated hourly variables (default: the full pollutant set).
    pub hourly: Option<String>,
    /// Forecast days (1..=5).
    pub forecast_days: Option<u32>,
    /// Past days (0..=92).
    pub past_days: Option<u32>,
    /// CAMS model domain: `auto`, `cams_global`, `cams_europe`.
    pub domains: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AirQualityAqiQuery {
    /// Latitude, -90..90.
    pub latitude: f64,
    /// Longitude, -180..180.
    pub longitude: f64,
    /// CAMS model domain: `auto`, `cams_global`, `cams_europe`.
    pub domains: Option<String>,
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

struct AqPayload {
    body: Value,
    cached: bool,
}

async fn fetch_air_quality(
    state: &AppState,
    latitude: f64,
    longitude: f64,
    hourly: &[&str],
    forecast_days: u32,
    past_days: u32,
    domains: &str,
) -> Result<AqPayload, ApiError> {
    let cache_key = format!(
        "aq:{latitude}:{longitude}:{:?}:{forecast_days}:{past_days}:{domains}",
        hourly
    );
    let cache = state.json_cache(cache_names::AIR_QUALITY);
    if let Some(v) = cache.get(&cache_key).await {
        return Ok(AqPayload { body: v.as_ref().clone(), cached: true });
    }

    let url = format!(
        "{UPSTREAM_OPEN_METEO_AQ}?latitude={latitude}&longitude={longitude}&hourly={}&forecast_days={forecast_days}&past_days={past_days}&domains={}&timeformat=unixtime",
        enc(&hourly.join(",")),
        enc(domains),
    );
    let body = httpx::get_json(state, &url, &[], CODE_PREFIX).await?.body;

    // Shape the upstream payload into the service's own record.
    let hourly_arr = body
        .pointer("/hourly/time")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut hours: Vec<Value> = Vec::with_capacity(hourly_arr.len());
    for (idx, t) in hourly_arr.iter().enumerate() {
        let time = t
            .as_i64()
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            .unwrap_or_default();
        let mut values = serde_json::Map::new();
        for var in hourly {
            let v = body
                .pointer(&format!("/hourly/{var}"))
                .and_then(|arr| arr.get(idx))
                .cloned()
                .unwrap_or(Value::Null);
            values.insert(var.to_string(), v);
        }
        hours.push(json!({ "time": time, "values": Value::Object(values) }));
    }
    let units = body.get("hourly_units").cloned().unwrap_or(json!({}));
    let variables: Vec<Value> = hourly
        .iter()
        .map(|v| {
            json!({
                "name": v,
                "unit": units.get(*v).and_then(|u| u.as_str()).unwrap_or(""),
            })
        })
        .collect();
    let shaped = json!({
        "gridCell": {
            "latitude": body.get("latitude").cloned().unwrap_or(Value::Null),
            "longitude": body.get("longitude").cloned().unwrap_or(Value::Null),
            "elevation": body.get("elevation").cloned().unwrap_or(Value::Null),
        },
        "units": units,
        "variables": variables,
        "hourly": hours,
    });
    cache.insert(cache_key, std::sync::Arc::new(shaped.clone())).await;
    Ok(AqPayload { body: shaped, cached: false })
}

/// `GET /earthquake/air-quality` — hourly forecast.
#[utoipa::path(get, path = "/earthquake/air-quality", context_path = "/earthquake", params(AirQualityQuery),
    responses((status = 200, description = "Hourly air-quality forecast")))]
pub async fn forecast(
    state: web::Data<AppState>,
    query: web::Query<AirQualityQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    validate_coords(query.latitude, query.longitude)?;
    let forecast_days = query.forecast_days.unwrap_or(5).clamp(1, 5);
    let past_days = query.past_days.unwrap_or(0).clamp(0, 92);
    let domain = query.domains.clone().unwrap_or_else(|| "auto".into());
    if !matches!(domain.as_str(), "auto" | "cams_global" | "cams_europe") {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "domains must be one of: auto, cams_global, cams_europe",
            json!({ "domains": domain }),
        ));
    }
    let requested_vars: Vec<String> = match &query.hourly {
        Some(csv) if !csv.trim().is_empty() => csv.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        _ => DEFAULT_VARIABLES.iter().map(|s| s.to_string()).collect(),
    };
    if requested_vars.len() > 30 {
        return Err(ApiError::validation(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "hourly accepts at most 30 variables",
        ));
    }
    let var_refs: Vec<&str> = requested_vars.iter().map(String::as_str).collect();

    let payload =
        fetch_air_quality(&state, query.latitude, query.longitude, &var_refs, forecast_days, past_days, &domain).await?;
    let count = payload
        .body
        .get("hourly")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let window = json!({
        "starttime": payload.body.pointer("/hourly/0/time").cloned().unwrap_or(Value::Null),
        "endtime": payload
            .body
            .pointer(&format!("/hourly/{}/time", count.saturating_sub(1)))
            .cloned()
            .unwrap_or(Value::Null),
    });

    let body = Envelope::ok(
        json!({
            "requested": { "latitude": query.latitude, "longitude": query.longitude },
            "gridCell": payload.body.get("gridCell"),
            "domain": domain,
            "units": payload.body.get("units"),
            "variables": payload.body.get("variables"),
            "window": window,
            "hourly": payload.body.get("hourly"),
        }),
        json!({
            "count": count,
            "forecastDays": forecast_days,
            "pastDays": past_days,
            "upstream": UPSTREAM_OPEN_METEO_AQ,
            "durationMs": started.elapsed().as_millis() as u64,
            "cached": payload.cached,
        }),
    );
    Ok(Json(body.to_json()))
}

/// `GET /earthquake/air-quality/aqi` — current AQI with named bands.
#[utoipa::path(get, path = "/earthquake/air-quality/aqi", context_path = "/earthquake", params(AirQualityAqiQuery),
    responses((status = 200, description = "Current AQI classification")))]
pub async fn aqi(
    state: web::Data<AppState>,
    query: web::Query<AirQualityAqiQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    validate_coords(query.latitude, query.longitude)?;
    let domain = query.domains.clone().unwrap_or_else(|| "auto".into());
    let payload = fetch_air_quality(
        &state,
        query.latitude,
        query.longitude,
        &["european_aqi", "us_aqi"],
        1,
        0,
        &domain,
    )
    .await?;

    // "Current" = the first hour at or after now.
    let now = chrono::Utc::now().timestamp();
    let hours = payload.body.get("hourly").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let current = hours
        .iter()
        .find(|h| {
            h.get("time")
                .and_then(|t| t.as_str())
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.timestamp() >= now)
                .unwrap_or(false)
        })
        .or_else(|| hours.first())
        .cloned()
        .unwrap_or(Value::Null);
    let values = current.get("values").cloned().unwrap_or(json!({}));
    let time = current.get("time").cloned().unwrap_or(Value::Null);

    let body = Envelope::ok(
        json!({
            "requested": { "latitude": query.latitude, "longitude": query.longitude },
            "gridCell": payload.body.get("gridCell"),
            "domain": domain,
            "time": time,
            "european": classify_aqi(values.get("european_aqi").and_then(|v| v.as_f64()), "european"),
            "us": classify_aqi(values.get("us_aqi").and_then(|v| v.as_f64()), "us"),
        }),
        json!({
            "upstream": UPSTREAM_OPEN_METEO_AQ,
            "durationMs": started.elapsed().as_millis() as u64,
            "cached": payload.cached,
        }),
    );
    Ok(Json(body.to_json()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn european_bands() {
        assert_eq!(classify_aqi(Some(10.0), "european").unwrap()["band"], "good");
        assert_eq!(classify_aqi(Some(45.0), "european").unwrap()["band"], "moderate");
        assert_eq!(classify_aqi(Some(26.0), "european").unwrap()["band"], "fair");
        assert_eq!(classify_aqi(Some(500.0), "european").unwrap()["band"], "extremely poor");
        assert_eq!(classify_aqi(Some(500.0), "european").unwrap()["upperBound"], Value::Null);
    }

    #[test]
    fn us_bands() {
        assert_eq!(classify_aqi(Some(37.0), "us").unwrap()["band"], "good");
        assert_eq!(classify_aqi(Some(120.0), "us").unwrap()["band"], "unhealthy-for-sensitive-groups");
        assert_eq!(classify_aqi(Some(350.0), "us").unwrap()["band"], "hazardous");
    }

    #[test]
    fn null_values_classify_to_null() {
        assert!(classify_aqi(None, "european").is_none());
        assert!(classify_aqi(Some(f64::NAN), "us").is_none());
    }

    #[test]
    fn encodes_components() {
        assert_eq!(enc("pm2_5,pm10"), "pm2_5%2Cpm10");
        assert_eq!(enc("cams_global"), "cams_global");
    }
}
