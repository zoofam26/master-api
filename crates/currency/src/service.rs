//! Orchestration for the currency service: input policy (what counts as a
//! valid code, date window, amount or provider), cache keys, window maths and
//! the translation of upstream rows into the documented response shapes.
//! Ported from `currency/currency.service.ts`; the pure parts are unit-tested
//! here, the HTTP/cache plumbing stays in `routes.rs`.

use std::collections::BTreeSet;

use serde_json::{json, Map, Value};

use master_common::error::ApiError;
use master_common::httpx;
use master_common::state::{cache_names, AppState};

use crate::client::{self, CurrencyInfo, ProviderInfo, RateRow, BLEND};
use crate::CODE_PREFIX;

/// Base currency used when the caller does not supply one (`CURRENCY__DEFAULT_BASE`).
pub const DEFAULT_BASE: &str = "EUR";
/// Provider the rates are pinned to (`UPSTREAM__DEFAULT_PROVIDER`).
pub const DEFAULT_PROVIDER: &str = "ECB";
/// Longest span `GET /currency/rates` and `/currency/rates/dates` accept.
pub const MAX_RANGE_DAYS: i64 = 3660;
/// Maximum number of quote currencies accepted in a single `symbols` filter.
pub const MAX_SYMBOLS: usize = 50;
/// Default page size for the paginated list routes.
pub const DEFAULT_LIMIT: i64 = 100;
/// Maximum page size for the paginated list routes.
pub const MAX_LIMIT: i64 = 1000;
/// Decimal places kept on rates and converted amounts (display precision only).
pub const RATE_DECIMALS: u32 = 6;

// ------------------------------------------------------------------ helpers

fn code_prefix(field: &str) -> String {
    format!("{CODE_PREFIX}_INVALID_{}", field.to_uppercase())
}

/// Build a JSON object from key/value pairs (order-insensitive, like the
/// original's object literals).
pub fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut map = Map::new();
    for (k, v) in pairs {
        map.insert(k.to_string(), v);
    }
    Value::Object(map)
}

/// Display-precision rounding (`common/utils/round.ts`): removes float
/// artefacts like `87.39000000000001` without inventing precision.
pub fn round_to(value: f64, decimals: u32) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let places = decimals.min(100) as usize;
    format!("{:.*}", places, value).parse::<f64>().unwrap_or(value)
}

/// `YYYY-MM-DD` shape only (four digits, dash, two, dash, two).
pub fn has_iso_shape(value: &str) -> bool {
    let bytes = value.as_bytes();
    value.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
}

/// A real calendar date in `YYYY-MM-DD`, rejecting things like `2026-02-31`.
pub fn is_calendar_date(value: &str) -> bool {
    if !has_iso_shape(value) {
        return false;
    }
    let y: i32 = value[0..4].parse().unwrap_or(0);
    let m: u32 = value[5..7].parse().unwrap_or(0);
    let d: u32 = value[8..10].parse().unwrap_or(0);
    chrono::NaiveDate::from_ymd_opt(y, m, d).is_some()
}

fn parse_iso(value: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()
}

/// `2026-01-31` + `-30` → `2026-01-01` (UTC arithmetic, like the original).
pub fn shift_days(iso_date: &str, delta: i64) -> String {
    parse_iso(iso_date)
        .map(|d| (d + chrono::Duration::days(delta)).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| iso_date.to_string())
}

/// True when `iso_date` is a Saturday or Sunday.
pub fn is_weekend(iso_date: &str) -> bool {
    parse_iso(iso_date)
        .map(|d| matches!(d.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun))
        .unwrap_or(false)
}

/// Today's date in UTC, `YYYY-MM-DD` (the original uses `toISOString()`).
pub fn today_iso() -> String {
    chrono::Utc::now().date_naive().format("%Y-%m-%d").to_string()
}

/// Inclusive day count between two ISO dates.
pub fn window_days(start: &str, end: &str) -> i64 {
    match (parse_iso(start), parse_iso(end)) {
        (Some(s), Some(e)) => (e - s).num_days() + 1,
        _ => 0,
    }
}

/// The base, provider and symbol filter in force for one request.
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub base: String,
    pub provider: String,
}

// ------------------------------------------------------- parameter parsing

/// `toUpperCode` + `@Matches(CURRENCY_CODE)`.
pub fn parse_code(raw: &str, field: &str) -> Result<String, ApiError> {
    let upper = raw.trim().to_uppercase();
    let is_code = upper.len() == 3 && upper.bytes().all(|b| b.is_ascii_alphabetic());
    if !is_code {
        return Err(ApiError::validation_details(
            code_prefix(field),
            format!("{field} must be a 3-letter ISO 4217 code, e.g. USD"),
            obj(vec![(field, Value::String(raw.to_string()))]),
        ));
    }
    Ok(upper)
}

/// `toCodes`: comma-separated, upper-cased, de-duplicated code list.
pub fn parse_symbols(raw: Option<&str>) -> Result<Option<Vec<String>>, ApiError> {
    let Some(raw) = raw else { return Ok(None) };
    let mut codes: Vec<String> = Vec::new();
    for item in raw.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let upper = item.to_uppercase();
        let is_code = upper.len() == 3 && upper.bytes().all(|b| b.is_ascii_alphabetic());
        if !is_code {
            return Err(ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_SYMBOLS"),
                "each symbol must be a 3-letter ISO 4217 code, e.g. USD",
                obj(vec![("symbol", Value::String(item.to_string()))]),
            ));
        }
        if !codes.contains(&upper) {
            codes.push(upper);
        }
    }
    if codes.is_empty() {
        Ok(None)
    } else {
        Ok(Some(codes))
    }
}

/// `^(?:blend|[A-Z][A-Z0-9_]{1,15})$` applied to the normalised key.
pub fn is_provider_key(value: &str) -> bool {
    if value == BLEND {
        return true;
    }
    let bytes = value.as_bytes();
    (2..=16).contains(&bytes.len())
        && bytes[0].is_ascii_uppercase()
        && bytes[1..].iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
}

/// `toProvider`: trim; `blend`/`all` (any case) mean "do not pin"; else uppercase.
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

/// Validate the `provider` query parameter; `None`/empty → use the default.
pub fn parse_provider(raw: Option<&str>) -> Result<Option<String>, ApiError> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let normalised = normalise_provider(trimmed);
    if !is_provider_key(&normalised) {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_PROVIDER"),
            "provider must be a provider key (e.g. ECB) or the literal blend",
            obj(vec![("provider", Value::String(raw.to_string()))]),
        ));
    }
    Ok(Some(normalised))
}

/// `toBoolean` for flag-style parameters (`weekends_only`).
pub fn parse_bool(raw: Option<&str>) -> Result<Option<bool>, ApiError> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    match trimmed.to_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(Some(true)),
        "false" | "0" | "no" | "off" => Ok(Some(false)),
        _ => Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_FLAG"),
            "weekends_only must be a boolean value",
            obj(vec![("weekends_only", Value::String(raw.to_string()))]),
        )),
    }
}

/// `toAmount` + `@IsNumber`: query text becomes a number; `NaN` is rejected
/// here so the message can name the real problem.
pub fn parse_amount(raw: Option<&str>) -> Result<Option<f64>, ApiError> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    match trimmed.parse::<f64>() {
        Ok(v) => Ok(Some(v)),
        Err(_) => Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_AMOUNT"),
            "amount must be a number",
            obj(vec![("amount", Value::String(raw.to_string()))]),
        )),
    }
}

// ----------------------------------------------------------- window / pages

/// A validated date window.
#[derive(Debug, Clone)]
pub struct Window {
    pub start_date: String,
    pub end_date: String,
    pub days: i64,
}

/// Validates and defaults a date window.
///
/// Defaults to the last 30 days, which always contains publication days for
/// every provider. An inverted or oversized window is a 400 here rather than
/// the silent empty array the upstream returns.
pub fn resolve_window(start_date: Option<&str>, end_date: Option<&str>) -> Result<Window, ApiError> {
    for (value, field) in [(start_date, "start_date"), (end_date, "end_date")] {
        if let Some(value) = value {
            if !has_iso_shape(value) {
                return Err(ApiError::validation_details(
                    format!("{CODE_PREFIX}_INVALID_DATE"),
                    format!("{field} must be formatted YYYY-MM-DD"),
                    obj(vec![(field, Value::String(value.to_string()))]),
                ));
            }
            if !is_calendar_date(value) {
                return Err(ApiError::validation_details(
                    format!("{CODE_PREFIX}_INVALID_DATE"),
                    format!("{field} must be a real calendar date formatted YYYY-MM-DD"),
                    obj(vec![(field, Value::String(value.to_string()))]),
                ));
            }
        }
    }

    let today = today_iso();
    let end_date = end_date.unwrap_or(&today).to_string();
    let start_date = start_date
        .map(String::from)
        .unwrap_or_else(|| shift_days(&end_date, -29));

    if start_date > end_date {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_WINDOW"),
            "start_date must not be after end_date",
            obj(vec![
                ("start_date", Value::String(start_date.clone())),
                ("end_date", Value::String(end_date.clone())),
            ]),
        ));
    }

    let days = window_days(&start_date, &end_date);
    if days > MAX_RANGE_DAYS {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_RANGE_TOO_WIDE"),
            format!(
                "The requested window spans {days} days, which exceeds the {MAX_RANGE_DAYS} day limit. \
                 Request a shorter window or use /currency/rates/latest."
            ),
            obj(vec![
                ("start_date", Value::String(start_date.clone())),
                ("end_date", Value::String(end_date.clone())),
                ("days", json!(days)),
                ("limit", json!(MAX_RANGE_DAYS)),
            ]),
        ));
    }

    Ok(Window { start_date, end_date, days })
}

/// `pagination()`: limit defaults to 100 and is clamped to 1000; offset ≥ 0.
/// Out-of-range values are rejected in the parameter parsers, so this only
/// clamps.
pub fn resolve_pagination(limit: Option<i64>, offset: Option<i64>) -> Result<(i64, i64), ApiError> {
    if let Some(v) = limit {
        if v < 1 {
            return Err(ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_LIMIT"),
                "limit must be an integer greater than or equal to 1",
                obj(vec![("limit", json!(v))]),
            ));
        }
    }
    if let Some(v) = offset {
        if v < 0 {
            return Err(ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_OFFSET"),
                "offset must be an integer greater than or equal to 0",
                obj(vec![("offset", json!(v))]),
            ));
        }
    }
    Ok((limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT), offset.unwrap_or(0).max(0)))
}

/// `carriedForward` for a time series: true when the returned dates do not
/// line up with the requested window (each edge resolves to the nearest
/// publication on or before it).
pub fn is_carried_forward(dates: &[String], start_date: &str, end_date: &str) -> bool {
    match (dates.first(), dates.last()) {
        (Some(first), Some(last)) => last.as_str() < end_date || first.as_str() < start_date,
        _ => false,
    }
}

// ------------------------------------------------------------ row shaping

/// Sort by date then quote and group into `{date, rates}` buckets, rounding
/// each rate for display. Returns `(dates, series)` both in ascending order.
pub fn group_series(rows: &[RateRow]) -> (Vec<String>, Vec<(String, Map<String, Value>)>) {
    let mut sorted: Vec<&RateRow> = rows.iter().collect();
    sorted.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.quote.cmp(&b.quote)));

    let mut by_date: std::collections::BTreeMap<String, Map<String, Value>> =
        std::collections::BTreeMap::new();
    for row in sorted {
        let bucket = by_date.entry(row.date.clone()).or_default();
        bucket.insert(row.quote.clone(), json!(round_to(row.rate, RATE_DECIMALS)));
    }
    (by_date.keys().cloned().collect(), by_date.into_iter().collect())
}

/// Distinct quote currencies of a payload, sorted (used when the caller did
/// not pin any).
pub fn distinct_quotes(rows: &[RateRow]) -> Vec<String> {
    let set: BTreeSet<String> = rows.iter().map(|r| r.quote.clone()).collect();
    set.into_iter().collect()
}

/// `didYouMean`: same first letter, up to five candidates.
pub fn did_you_mean(codes: &[String], code: &str) -> Vec<String> {
    let first = code.chars().next();
    codes
        .iter()
        .filter(|c| c.as_str() != code && c.chars().next() == first)
        .take(5)
        .cloned()
        .collect()
}

/// 400 for a code the providers have never published (filter routes).
pub fn unknown_currency_error(all: &[CurrencyInfo], code: &str) -> ApiError {
    let mut seen: Vec<String> = Vec::new();
    for item in all {
        if !seen.contains(&item.code) {
            seen.push(item.code.clone());
        }
    }
    let suggestions = did_you_mean(&seen, code);
    let mut details = vec![("code", json!(code))];
    if !suggestions.is_empty() {
        details.push(("didYouMean", json!(suggestions)));
    }
    details.push(("hint", json!("GET /currency/currencies lists every supported code")));
    ApiError::validation_details(
        format!("{CODE_PREFIX}_UNKNOWN_CURRENCY"),
        format!("Unknown currency code: {code}"),
        obj(details),
    )
}

/// 404 for a well-formed but unknown single currency resource.
pub fn unknown_currency_404(all: &[CurrencyInfo], code: &str) -> ApiError {
    let mut details = vec![("code", json!(code))];
    let suggestions = did_you_mean(
        &all.iter().map(|c| c.code.clone()).collect::<Vec<_>>(),
        code,
    );
    if !suggestions.is_empty() {
        details.push(("didYouMean", json!(suggestions)));
    }
    details.push(("hint", json!("GET /currency/currencies lists every supported code")));
    ApiError::not_found_details(
        format!("{CODE_PREFIX}_NOT_FOUND"),
        format!("Unknown currency code: {code}"),
        obj(details),
    )
}

// ------------------------------------------------------------ orchestration

/// Resolve and validate base and provider for a request. Codes are checked
/// against the live reference list rather than a regex alone, so a well-formed
/// but non-existent provider never costs an upstream round trip.
pub async fn resolve_context(
    state: &AppState,
    base_url: &str,
    base: Option<&str>,
    provider: Option<&str>,
) -> Result<RequestContext, ApiError> {
    let provider = parse_provider(provider)?.unwrap_or_else(|| DEFAULT_PROVIDER.to_string());
    assert_known_provider(state, base_url, &provider).await?;
    let base = match base {
        Some(b) => parse_code(b, "base")?,
        None => DEFAULT_BASE.to_string(),
    };
    Ok(RequestContext { base, provider })
}

/// Cached read of the provider directory (`reference:providers`).
pub async fn providers_cached(
    state: &AppState,
    base_url: &str,
) -> Result<(Vec<ProviderInfo>, bool), ApiError> {
    let cached = httpx::cached_json(state, cache_names::CURRENCY, "reference:providers", || {
        client::fetch_providers(state, base_url)
    })
    .await?;
    Ok((client::parse_providers(&cached.body)?, cached.cached))
}

pub async fn assert_known_provider(
    state: &AppState,
    base_url: &str,
    provider: &str,
) -> Result<(), ApiError> {
    if provider == BLEND {
        return Ok(());
    }
    let (providers, _) = providers_cached(state, base_url).await?;
    if providers.iter().any(|p| p.key == provider) {
        return Ok(());
    }
    let needle: String = provider.chars().take(2).collect();
    let suggestions: Vec<String> = providers
        .iter()
        .map(|p| p.key.clone())
        .filter(|key| key.contains(&needle))
        .take(5)
        .collect();
    let mut details = vec![("provider", json!(provider))];
    if !suggestions.is_empty() {
        details.push(("suggestions", json!(suggestions)));
    }
    details.push((
        "hint",
        json!("Use provider=blend for the multi-provider consensus, or omit it for the default"),
    ));
    Err(ApiError::validation_details(
        format!("{CODE_PREFIX}_UNKNOWN_PROVIDER"),
        format!("Unknown provider: {provider}"),
        obj(details),
    ))
}

/// Cached read of a currency reference list. `scope` is `Some("all")` for the
/// retired-codes-inclusive list; `provider` is dropped for `blend`.
pub async fn currencies_cached(
    state: &AppState,
    base_url: &str,
    key: &str,
    scope: Option<&str>,
    provider: Option<&str>,
) -> Result<(Vec<CurrencyInfo>, bool), ApiError> {
    let cached = httpx::cached_json(state, cache_names::CURRENCY, key, || {
        client::fetch_currencies(state, base_url, scope, provider)
    })
    .await?;
    Ok((client::parse_currencies(&cached.body)?, cached.cached))
}

/// The full reference list, retired codes included.
pub async fn all_currencies(state: &AppState, base_url: &str) -> Result<Vec<CurrencyInfo>, ApiError> {
    let (items, _) = currencies_cached(state, base_url, "reference:currencies:all", Some("all"), None).await?;
    Ok(items)
}

/// Validates a code against the full reference list, including retired codes.
pub async fn assert_known_currency(
    state: &AppState,
    base_url: &str,
    code: &str,
) -> Result<CurrencyInfo, ApiError> {
    let all = all_currencies(state, base_url).await?;
    match all.iter().find(|c| c.code == code) {
        Some(found) => Ok(found.clone()),
        None => Err(unknown_currency_error(&all, code)),
    }
}

/// Validates each requested quote and sorts them so `symbols=USD,GBP` and
/// `symbols=GBP,USD` share a cache entry.
pub async fn assert_known_symbols(
    state: &AppState,
    base_url: &str,
    symbols: Option<Vec<String>>,
) -> Result<Option<Vec<String>>, ApiError> {
    let Some(list) = symbols else { return Ok(None) };
    for code in &list {
        assert_known_currency(state, base_url, code).await?;
    }
    let mut sorted = list;
    sorted.sort();
    Ok(Some(sorted))
}

/// Cached raw upstream payload through the shared `CURRENCY` cache. The
/// loader never errors for deterministic upstream failures: those are stored
/// as a sentinel and re-raised on every hit with their original status.
pub async fn cached_payload<F, Fut>(
    state: &AppState,
    key: &str,
    load: F,
) -> Result<(std::sync::Arc<Value>, bool), ApiError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Value, ApiError>>,
{
    let cached = httpx::cached_json(state, cache_names::CURRENCY, key, || async {
        match load().await {
            Ok(body) => Ok(body),
            // Deterministic failures survive the cache with their real status.
            Err(e) if client::deterministic_status(&e) => Ok(client::cache_error_value(&e)),
            Err(e) => Err(e),
        }
    })
    .await?;
    let value = client::decode_cached(&cached.body)?;
    Ok((std::sync::Arc::new(value), cached.cached))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::RateRow;

    fn row(date: &str, quote: &str, rate: f64) -> RateRow {
        RateRow { date: date.to_string(), base: "EUR".into(), quote: quote.into(), rate }
    }

    #[test]
    fn round_to_removes_float_artefacts() {
        assert_eq!(round_to(87.39000000000001, 6), 87.39);
        assert_eq!(round_to(0.1 + 0.2, 6), 0.3);
        assert_eq!(round_to(1.0, 6), 1.0);
        assert_eq!(round_to(f64::INFINITY, 6), f64::INFINITY, "non-finite passes through");
    }

    #[test]
    fn is_calendar_date_rejects_impossible_dates() {
        assert!(is_calendar_date("2026-02-28"));
        assert!(is_calendar_date("2028-02-29"), "2028 is a leap year");
        assert!(!is_calendar_date("2026-02-31"));
        assert!(!is_calendar_date("2026-13-01"));
        assert!(!is_calendar_date("2026-1-01"), "shape must be zero-padded");
        assert!(!is_calendar_date("not-a-date"));
    }

    #[test]
    fn resolve_window_defaults_to_the_last_30_days() {
        let window = resolve_window(None, None).expect("defaults");
        let today = today_iso();
        assert_eq!(window.end_date, today);
        assert_eq!(window.start_date, shift_days(&today, -29));
        assert_eq!(window.days, 30);
    }

    #[test]
    fn resolve_window_rejects_inverted_and_oversized_windows() {
        let err = resolve_window(Some("2026-05-10"), Some("2026-05-01")).unwrap_err();
        assert!(err.to_string().contains("must not be after"));
        let err = resolve_window(Some("1990-01-01"), Some("2026-01-01")).unwrap_err();
        assert!(err.to_string().contains("day limit"), "windows over 3660 days are rejected");
        let err = resolve_window(Some("2026-02-31"), None).unwrap_err();
        assert!(err.to_string().contains("real calendar date"));
        let err = resolve_window(None, Some("20260101")).unwrap_err();
        assert!(err.to_string().contains("must be formatted YYYY-MM-DD"));
    }

    #[test]
    fn pagination_defaults_clamps_and_rejects() {
        assert_eq!(resolve_pagination(None, None).unwrap(), (100, 0));
        assert_eq!(resolve_pagination(Some(25), Some(7)).unwrap(), (25, 7));
        assert_eq!(resolve_pagination(Some(5000), None).unwrap(), (1000, 0), "clamped to max");
        assert!(resolve_pagination(Some(0), None).is_err());
        assert!(resolve_pagination(None, Some(-1)).is_err());
    }

    #[test]
    fn provider_keys_normalise_like_the_original() {
        assert_eq!(normalise_provider("ecb"), "ECB");
        assert_eq!(normalise_provider("Blend"), "blend");
        assert_eq!(normalise_provider("all"), "blend");
        assert_eq!(normalise_provider("   "), "ECB", "empty falls back to the default");
        assert_eq!(normalise_provider("MY_BANK"), "MY_BANK");
        assert!(is_provider_key("ECB"));
        assert!(is_provider_key("blend"));
        assert!(!is_provider_key("1ABC"), "must start with a letter");
        assert!(!is_provider_key("x!"));
        assert!(!is_provider_key("A"), "single letters are rejected by the shape");
        assert!(parse_provider(Some(" ecb ")).unwrap().as_deref() == Some("ECB"));
        assert!(parse_provider(None).unwrap().is_none());
        assert!(parse_provider(Some("nope!")).is_err());
    }

    #[test]
    fn symbols_parse_uppercase_dedupe_and_sort() {
        let parsed = parse_symbols(Some("usd, GBP,usd")).unwrap().unwrap();
        assert_eq!(parsed, vec!["USD", "GBP"]);
        assert!(parse_symbols(Some("EURO")).is_err(), "four letters is not a code");
        assert!(parse_symbols(Some("")).unwrap().is_none());
        assert!(parse_symbols(None).unwrap().is_none());
    }

    #[test]
    fn series_groups_by_date_and_detects_carried_forward_edges() {
        let rows = vec![
            row("2026-01-06", "USD", 1.030499),
            row("2026-01-02", "USD", 1.0211115),
            row("2026-01-06", "GBP", 0.82),
        ];
        let (dates, series) = group_series(&rows);
        assert_eq!(dates, vec!["2026-01-02", "2026-01-06"]);
        assert_eq!(series.len(), 2);
        let (first_date, first_rates) = &series[0];
        assert_eq!(first_date, "2026-01-02");
        assert_eq!(first_rates.get("USD"), Some(&json!(1.021112)), "rate rounded to 6 decimals");
        let (_, second_rates) = &series[1];
        assert_eq!(second_rates.len(), 2, "both quotes share the 2026-01-06 bucket");
        assert_eq!(second_rates.get("GBP"), Some(&json!(0.82)));

        // The upstream resolves each window edge to the nearest publication on
        // or before it, so a window opening on a holiday shifts both ends.
        assert!(is_carried_forward(&dates, "2026-01-05", "2026-01-07"));
        assert!(!is_carried_forward(&["2026-01-05".to_string()], "2026-01-05", "2026-01-05"));
    }

    #[test]
    fn distinct_quotes_are_sorted() {
        let rows = vec![row("2026-01-02", "GBP", 0.8), row("2026-01-02", "USD", 1.02)];
        assert_eq!(distinct_quotes(&rows), vec!["GBP", "USD"]);
    }

    #[test]
    fn weekend_detection_matches_utc_semantics() {
        assert!(is_weekend("2026-10-17"), "2026-10-17 is a Saturday");
        assert!(is_weekend("2026-10-18"), "2026-10-18 is a Sunday");
        assert!(!is_weekend("2026-10-19"), "2026-10-19 is a Monday");
        assert_eq!(window_days("2026-01-01", "2026-01-31"), 31);
        assert_eq!(shift_days("2026-01-01", -1), "2025-12-31");
    }

    #[test]
    fn bool_and_amount_parameters_behave_like_the_transforms() {
        assert_eq!(parse_bool(Some("yes")).unwrap(), Some(true));
        assert_eq!(parse_bool(Some("0")).unwrap(), Some(false));
        assert_eq!(parse_bool(Some("")).unwrap(), None);
        assert!(parse_bool(Some("maybe")).is_err());
        assert_eq!(parse_amount(Some("12.5")).unwrap(), Some(12.5));
        assert_eq!(parse_amount(Some("-5")).unwrap(), Some(-5.0), "positivity is a later check");
        assert!(parse_amount(Some("abc")).is_err());
        assert_eq!(parse_amount(None).unwrap(), None);
    }

    #[test]
    fn did_you_mean_suggests_same_first_letter() {
        let codes = vec!["USD".to_string(), "USN".to_string(), "EUR".to_string()];
        assert_eq!(did_you_mean(&codes, "USX"), vec!["USD", "USN"]);
        assert!(did_you_mean(&codes, "USD").is_empty(), "the exact code is not a suggestion");
    }

    #[test]
    fn unknown_currency_errors_carry_a_hint_and_suggestions() {
        let all = vec![
            CurrencyInfo { code: "USD".into(), name: "US Dollar".into(), symbol: None, numeric: None, start_date: None, end_date: None, provider_count: None },
            CurrencyInfo { code: "EUR".into(), name: "Euro".into(), symbol: None, numeric: None, start_date: None, end_date: None, provider_count: None },
        ];
        let err = unknown_currency_error(&all, "USX");
        let text = err.to_string();
        assert!(text.contains("Unknown currency code: USX"));
        let err = unknown_currency_404(&all, "XXY");
        assert!(matches!(err, ApiError::NotFound { .. }), "single resource miss is a 404");
    }
}
