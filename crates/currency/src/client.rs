//! Frankfurter v2 client — the only place in this service that knows
//! Frankfurter exists. Ports `currency/client/frankfurter.client.ts` (paths,
//! provider pinning, `blend` sentinel) and `currency/parsers/frankfurter.parser.ts`
//! (payload → domain types).
//!
//! Transport (timeouts, retries, jitter) lives in `master_common::httpx`; this
//! module owns URLs, query building and shape knowledge. Response shapes were
//! verified against the live v2 API:
//! - `GET /v2/rates` → flat array `[{date, base, quote, rate}]`
//! - `GET /v2/rate/{base}/{quote}` → single `{date, base, quote, rate}`
//! - `GET /v2/currencies` → `[{iso_code, iso_numeric, name, symbol,
//!   start_date, end_date, providers?}]`
//! - `GET /v2/currency/{code}` → one currency object (with `providers`)
//! - `GET /v2/providers` → `[{key, name, country_code, rate_type,
//!   pivot_currency, data_url, terms_url, start_date, end_date, frequency, …}]`

use serde::Serialize;
use serde_json::{Map, Value};

use actix_web::ResponseError;
use master_common::error::ApiError;
use master_common::httpx;
use master_common::state::AppState;

use crate::service::is_calendar_date;
use crate::CODE_PREFIX;

/// The sentinel the upstream uses for "do not pin a provider".
pub const BLEND: &str = "blend";

/// One published exchange rate.
#[derive(Debug, Clone, Serialize)]
pub struct RateRow {
    /// The date the rate was actually published, `YYYY-MM-DD`.
    pub date: String,
    pub base: String,
    pub quote: String,
    pub rate: f64,
}

/// Reference entry for one ISO 4217 currency.
#[derive(Debug, Clone, Serialize)]
pub struct CurrencyInfo {
    pub code: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// ISO 4217 numeric code, e.g. `978` for EUR.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub numeric: Option<String>,
    /// First day the provider has a rate for this currency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_date: Option<String>,
    /// Last day the provider has a rate for this currency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_date: Option<String>,
    /// How many providers publish this currency, when the upstream reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_count: Option<u64>,
}

/// One entry of the upstream provider directory.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub key: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pivot_currency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terms_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency: Option<String>,
}

// ------------------------------------------------------------------ errors

fn parse_error(message: impl Into<String>) -> ApiError {
    ApiError::upstream(format!("{CODE_PREFIX}_PARSE"), message)
}

/// Deterministic upstream failures (400/404/422) are cached as a sentinel so
/// the exact status, code, message and details survive a cache hit; transient
/// failures (network, 429, 5xx, timeout) propagate so they are **not** cached
/// and the next request retries them (matching the original TTL-cache policy).
pub fn cache_error_value(e: &ApiError) -> Value {
    let mut wire = Map::new();
    wire.insert("status".into(), Value::from(e.status_code().as_u16()));
    wire.insert("error".into(), e.to_error_body().get("error").cloned().unwrap_or(Value::Null));
    serde_json::json!({ "__error__": Value::Object(wire) })
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
        400 => ApiError::Validation { code, message, details },
        // The upstream answers 422 for requests it rejects — a caller mistake,
        // exactly like the original's InvalidRequestError mapping.
        422 => ApiError::Validation { code, message, details },
        404 => ApiError::NotFound { code, message, details },
        429 => ApiError::RateLimited { code, message, details },
        503 => ApiError::Unavailable { code, message, details },
        504 => ApiError::Timeout { code, message, details },
        _ => ApiError::Upstream { code, message, details },
    };
    Err(error)
}

fn deterministic(status: u16) -> bool {
    matches!(status, 400 | 404 | 422)
}

/// True for upstream failures whose answer will not change on retry (400/404/422).
pub fn deterministic_status(e: &ApiError) -> bool {
    deterministic(e.status_code().as_u16())
}

// ------------------------------------------------------------------ parsers

fn read_code(value: &Value, what: &str) -> Result<String, ApiError> {
    let text = value.as_str().unwrap_or_default();
    let is_code = text.len() == 3 && text.bytes().all(|b| b.is_ascii_alphabetic());
    if !is_code {
        return Err(parse_error(format!(
            "Upstream {what} is not a 3-letter currency code"
        )));
    }
    Ok(text.to_uppercase())
}

fn read_date(value: &Value, what: &str) -> Result<String, ApiError> {
    match value.as_str() {
        Some(text) if is_calendar_date(text) => Ok(text.to_string()),
        _ => Err(parse_error(format!("Upstream {what} is not an ISO date"))),
    }
}

fn read_rate(value: &Value) -> Result<f64, ApiError> {
    let parsed = match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    match parsed {
        Some(rate) if rate.is_finite() && rate > 0.0 => Ok(rate),
        _ => Err(parse_error(
            "Upstream rate is not a positive finite number",
        )),
    }
}

fn read_optional_string(value: &Value) -> Option<String> {
    value.as_str().filter(|s| !s.is_empty()).map(String::from)
}

fn read_optional_date(value: &Value) -> Option<String> {
    value.as_str().filter(|s| is_calendar_date(s)).map(String::from)
}

/// One `Rate` object from `/v2/rates` or `/v2/rate/{base}/{quote}`.
pub fn parse_rate_row(raw: &Value, what: &str) -> Result<RateRow, ApiError> {
    let obj = raw
        .as_object()
        .ok_or_else(|| parse_error(format!("Upstream {what} is not an object")))?;
    Ok(RateRow {
        date: read_date(field(obj, "date"), &format!("{what}.date"))?,
        base: read_code(field(obj, "base"), &format!("{what}.base"))?,
        quote: read_code(field(obj, "quote"), &format!("{what}.quote"))?,
        rate: read_rate(field(obj, "rate"))?,
    })
}

fn field<'a>(obj: &'a Map<String, Value>, key: &str) -> &'a Value {
    obj.get(key).unwrap_or(&Value::Null)
}

/// The `/v2/rates` response: always a flat array, possibly empty.
pub fn parse_rate_rows(raw: &Value) -> Result<Vec<RateRow>, ApiError> {
    let arr = raw
        .as_array()
        .ok_or_else(|| parse_error("Upstream rates payload is not an array"))?;
    arr.iter().map(|row| parse_rate_row(row, "rate")).collect()
}

/// The `/v2/currencies` response.
pub fn parse_currencies(raw: &Value) -> Result<Vec<CurrencyInfo>, ApiError> {
    let arr = raw
        .as_array()
        .ok_or_else(|| parse_error("Upstream currencies payload is not an array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (index, entry) in arr.iter().enumerate() {
        let obj = entry.as_object().ok_or_else(|| {
            parse_error(format!("Upstream currency entry {index} is not an object"))
        })?;
        out.push(CurrencyInfo {
            code: read_code(
                field(obj, "iso_code"),
                &format!("currencies[{index}].iso_code"),
            )?,
            name: read_optional_string(field(obj, "name"))
                .unwrap_or_else(|| "Unknown".to_string()),
            symbol: read_optional_string(field(obj, "symbol")),
            numeric: read_optional_string(field(obj, "iso_numeric")),
            start_date: read_optional_date(field(obj, "start_date")),
            end_date: read_optional_date(field(obj, "end_date")),
            provider_count: field(obj, "providers")
                .as_array()
                .map(|a| a.len() as u64),
        });
    }
    Ok(out)
}

/// The `/v2/currency/{code}` response: the single-currency endpoint guarantees
/// a `providers` array; only assert what the contract actually promises.
pub fn parse_currency(raw: &Value) -> Result<CurrencyInfo, ApiError> {
    let wrapped = Value::Array(vec![raw.clone()]);
    let mut items = parse_currencies(&wrapped)?;
    items.pop().ok_or_else(|| parse_error("Upstream returned an empty currency payload"))
}

/// The `/v2/providers` response.
pub fn parse_providers(raw: &Value) -> Result<Vec<ProviderInfo>, ApiError> {
    let arr = raw
        .as_array()
        .ok_or_else(|| parse_error("Upstream providers payload is not an array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (index, entry) in arr.iter().enumerate() {
        let obj = entry.as_object().ok_or_else(|| {
            parse_error(format!("Upstream provider entry {index} is not an object"))
        })?;
        let key = read_optional_string(field(obj, "key"))
            .ok_or_else(|| parse_error(format!("Upstream provider {index} is missing a key")))?;
        out.push(ProviderInfo {
            name: read_optional_string(field(obj, "name")).unwrap_or_else(|| key.clone()),
            key,
            country_code: read_optional_string(field(obj, "country_code")),
            rate_type: read_optional_string(field(obj, "rate_type")),
            pivot_currency: read_optional_string(field(obj, "pivot_currency")),
            data_url: read_optional_string(field(obj, "data_url")),
            terms_url: read_optional_string(field(obj, "terms_url")),
            start_date: read_optional_date(field(obj, "start_date")),
            end_date: read_optional_date(field(obj, "end_date")),
            frequency: read_optional_string(field(obj, "frequency")),
        });
    }
    Ok(out)
}

// ----------------------------------------------------------------- fetchers

/// Parameters for [`fetch_rates`].
#[derive(Debug, Clone, Default)]
pub struct RatesQuery {
    pub base: Option<String>,
    pub quotes: Vec<String>,
    /// A single publication date to resolve. Mutually exclusive with from/to.
    pub date: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Provider key, or `blend` to use the multi-provider consensus.
    pub provider: Option<String>,
}

/// Build the query string pairs for `GET /v2/rates`. `blend` drops the pin.
pub fn rates_query_params(q: &RatesQuery) -> Vec<(String, String)> {
    let mut params: Vec<(String, String)> = Vec::new();
    if let Some(base) = &q.base {
        params.push(("base".into(), base.clone()));
    }
    if !q.quotes.is_empty() {
        params.push(("quotes".into(), q.quotes.join(",")));
    }
    if let Some(date) = &q.date {
        params.push(("date".into(), date.clone()));
    }
    if let Some(from) = &q.from {
        params.push(("from".into(), from.clone()));
    }
    if let Some(to) = &q.to {
        params.push(("to".into(), to.clone()));
    }
    if let Some(provider) = &q.provider {
        if provider != BLEND {
            params.push(("providers".into(), provider.clone()));
        }
    }
    params
}

async fn get(state: &AppState, url: &str, params: &[(String, String)]) -> Result<Value, ApiError> {
    let refs: Vec<(&str, &str)> =
        params.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let timed = httpx::get_json(state, url, &refs, CODE_PREFIX).await?;
    Ok(timed.body)
}

/// `GET /v2/rates` — latest, on a date, or over a range.
pub async fn fetch_rates(state: &AppState, base_url: &str, q: &RatesQuery) -> Result<Value, ApiError> {
    let url = format!("{base_url}/v2/rates");
    let params = rates_query_params(q);
    get(state, &url, &params).await
}

/// `GET /v2/rate/{base}/{quote}` — one pair, optionally as of a date.
pub async fn fetch_rate(
    state: &AppState,
    base_url: &str,
    base: &str,
    quote: &str,
    date: Option<&str>,
    provider: Option<&str>,
) -> Result<Value, ApiError> {
    let url = format!("{base_url}/v2/rate/{base}/{quote}");
    let mut params: Vec<(String, String)> = Vec::new();
    if let Some(d) = date {
        params.push(("date".into(), d.to_string()));
    }
    if let Some(p) = provider {
        if p != BLEND {
            params.push(("providers".into(), p.to_string()));
        }
    }
    get(state, &url, &params).await
}

/// `GET /v2/currencies` — the currency reference list.
pub async fn fetch_currencies(
    state: &AppState,
    base_url: &str,
    scope: Option<&str>,
    provider: Option<&str>,
) -> Result<Value, ApiError> {
    let url = format!("{base_url}/v2/currencies");
    let mut params: Vec<(String, String)> = Vec::new();
    if let Some(s) = scope {
        params.push(("scope".into(), s.to_string()));
    }
    if let Some(p) = provider {
        if p != BLEND {
            params.push(("providers".into(), p.to_string()));
        }
    }
    get(state, &url, &params).await
}

/// `GET /v2/currency/{code}` — one currency, with its provider coverage.
pub async fn fetch_currency(state: &AppState, base_url: &str, code: &str) -> Result<Value, ApiError> {
    let url = format!("{base_url}/v2/currency/{code}");
    get(state, &url, &[]).await
}

/// `GET /v2/providers` — the data provider directory.
pub async fn fetch_providers(state: &AppState, base_url: &str) -> Result<Value, ApiError> {
    let url = format!("{base_url}/v2/providers");
    get(state, &url, &[]).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_the_real_rates_shape() {
        let body = json!([
            { "date": "2026-10-20", "base": "USD", "quote": "EUR", "rate": 0.8913 },
            { "date": "2026-10-20", "base": "USD", "quote": "GBP", "rate": 0.7801 }
        ]);
        let rows = parse_rate_rows(&body).expect("parses");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].quote, "EUR");
        assert_eq!(rows[0].base, "USD");
        assert!((rows[1].rate - 0.7801).abs() < 1e-12);
    }

    #[test]
    fn rejects_an_unusable_rate_row() {
        let bad = json!({ "date": "2026-10-20", "base": "USD", "quote": "EUR", "rate": 0.0 });
        assert!(parse_rate_row(&bad, "rate").is_err(), "zero rate is not a rate");
        let bad_date = json!({ "date": "2026-10-2", "base": "USD", "quote": "EUR", "rate": 1.0 });
        assert!(parse_rate_row(&bad_date, "rate").is_err());
    }

    #[test]
    fn parses_the_real_currencies_shape_with_provider_count() {
        let body = json!([
            { "iso_code": "ars", "iso_numeric": "032", "name": "Argentine Peso", "symbol": "$",
              "start_date": "2000-01-13", "end_date": "2020-10-30" },
            { "iso_code": "EUR", "iso_numeric": "978", "name": "Euro",
              "providers": ["ECB", "AMCM"] }
        ]);
        let items = parse_currencies(&body).expect("parses");
        assert_eq!(items[0].code, "ARS", "codes are upper-cased");
        assert_eq!(items[0].end_date.as_deref(), Some("2020-10-30"));
        assert_eq!(items[0].provider_count, None);
        assert_eq!(items[1].provider_count, Some(2));
        // Absent optionals are omitted from the serialised form, exactly like
        // the original (JSON.stringify drops undefined).
        let text = serde_json::to_string(&items[0]).unwrap();
        assert!(text.contains("\"code\":\"ARS\""));
        assert!(!text.contains("providerCount"));
        assert!(!text.contains("\"numeric\""));
    }

    #[test]
    fn parses_the_real_providers_shape() {
        let body = json!([
            { "key": "AMCM", "name": "Autoridade Monetária de Macau", "country_code": "MO",
              "rate_type": "interbank middle rate", "pivot_currency": "MOP",
              "data_url": "https://www.amcm.gov.mo/en/financial-information/interbank-midpoint",
              "terms_url": null, "start_date": "1986-01-02", "end_date": "2026-10-20",
              "publish_cadence": "daily", "frequency": "daily", "publishes_missed": 0,
              "currencies": ["EUR", "HKD"], "unknown_currencies": [] }
        ]);
        let items = parse_providers(&body).expect("parses");
        assert_eq!(items.len(), 1);
        let p = &items[0];
        assert_eq!(p.key, "AMCM");
        assert_eq!(p.country_code.as_deref(), Some("MO"));
        assert_eq!(p.frequency.as_deref(), Some("daily"));
        assert_eq!(p.terms_url, None, "null terms_url stays absent");
        // Fields the original parser does not surface stay unsurfaced.
        let text = serde_json::to_string(p).unwrap();
        assert!(!text.contains("publish_cadence"));
        assert!(!text.contains("currencies"));
    }

    #[test]
    fn single_currency_endpoint_maps_like_the_list() {
        let body = json!({ "iso_code": "EUR", "iso_numeric": "978", "name": "Euro", "symbol": "€" });
        let info = parse_currency(&body).expect("parses");
        assert_eq!(info.code, "EUR");
        assert_eq!(info.numeric.as_deref(), Some("978"));
    }

    #[test]
    fn rate_query_params_drop_the_blend_pin() {
        let q = RatesQuery {
            base: Some("EUR".into()),
            quotes: vec!["USD".into(), "GBP".into()],
            from: Some("2026-01-01".into()),
            to: Some("2026-01-31".into()),
            provider: Some("blend".into()),
            ..Default::default()
        };
        let params = rates_query_params(&q);
        assert!(!params.iter().any(|(k, _)| k == "providers"), "blend must not pin");
        assert_eq!(
            params.iter().find(|(k, _)| *k == "quotes").map(|(_, v)| v.as_str()),
            Some("USD,GBP")
        );
        let pinned = RatesQuery { provider: Some("ECB".into()), ..q };
        let params = rates_query_params(&pinned);
        assert_eq!(
            params.iter().find(|(k, _)| *k == "providers").map(|(_, v)| v.as_str()),
            Some("ECB")
        );
    }

    #[test]
    fn deterministic_errors_survive_the_cache_sentinel() {
        let original = ApiError::not_found_details(
            "CURRENCY_NOT_FOUND",
            "The requested resource was not found upstream",
            json!({ "upstreamStatus": 404 }),
        );
        let stored = cache_error_value(&original);
        assert!(stored.get("__error__").is_some());
        match decode_cached(&stored) {
            Err(ApiError::NotFound { code, details, .. }) => {
                assert_eq!(code, "CURRENCY_NOT_FOUND");
                assert_eq!(details.map(|d| d.get("upstreamStatus").and_then(Value::as_u64).unwrap_or(0)), Some(404));
            }
            other => panic!("expected the 404 to survive, got {other:?}"),
        }
        // A real payload passes straight through.
        assert_eq!(decode_cached(&json!([{ "x": 1 }])).unwrap(), json!([{ "x": 1 }]));
        // Transient statuses stay transient when re-decoded.
        let stored = cache_error_value(&ApiError::upstream("CURRENCY_UPSTREAM", "boom"));
        assert!(matches!(decode_cached(&stored), Err(ApiError::Upstream { .. })));
    }
}
