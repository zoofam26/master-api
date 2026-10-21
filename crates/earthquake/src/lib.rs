//! earthquake service — USGS FDSN earthquake queries (list / summary / by id)
//! plus an Open-Meteo air-quality sub-service (`/air-quality`, `/air-quality/aqi`).
//!
//! Ported from `zoofam26/earthquake-api` (NestJS).

mod aq;
mod routes;

use actix_web::web;
use utoipa::openapi::OpenApi;

pub const SERVICE_PREFIX: &str = "/earthquake";
pub const CODE_PREFIX: &str = "EARTHQUAKE";
pub const UPSTREAM_USGS: &str = "https://earthquake.usgs.gov/fdsnws/event/1";
pub const UPSTREAM_OPEN_METEO_AQ: &str = "https://air-quality-api.open-meteo.com/v1/air-quality";
pub const DEPENDENCY: &str = "USGS FDSN Event Web Service";

/// Cache TTLs (s), mirroring the original (60s list/event, 120s summary).
pub const TTL_LIST_SECS: u64 = 60;
pub const TTL_SUMMARY_SECS: u64 = 120;
pub const TTL_AQ_SECS: u64 = 900;

pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 200;
pub const DEFAULT_WINDOW_DAYS: i64 = 30;
pub const SUMMARY_SAMPLE_LIMIT: usize = 1_000;
pub const SUMMARY_TOP_REGIONS: usize = 10;

pub const TIEBREAKER: &str = "event id, ascending (lexicographic)";

pub const ORDER_BY: &[&str] = &[
    "time", "time-asc", "magnitude", "magnitude-asc", "area", "area-asc",
];
pub const EVENT_STATUSES: &[&str] = &["all", "automatic", "reviewed", "deleted"];

pub fn router(cfg: &mut web::ServiceConfig) {
    routes::configure(cfg);
}

pub fn openapi() -> OpenApi {
    routes::openapi_doc()
}

pub fn index() -> serde_json::Value {
    serde_json::json!({
        "name": "earthquake-api",
        "prefix": SERVICE_PREFIX,
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Earthquake queries over the USGS FDSN Event Web Service (list with rectangle filters, aggregate summary with magnitude/energy/region breakdowns, single event by id), plus Open-Meteo air-quality forecasts and AQI classification.",
        "dataSource": {
            "provider": DEPENDENCY,
            "upstream": [UPSTREAM_USGS, UPSTREAM_OPEN_METEO_AQ],
            "requiresApiKey": false,
            "note": "USGS responses are cached ~60s (matching their cache-control); air quality ~15min.",
        },
        "endpoints": [
            { "method": "GET", "path": "/earthquake/summary", "description": "Aggregate statistics over a window (magnitudes, buckets, regions, energy)" },
            { "method": "GET", "path": "/earthquake/air-quality/aqi", "description": "Current European/US AQI with named bands" },
            { "method": "GET", "path": "/earthquake/air-quality", "description": "Hourly air-quality forecast" },
            { "method": "GET", "path": "/earthquake/{id}", "description": "One event by USGS id" },
            { "method": "GET", "path": "/earthquake", "description": "Filtered, paginated, stably sorted event list" },
            { "method": "GET", "path": "/earthquake/health", "description": "Liveness plus a real upstream probe" },
            { "method": "GET", "path": "/earthquake/health/ready", "description": "Readiness probe" },
            { "method": "GET", "path": "/earthquake/openapi.json", "description": "OpenAPI 3 document for this service" },
        ],
    })
}

// ------------------------------------------------------------- magnitude

pub const MAGNITUDE_BUCKETS: &[(&str, &str, Option<f64>, Option<f64>)] = &[
    // (category, label, minInclusive, maxExclusive)
    ("micro", "Micro", None, Some(2.5)),
    ("minor", "Minor", Some(2.5), Some(5.0)),
    ("moderate", "Moderate", Some(5.0), Some(6.0)),
    ("strong", "Strong", Some(6.0), Some(7.0)),
    ("major", "Major", Some(7.0), None),
];

pub fn classify_magnitude(magnitude: Option<f64>) -> &'static str {
    let Some(m) = magnitude.filter(|v| v.is_finite()) else {
        return "unknown";
    };
    for (cat, _, min, max) in MAGNITUDE_BUCKETS {
        let above_min = min.map(|v| m >= v).unwrap_or(true);
        let below_max = max.map(|v| m < v).unwrap_or(true);
        if above_min && below_max {
            return cat;
        }
    }
    "micro" // negative magnitudes (icequakes) sit below the first bucket.
}

/// Comparable magnitude scale family derived from the raw `magType`.
pub fn magnitude_scale(mag_type: Option<&str>) -> &'static str {
    match mag_type.unwrap_or("") {
        "ml" | "mlv" | "mlvt" => "local",
        "md" => "duration",
        "mb" | "mblg" | "mb_lg" => "body-wave",
        "mww" | "mwc" | "mwr" | "mwb" | "mwc_pde" | "mww_pde" => "moment-tensor",
        "ms" | "msp" => "proxy",
        "" => "other",
        other => match other {
            "Mi" | "mi" | "Mlr" | "mlr" => "local",
            "Mb" | "mB" => "body-wave",
            "Mw" | "mw" => "moment-tensor",
            _ => "other",
        },
    }
}

pub fn energy_proxy(magnitude: f64) -> f64 {
    10f64.powf(1.5 * magnitude)
}

pub fn energy_joules(magnitude: f64) -> f64 {
    10f64.powf(1.5 * magnitude + 4.8) * 1e-7
}

pub fn round(value: f64, digits: i32) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let factor = 10f64.powi(digits);
    (value * factor).round() / factor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_magnitudes() {
        assert_eq!(classify_magnitude(Some(1.2)), "micro");
        assert_eq!(classify_magnitude(Some(3.0)), "minor");
        assert_eq!(classify_magnitude(Some(5.5)), "moderate");
        assert_eq!(classify_magnitude(Some(6.4)), "strong");
        assert_eq!(classify_magnitude(Some(8.9)), "major");
        assert_eq!(classify_magnitude(Some(-0.5)), "micro");
        assert_eq!(classify_magnitude(None), "unknown");
    }

    #[test]
    fn maps_magnitude_scales() {
        assert_eq!(magnitude_scale(Some("ml")), "local");
        assert_eq!(magnitude_scale(Some("md")), "duration");
        assert_eq!(magnitude_scale(Some("mb")), "body-wave");
        assert_eq!(magnitude_scale(Some("mww")), "moment-tensor");
        assert_eq!(magnitude_scale(Some("weird")), "other");
        assert_eq!(magnitude_scale(None), "other");
    }

    #[test]
    fn energy_formulas() {
        assert!((energy_proxy(1.0) - 10f64.powf(1.5)).abs() < 1e-9);
        assert!((energy_joules(1.0) - 10f64.powf(6.3) * 1e-7).abs() < 1e-6);
    }
}
