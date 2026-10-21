//! public-holidays service — worldwide public holidays with working-day
//! computation, backed by the Nager.Date Community API v4.
//!
//! Ported 1:1 from `zoofam26/public-holidays-api` (NestJS). Responses are
//! bare payloads (no envelope), exactly like the original service.

mod routes;

use actix_web::web;
use utoipa::openapi::OpenApi;

pub const SERVICE_PREFIX: &str = "/public-holidays";
pub const CODE_PREFIX: &str = "HOLIDAYS";
pub const UPSTREAM_NAGER: &str = "https://date.nager.at";

/// Nager v4 serves 2025-2031 only (verified exhaustively upstream).
pub const FIRST_YEAR: i32 = 2025;
pub const LAST_YEAR: i32 = 2031;

pub const TTL_SECS: u64 = 86_400;
pub const TTL_UPCOMING_SECS: u64 = 600;
pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 200;
pub const MAX_RANGE_DAYS: usize = 366;
pub const MAX_LIST_COUNTRIES: usize = 25;
pub const MAX_FAN_OUT: usize = 6;

pub const HOLIDAY_TYPES: &[&str] = &[
    "Public", "Bank", "School", "Authorities", "Optional", "Observance",
];

pub fn router(cfg: &mut web::ServiceConfig) {
    routes::configure(cfg);
}

pub fn openapi() -> OpenApi {
    routes::openapi_doc()
}

pub fn index() -> serde_json::Value {
    serde_json::json!({
        "name": "public-holidays-api",
        "prefix": SERVICE_PREFIX,
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Worldwide public holidays with subdivision filtering, working-day computation and upcoming-holiday windows, backed by the Nager.Date Community API v4.",
        "dataSource": {
            "provider": "Nager.Date Community v4",
            "upstream": [UPSTREAM_NAGER],
            "requiresApiKey": false,
            "note": format!("Nager v4 serves {FIRST_YEAR}-{LAST_YEAR} only; the upstream window is validated locally so out-of-range years are an instant, explanatory 400."),
        },
        "endpoints": [
            { "method": "GET", "path": "/public-holidays/countries", "description": "Supported countries, with opt-in region counts" },
            { "method": "GET", "path": "/public-holidays/working-days", "description": "Working days over a date range for one country" },
            { "method": "GET", "path": "/public-holidays/holidays/upcoming", "description": "Next holidays for one country (365-day forward window)" },
            { "method": "GET", "path": "/public-holidays/holidays/is-holiday", "description": "Whether a date is a public holiday for one country" },
            { "method": "GET", "path": "/public-holidays/holidays", "description": "Windowed holiday list across countries" },
            { "method": "GET", "path": "/public-holidays/holidays/{countryCode}", "description": "One country's holidays for a year" },
            { "method": "GET", "path": "/public-holidays/holidays/{countryCode}/{year}", "description": "Subdivision-scope holidays for a country/year" },
            { "method": "GET", "path": "/public-holidays/health", "description": "Liveness plus a real upstream probe" },
            { "method": "GET", "path": "/public-holidays/health/ready", "description": "Readiness probe" },
            { "method": "GET", "path": "/public-holidays/openapi.json", "description": "OpenAPI 3 document for this service" },
        ],
    })
}

// ------------------------------------------------------------ date helpers

pub fn is_real_iso_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    if !value[..4].chars().all(|c| c.is_ascii_digit())
        || !value[5..7].chars().all(|c| c.is_ascii_digit())
        || !value[8..].chars().all(|c| c.is_ascii_digit())
    {
        return false;
    }
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map(|d| d.format("%Y-%m-%d").to_string() == value)
        .unwrap_or(false)
}

/// `AT` / `at` / ` At ` → `AT`; `None` when not two ASCII letters.
pub fn normalise_country_code(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() == 2 && bytes.iter().all(|b| b.is_ascii_alphabetic()) {
        Some(trimmed.to_uppercase())
    } else {
        None
    }
}

/// `at-2` / `AT-2 ` → `AT-2`; `None` for anything not ISO 3166-2 shaped.
pub fn normalise_region_code(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 4
        && bytes.len() <= 6
        && bytes[2] == b'-'
        && bytes[..2].iter().all(|b| b.is_ascii_alphabetic())
        && bytes[3..].iter().all(|b| b.is_ascii_alphanumeric())
    {
        Some(format!(
            "{}-{}",
            trimmed[..2].to_uppercase(),
            trimmed[3..].to_uppercase()
        ))
    } else {
        None
    }
}

/// `2026-01-05` → `1` (Monday). Timezone-independent.
pub fn day_of_week(iso_date: &str) -> u32 {
    use chrono::Datelike;
    chrono::NaiveDate::parse_from_str(iso_date, "%Y-%m-%d")
        .map(|d| d.weekday().num_days_from_sunday())
        .unwrap_or(0)
}

pub fn is_weekend_date(iso_date: &str) -> bool {
    matches!(day_of_week(iso_date), 0 | 6)
}

pub fn each_date(from: &str, to: &str) -> Vec<String> {
    let (Some(f), Some(t)) = (
        chrono::NaiveDate::parse_from_str(from, "%Y-%m-%d").ok(),
        chrono::NaiveDate::parse_from_str(to, "%Y-%m-%d").ok(),
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = f;
    while cursor <= t {
        out.push(cursor.format("%Y-%m-%d").to_string());
        cursor += chrono::Duration::days(1);
    }
    out
}

pub fn add_days(iso_date: &str, days: i64) -> String {
    chrono::NaiveDate::parse_from_str(iso_date, "%Y-%m-%d")
        .map(|d| (d + chrono::Duration::days(days)).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|_| iso_date.to_string())
}

pub fn today_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_impossible_dates() {
        assert!(is_real_iso_date("2026-01-15"));
        assert!(!is_real_iso_date("2026-02-30"));
        assert!(!is_real_iso_date("2026-1-5"));
        assert!(!is_real_iso_date("15/01/2026"));
    }

    #[test]
    fn normalises_codes() {
        assert_eq!(normalise_country_code(" At ").as_deref(), Some("AT"));
        assert_eq!(normalise_country_code("ATT"), None);
        assert_eq!(normalise_region_code("at-2").as_deref(), Some("AT-2"));
        assert_eq!(normalise_region_code("GB-ENG").as_deref(), Some("GB-ENG"));
        assert_eq!(normalise_region_code("US-CA ").as_deref(), Some("US-CA"));
        assert_eq!(normalise_region_code("USCA"), None);
    }

    #[test]
    fn date_math() {
        assert_eq!(day_of_week("2026-01-05"), 1); // Monday
        assert_eq!(day_of_week("2026-01-04"), 0); // Sunday
        assert!(is_weekend_date("2026-01-17"));
        assert_eq!(add_days("2026-01-01", -1), "2025-12-31");
        let span = each_date("2026-01-01", "2026-01-03");
        assert_eq!(span, vec!["2026-01-01", "2026-01-02", "2026-01-03"]);
        assert_eq!(today_iso().len(), 10);
    }
}
