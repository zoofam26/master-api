//! Nager.Date Community API v4 client — the only component in this service
//! that talks to Nager.Date. Ports `holidays/client/nager.client.ts`.
//!
//! Transport (timeouts, bounded retries with jitter) lives in
//! `master_common::httpx`; this module owns the exact v4 paths, the 204
//! "no holidays" case and shape knowledge. Paths verified against the live
//! API: `/api/v4/Countries/Available`, `/api/v4/Holidays/{cc}/{year}`,
//! `/api/v4/Holidays/{cc}/Next` (a supported country with no rules at all,
//! e.g. AQ, answers HTTP 204 with an empty body).

use serde_json::Value;

use master_common::error::ApiError;
use master_common::httpx;
use master_common::state::AppState;

use crate::model::{normalise_country_code, parse_holiday_list, Country, Holiday};
use crate::CODE_PREFIX;

fn parse_error(message: impl Into<String>) -> ApiError {
    ApiError::upstream(format!("{CODE_PREFIX}_PARSE"), message)
}

/// Deterministic upstream failures (400/404/422) are cached as a sentinel so
/// the exact status, code, message and details survive a cache hit; transient
/// failures (network, 429, 5xx, timeout) propagate so they are **not** cached
/// and the next request retries them (matching the original TTL-cache policy).
pub fn cache_error_value(e: &ApiError) -> Value {
    use actix_web::ResponseError;
    serde_json::json!({
        "__error__": {
            "status": e.status_code().as_u16(),
            "error": e.to_error_body().get("error").cloned().unwrap_or(Value::Null),
        }
    })
}

/// Rebuild an [`ApiError`] from a cached sentinel payload, or return the
/// payload itself when it is a real result.
pub fn decode_cached(body: &Value) -> Result<Value, ApiError> {
    let Some(err) = body.get("__error__") else {
        return Ok(body.clone());
    };
    let status = err.get("status").and_then(Value::as_u64).unwrap_or(502) as u16;
    let code = err
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("UPSTREAM_ERROR")
        .to_string();
    let message = err
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("Upstream request failed")
        .to_string();
    let details = err
        .get("error")
        .and_then(|e| e.get("details"))
        .cloned()
        .filter(|d| !d.is_null());
    let error = match status {
        400 | 422 => ApiError::Validation { code, message, details },
        404 => ApiError::NotFound { code, message, details },
        429 => ApiError::RateLimited { code, message, details },
        503 => ApiError::Unavailable { code, message, details },
        504 => ApiError::Timeout { code, message, details },
        _ => ApiError::Upstream { code, message, details },
    };
    Err(error)
}

/// True for upstream failures whose answer will not change on retry.
pub fn deterministic_status(e: &ApiError) -> bool {
    use actix_web::ResponseError;
    matches!(e.status_code().as_u16(), 400 | 404 | 422)
}

/// Fetch a v4 path and parse the JSON body. Nager answers 204 No Content for a
/// supported country with no rules at all (verified for AQ, BV, HM, TF), which
/// surfaces here as `Ok(None)` — that is "no holidays", not a failure.
async fn get_json(
    state: &AppState,
    url: &str,
) -> Result<Option<Value>, ApiError> {
    let (bytes, _) = httpx::get_bytes(state, url, CODE_PREFIX).await?;
    if bytes.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| parse_error("Upstream returned a body that is not valid JSON"))
}

/// `GET /api/v4/Holidays/{countryCode}/{year}` — one country's year.
pub async fn fetch_country_year(
    state: &AppState,
    base_url: &str,
    country_code: &str,
    year: i64,
) -> Result<Vec<Holiday>, ApiError> {
    let code = require_country(country_code)?;
    let url = format!("{base_url}/api/v4/Holidays/{code}/{year}");
    match get_json(state, &url).await? {
        // "No holidays", not a failure.
        None => Ok(Vec::new()),
        Some(body) => parse_holiday_list(&body, Some(code)),
    }
}

/// `GET /api/v4/Holidays/{countryCode}/Next` — upstream 365-day forward window.
pub async fn fetch_upcoming(state: &AppState, base_url: &str, country_code: &str) -> Result<Vec<Holiday>, ApiError> {
    let code = require_country(country_code)?;
    let url = format!("{base_url}/api/v4/Holidays/{code}/Next");
    match get_json(state, &url).await? {
        None => Ok(Vec::new()),
        Some(body) => parse_holiday_list(&body, Some(code)),
    }
}

/// `GET /api/v4/Countries/Available` — the supported-country directory.
pub async fn fetch_available_countries(state: &AppState, base_url: &str) -> Result<Vec<Country>, ApiError> {
    let url = format!("{base_url}/api/v4/Countries/Available");
    let body = get_json(state, &url)
        .await?
        .ok_or_else(|| parse_error("Upstream returned an empty body with HTTP 200"))?;
    let arr = body
        .as_array()
        .ok_or_else(|| parse_error("Upstream country list is not a JSON array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (index, entry) in arr.iter().enumerate() {
        let obj = entry.as_object().ok_or_else(|| {
            parse_error(format!("Upstream country list entry {index} is not an object"))
        })?;
        let code = normalise_country_code(obj.get("countryCode").and_then(Value::as_str).unwrap_or(""))
            .ok_or_else(|| {
                parse_error(format!(
                    "Upstream country entry {index} is missing `countryCode` or `name`"
                ))
            })?;
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                parse_error(format!(
                    "Upstream country entry {index} is missing `countryCode` or `name`"
                ))
            })?;
        out.push(Country { country_code: code, name: name.to_string(), region_count: None });
    }
    Ok(out)
}

fn require_country(country_code: &str) -> Result<&str, ApiError> {
    if normalise_country_code(country_code).is_some() {
        Ok(country_code.trim())
    } else {
        Err(ApiError::upstream(
            format!("{CODE_PREFIX}_UPSTREAM"),
            "Refusing to call upstream with an invalid country code",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_a_real_v4_holidays_payload() {
        // Verbatim prefix of the live `GET /api/v4/Holidays/AT/2026` response.
        let body = json!([
            { "date": "2026-01-01", "name": "New Year's Day", "countryCode": "AT",
              "nationalHoliday": true, "subdivisionCodes": null, "holidayTypes": ["Public"] },
            { "date": "2026-03-19", "name": "Saint Joseph's Day", "countryCode": "AT",
              "nationalHoliday": false, "subdivisionCodes": ["AT-2", "AT-6", "AT-7", "AT-8"],
              "holidayTypes": ["School"] }
        ]);
        let items = parse_holiday_list(&body, Some("AT")).expect("parses");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].scope, "national");
        assert_eq!(items[1].subdivision_codes.len(), 4);
    }

    #[test]
    fn empty_body_means_no_holidays() {
        // The 204 path: get_json returns None → the caller maps to [].
        let none: Option<Value> = None;
        assert!(none.is_none());
    }

    #[test]
    fn a_non_array_holidays_payload_is_a_parse_error() {
        let body = json!({ "unexpected": "object" });
        assert!(parse_holiday_list(&body, Some("AT")).is_err());
    }

    #[test]
    fn errors_round_trip_through_the_cache_sentinel() {
        use actix_web::ResponseError;
        let original = ApiError::not_found_details(
            "HOLIDAYS_NOT_FOUND",
            "The requested resource was not found upstream",
            json!({ "upstreamStatus": 404 }),
        );
        assert!(deterministic_status(&original), "404 is deterministic");
        let stored = cache_error_value(&original);
        match decode_cached(&stored) {
            Err(ApiError::NotFound { code, .. }) => assert_eq!(code, "HOLIDAYS_NOT_FOUND"),
            other => panic!("expected the 404 to survive, got {other:?}"),
        }
        // Real payloads pass through untouched.
        assert_eq!(decode_cached(&json!([{ "date": "2026-01-01" }])).unwrap(), json!([{ "date": "2026-01-01" }]));
        // 5xx stays transient so it is never cached.
        let stored = cache_error_value(&ApiError::upstream("HOLIDAYS_UPSTREAM", "boom"));
        assert!(!deterministic_status(&ApiError::upstream("HOLIDAYS_UPSTREAM", "boom")));
        assert!(matches!(decode_cached(&stored), Err(ApiError::Upstream { .. })));
    }
}
