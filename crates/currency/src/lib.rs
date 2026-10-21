//! currency service — central-bank reference exchange rates without an API
//! key, plus amount conversion. Sourced from Frankfurter v2 (multi-provider).
//!
//! Ported 1:1 from `zoofam26/currency-api` (NestJS).

mod routes;

use actix_web::web;
use utoipa::openapi::OpenApi;

pub const SERVICE_PREFIX: &str = "/currency";
pub const CODE_PREFIX: &str = "CURRENCY";
pub const UPSTREAM_FRANKFURTER: &str = "https://api.frankfurter.dev";
pub const BLEND: &str = "blend";

/// Cache TTLs mirror the original (ms → s).
pub const TTL_LATEST_SECS: u64 = 3_600;
pub const TTL_HISTORICAL_SECS: u64 = 43_200;
pub const TTL_REFERENCE_SECS: u64 = 86_400;

pub const DEFAULT_BASE: &str = "EUR";
pub const DEFAULT_PROVIDER: &str = "ECB";
pub const MAX_RANGE_DAYS: i64 = 3_660;
pub const MAX_SYMBOLS: usize = 50;
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 1_000;
pub const RATE_DECIMALS: u32 = 6;

pub fn router(cfg: &mut web::ServiceConfig) {
    routes::configure(cfg);
}

pub fn openapi() -> OpenApi {
    routes::openapi_doc()
}

pub fn index() -> serde_json::Value {
    serde_json::json!({
        "name": "currency-api",
        "prefix": SERVICE_PREFIX,
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Central bank reference exchange rates without an API key, plus amount conversion. Sourced from Frankfurter v2, pinned to the European Central Bank by default.",
        "dataSource": {
            "provider": DEFAULT_PROVIDER,
            "upstream": [UPSTREAM_FRANKFURTER],
            "requiresApiKey": false,
            "note": "Reference rates are published on business days only, so a request for a weekend or public holiday is answered with the most recent preceding publication; every response reports the effective publication date alongside the requested one.",
        },
        "endpoints": [
            { "method": "GET", "path": "/currency/rates/latest", "description": "Latest reference rates for a base currency, optionally filtered" },
            { "method": "GET", "path": "/currency/rates/dates", "description": "Paginated list of the days a provider published rates" },
            { "method": "GET", "path": "/currency/rates", "description": "Daily time series over a date window, grouped by publication date" },
            { "method": "GET", "path": "/currency/convert", "description": "An amount in one currency expressed in another" },
            { "method": "GET", "path": "/currency/currencies", "description": "Reference list of supported currency codes" },
            { "method": "GET", "path": "/currency/currencies/{code}", "description": "A single currency. Unknown codes return 404." },
            { "method": "GET", "path": "/currency/providers", "description": "The data providers behind the rates, and which one is the default" },
            { "method": "GET", "path": "/currency/health", "description": "Liveness plus a real upstream probe" },
            { "method": "GET", "path": "/currency/health/ready", "description": "Readiness probe" },
            { "method": "GET", "path": "/currency/openapi.json", "description": "OpenAPI 3 document for this service" },
        ],
    })
}

/// Round to `RATE_DECIMALS` places (half-away-from-zero like the original).
pub fn round_to(value: f64, decimals: u32) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

/// Validates a real calendar date in `YYYY-MM-DD` form.
pub fn is_calendar_date(value: &str) -> bool {
    match chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        Ok(d) => d.format("%Y-%m-%d").to_string() == value,
        Err(_) => false,
    }
}

pub fn shift_days(iso_date: &str, delta: i64) -> String {
    chrono::NaiveDate::parse_from_str(iso_date, "%Y-%m-%d")
        .map(|d| (d + chrono::Duration::days(delta)).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|_| iso_date.to_string())
}

pub fn is_weekend(iso_date: &str) -> bool {
    use chrono::Datelike;
    chrono::NaiveDate::parse_from_str(iso_date, "%Y-%m-%d")
        .map(|d| {
            let wd = d.weekday();
            wd == chrono::Weekday::Sat || wd == chrono::Weekday::Sun
        })
        .unwrap_or(false)
}

/// `blend`/`all` → `blend`; otherwise upper-cased provider key.
pub fn normalise_provider(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return DEFAULT_PROVIDER.to_string();
    }
    let lowered = trimmed.to_lowercase();
    if lowered == "blend" || lowered == "all" {
        BLEND.to_string()
    } else {
        trimmed.to_uppercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_like_the_original() {
        assert_eq!(round_to(0.8907712345, 6), 0.890771);
        assert_eq!(round_to(1.5, 0), 2.0);
        assert_eq!(round_to(0.0, 6), 0.0);
    }

    #[test]
    fn validates_calendar_dates() {
        assert!(is_calendar_date("2026-01-15"));
        assert!(!is_calendar_date("2026-02-30"));
        assert!(!is_calendar_date("15/01/2026"));
        assert!(!is_calendar_date("2026-1-5"));
    }

    #[test]
    fn shifts_days_and_weekends() {
        assert_eq!(shift_days("2026-01-15", -29), "2025-12-17");
        assert_eq!(shift_days("2026-01-01", 1), "2026-01-02");
        assert!(is_weekend("2026-01-17")); // Saturday
        assert!(!is_weekend("2026-01-19")); // Monday
    }

    #[test]
    fn normalises_providers() {
        assert_eq!(normalise_provider("ecb"), "ECB");
        assert_eq!(normalise_provider("blend"), BLEND);
        assert_eq!(normalise_provider("all"), BLEND);
        assert_eq!(normalise_provider(""), "ECB");
        assert_eq!(normalise_provider(" amcm "), "AMCM");
    }
}
