//! Route handlers for the currency service.

use std::collections::BTreeMap;
use std::sync::Arc;

use actix_web::web::{self, Json};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use utoipa::{IntoParams, OpenApi};

use master_common::envelope::Envelope;
use master_common::error::ApiError;
use master_common::httpx;
use master_common::state::{cache_names, AppState};

use crate::{
    is_calendar_date, is_weekend, normalise_provider, round_to, shift_days, BLEND, CODE_PREFIX,
    DEFAULT_BASE, DEFAULT_LIMIT, DEFAULT_PROVIDER, MAX_LIMIT, MAX_RANGE_DAYS, MAX_SYMBOLS,
    RATE_DECIMALS, SERVICE_PREFIX, TTL_REFERENCE_SECS, UPSTREAM_FRANKFURTER,
};

// ------------------------------------------------------------- upstream I/O

/// GET against Frankfurter v2 with the shared resilient client.
async fn ff_get(state: &AppState, path_and_query: &str) -> Result<Value, ApiError> {
    let url = format!("{UPSTREAM_FRANKFURTER}{path_and_query}");
    httpx::get_json(state, &url, &[], CODE_PREFIX).await.map(|t| t.body)
}

/// `RateRow`: {date, base, quote, rate}.
fn parse_rate_rows(payload: &Value) -> Result<Vec<Value>, ApiError> {
    let arr = payload
        .as_array()
        .ok_or_else(|| ApiError::upstream(format!("{CODE_PREFIX}_PARSE_ERROR"), "Expected a JSON array of rate rows from the upstream".to_string()))?;
    let mut rows = Vec::with_capacity(arr.len());
    for row in arr {
        let date = row.get("date").and_then(|v| v.as_str()).unwrap_or("");
        let base = row.get("base").and_then(|v| v.as_str()).unwrap_or("");
        let quote = row.get("quote").and_then(|v| v.as_str()).unwrap_or("");
        let rate = row.get("rate").and_then(|v| v.as_f64());
        if date.is_empty() || base.is_empty() || quote.is_empty() || rate.is_none() {
            continue;
        }
        rows.push(json!({ "date": date, "base": base, "quote": quote, "rate": rate.unwrap() }));
    }
    Ok(rows)
}

fn parse_rate_row(payload: &Value) -> Result<Value, ApiError> {
    let rows = parse_rate_rows(&json!([payload]))?;
    rows.into_iter().next().ok_or_else(|| {
        ApiError::upstream(format!("{CODE_PREFIX}_PARSE_ERROR"), "Expected one rate row from the upstream".to_string())
    })
}

/// `CurrencyInfo`: {code, name, symbol?, numeric?, startDate?, endDate?, providerCount?}.
fn parse_currencies(payload: &Value) -> Result<Vec<Value>, ApiError> {
    let arr = payload
        .as_array()
        .ok_or_else(|| ApiError::upstream(format!("{CODE_PREFIX}_PARSE_ERROR"), "Expected a JSON array of currencies from the upstream".to_string()))?;
    Ok(arr
        .iter()
        .filter_map(|c| {
            let code = c.get("iso_code").and_then(|v| v.as_str())?;
            let mut out = json!({
                "code": code,
                "name": c.get("name").and_then(|v| v.as_str()).unwrap_or(code),
            });
            if let Some(s) = c.get("symbol").and_then(|v| v.as_str()) {
                out["symbol"] = json!(s);
            }
            if let Some(n) = c.get("iso_numeric").and_then(|v| v.as_str()) {
                out["numeric"] = json!(n);
            }
            if let Some(d) = c.get("start_date").and_then(|v| v.as_str()) {
                out["startDate"] = json!(d);
            }
            if let Some(d) = c.get("end_date").and_then(|v| v.as_str()) {
                out["endDate"] = json!(d);
            }
            if let Some(p) = c.get("providers").and_then(|v| v.as_array()) {
                out["providerCount"] = json!(p.len());
            }
            Some(out)
        })
        .collect())
}

/// `ProviderInfo` passthrough with camelCase projection.
fn parse_providers(payload: &Value) -> Result<Vec<Value>, ApiError> {
    let arr = payload
        .as_array()
        .ok_or_else(|| ApiError::upstream(format!("{CODE_PREFIX}_PARSE_ERROR"), "Expected a JSON array of providers from the upstream".to_string()))?;
    Ok(arr
        .iter()
        .map(|p| {
            let mut out = json!({ "key": p.get("key").and_then(|v| v.as_str()).unwrap_or("") });
            for (src, dst) in [
                ("name", "name"),
                ("country_code", "countryCode"),
                ("rate_type", "rateType"),
                ("pivot_currency", "pivotCurrency"),
                ("data_url", "dataUrl"),
                ("terms_url", "termsUrl"),
                ("start_date", "startDate"),
                ("end_date", "endDate"),
            ] {
                if let Some(v) = p.get(src) {
                    if !v.is_null() {
                        out[dst] = v.clone();
                    }
                }
            }
            out
        })
        .collect())
}

// ------------------------------------------------------------ cached access

struct Cached {
    value: Value,
}

async fn cached_load<F, Fut>(state: &AppState, key: &str, ttl_secs: u64, load: F) -> Result<Cached, ApiError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Value, ApiError>>,
{
    let cache = state.json_cache(cache_names::CURRENCY);
    let full_key = format!("{key}@{ttl_secs}");
    if let Some(v) = cache.get(&full_key).await {
        return Ok(Cached { value: v.as_ref().clone() });
    }
    let loaded = load().await?;
    cache.insert(full_key, Arc::new(loaded.clone())).await;
    Ok(Cached { value: loaded })
}

// -------------------------------------------------------------- validation

fn invalid(code: &str, message: impl Into<String>) -> ApiError {
    ApiError::validation(format!("{CODE_PREFIX}_{code}"), message)
}

fn not_found(code: &str, message: impl Into<String>, details: Value) -> ApiError {
    ApiError::not_found_details(format!("{CODE_PREFIX}_{code}"), message, details)
}

fn validate_code_shape(code: &str, field: &str) -> Result<(), ApiError> {
    if code.len() == 3 && code.chars().all(|c| c.is_ascii_alphabetic()) {
        Ok(())
    } else {
        Err(invalid(
            "INVALID_REQUEST",
            format!("{field} must be a three-letter ISO 4217 code, received \"{code}\""),
        ))
    }
}

/// Resolves and validates {provider, base} for a request.
async fn request_context(
    state: &AppState,
    base: &Option<String>,
    provider_param: &Option<String>,
) -> Result<(String, String), ApiError> {
    let provider = match provider_param {
        Some(p) => normalise_provider(p),
        None => DEFAULT_PROVIDER.to_string(),
    };
    if provider != BLEND {
        assert_known_provider(state, &provider).await?;
    }
    let base = match base {
        Some(b) => {
            let upper = b.trim().to_uppercase();
            validate_code_shape(&upper, "base")?;
            upper
        }
        None => DEFAULT_BASE.to_string(),
    };
    Ok((provider, base))
}

async fn assert_known_provider(state: &AppState, provider: &str) -> Result<(), ApiError> {
    if provider == BLEND {
        return Ok(());
    }
    let providers = fetch_providers_cached(state).await?;
    let known = providers
        .as_array()
        .map(|a| a.iter().any(|p| p.get("key").and_then(|v| v.as_str()) == Some(provider)))
        .unwrap_or(false);
    if known {
        return Ok(());
    }
    let suggestions: Vec<String> = providers
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| p.get("key").and_then(|v| v.as_str()))
                .filter(|k| k.contains(&provider[..provider.len().min(2)]))
                .take(5)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let mut details = json!({ "provider": provider, "hint": "Use provider=blend for the multi-provider consensus, or omit it for the default" });
    if !suggestions.is_empty() {
        details["suggestions"] = json!(suggestions);
    }
    Err(ApiError::validation_details(
        format!("{CODE_PREFIX}_INVALID_REQUEST"),
        format!("Unknown provider: {provider}"),
        details,
    ))
}

async fn fetch_providers_cached(state: &AppState) -> Result<Value, ApiError> {
    cached_load(state, "reference:providers", TTL_REFERENCE_SECS, || async {
        let payload = ff_get(state, "/v2/providers").await?;
        parse_providers(&payload).map(|rows| Value::Array(rows))
    })
    .await
    .map(|c| c.value)
}

async fn fetch_currencies_cached(state: &AppState, scope: Option<&str>, provider: Option<&str>) -> Result<Value, ApiError> {
    let mut path = String::from("/v2/currencies");
    let mut pairs: Vec<(String, String)> = Vec::new();
    if let Some(s) = scope {
        pairs.push(("scope".into(), s.to_string()));
    }
    if let Some(p) = provider {
        if p != BLEND {
            pairs.push(("providers".into(), p.to_string()));
        }
    }
    if !pairs.is_empty() {
        path.push('?');
        path.push_str(
            &pairs
                .iter()
                .map(|(k, v)| format!("{k}={}", v))
                .collect::<Vec<_>>()
                .join("&"),
        );
    }
    let key = format!("reference:currencies:{:?}:{:?}", scope, provider);
    cached_load(state, &key, TTL_REFERENCE_SECS, || async {
        let payload = ff_get(state, &path).await?;
        parse_currencies(&payload).map(|rows| Value::Array(rows))
    })
    .await
    .map(|c| c.value)
}

/// Validates a code against the full reference list (including retired codes).
async fn assert_known_currency(state: &AppState, code: &str) -> Result<Value, ApiError> {
    if let Some(found) = find_currency(state, code).await? {
        return Ok(found);
    }
    // "Did you mean" suggestions share the first letter of the unknown code.
    let did_you_mean: Vec<String> = fetch_currencies_cached(state, Some("all"), None)
        .await
        .ok()
        .and_then(|all| {
            let mut list: Vec<String> = all
                .as_array()?
                .iter()
                .filter_map(|c| c.get("code").and_then(|v| v.as_str()))
                .filter(|k| !code.is_empty() && k.starts_with(&code[..1]) && *k != code)
                .take(5)
                .map(String::from)
                .collect();
            list.sort();
            Some(list)
        })
        .unwrap_or_default();
    let mut details = json!({
        "code": code,
        "hint": "GET /currency/currencies lists every supported code",
    });
    if !did_you_mean.is_empty() {
        details["didYouMean"] = json!(did_you_mean);
    }
    Err(ApiError::validation_details(
        format!("{CODE_PREFIX}_INVALID_REQUEST"),
        format!("Unknown currency code: {code}"),
        details,
    ))
}

async fn find_currency(state: &AppState, code: &str) -> Result<Option<Value>, ApiError> {
    let all = fetch_currencies_cached(state, Some("all"), None).await?;
    Ok(all
        .as_array()
        .and_then(|a| a.iter().find(|c| c.get("code").and_then(|v| v.as_str()) == Some(code)))
        .cloned())
}

async fn assert_known_symbols(state: &AppState, symbols: &Option<String>) -> Result<Option<Vec<String>>, ApiError> {
    let Some(csv) = symbols.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let codes: Vec<String> = csv
        .split(',')
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty())
        .collect();
    if codes.len() > MAX_SYMBOLS {
        return Err(invalid(
            "INVALID_REQUEST",
            format!("symbols accepts at most {MAX_SYMBOLS} codes, received {}", codes.len()),
        ));
    }
    for code in &codes {
        validate_code_shape(code, "symbols")?;
        assert_known_currency(state, code).await?;
    }
    let mut sorted = codes;
    sorted.sort();
    Ok(Some(sorted))
}

/// Validates and defaults a date window (last 30 days by default).
fn resolve_window(
    start_date: &Option<String>,
    end_date: &Option<String>,
) -> Result<(String, String), ApiError> {
    if let Some(v) = start_date {
        if !is_calendar_date(v) {
            return Err(invalid(
                "INVALID_REQUEST",
                "start_date must be a real calendar date formatted YYYY-MM-DD",
            ));
        }
    }
    if let Some(v) = end_date {
        if !is_calendar_date(v) {
            return Err(invalid(
                "INVALID_REQUEST",
                "end_date must be a real calendar date formatted YYYY-MM-DD",
            ));
        }
    }
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let end = end_date.clone().unwrap_or_else(|| today.clone());
    let start = start_date.clone().unwrap_or_else(|| shift_days(&end, -29));
    if start > end {
        return Err(invalid(
            "INVALID_REQUEST",
            format!("start_date must not be after end_date"),
        ));
    }
    let days = match (
        chrono::NaiveDate::parse_from_str(&start, "%Y-%m-%d"),
        chrono::NaiveDate::parse_from_str(&end, "%Y-%m-%d"),
    ) {
        (Ok(s), Ok(e)) => (e - s).num_days() + 1,
        _ => 0,
    };
    if days > MAX_RANGE_DAYS {
        return Err(invalid(
            "INVALID_REQUEST",
            format!(
                "The requested window spans {days} days, which exceeds the {MAX_RANGE_DAYS} day limit. Request a shorter window or use /currency/rates/latest."
            ),
        ));
    }
    Ok((start, end))
}

fn pagination(limit: Option<usize>, offset: Option<usize>) -> (usize, usize) {
    let l = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    (l, offset.unwrap_or(0))
}

fn envelope_json(data: Value, provider: &str, cached: bool, started: std::time::Instant) -> Value {
    Envelope::ok(
        data,
        json!({
            "upstream": UPSTREAM_FRANKFURTER,
            "provider": provider,
            "cached": cached,
            "durationMs": started.elapsed().as_millis() as u64,
        }),
    )
    .to_json()
}

// ------------------------------------------------------------- query DTOs

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LatestRatesQuery {
    /// Base currency (default EUR).
    pub base: Option<String>,
    /// Comma-separated quote codes, e.g. `EUR,GBP`.
    pub symbols: Option<String>,
    /// Provider key, or `blend` for the multi-provider consensus.
    pub provider: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RatesSeriesQuery {
    /// Window start (YYYY-MM-DD; defaults to end_date - 29 days).
    pub start_date: Option<String>,
    /// Window end (YYYY-MM-DD; defaults to today).
    pub end_date: Option<String>,
    pub base: Option<String>,
    pub symbols: Option<String>,
    pub provider: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PublicationDatesQuery {
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub base: Option<String>,
    pub provider: Option<String>,
    /// Only list publication days that fall on a weekend.
    pub weekends_only: Option<bool>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ConvertQuery {
    /// Source currency (default base).
    pub from: Option<String>,
    /// Target currency (required).
    pub to: Option<String>,
    /// Amount to convert (> 0; default 1).
    pub amount: Option<f64>,
    /// Resolve as of a date (YYYY-MM-DD).
    pub date: Option<String>,
    pub provider: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CurrenciesQuery {
    /// `active` (default) or `all` (includes retired codes).
    pub scope: Option<String>,
    /// Substring filter on code or name.
    pub search: Option<String>,
    pub provider: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    let scope = web::scope(SERVICE_PREFIX)
        .route("/rates/latest", web::get().to(latest_rates))
        .route("/rates/dates", web::get().to(publication_dates))
        .route("/rates", web::get().to(rate_series))
        .route("/convert", web::get().to(convert))
        .route("/currencies/{code}", web::get().to(currency_by_code))
        .route("/currencies", web::get().to(currencies))
        .route("/providers", web::get().to(providers))
        .route("/health/ready", web::get().to(health_ready))
        .route("/health", web::get().to(health));
    cfg.service(scope);
}

// ---------------------------------------------------------------- handlers

/// `GET /currency/rates/latest`
#[utoipa::path(get, path = "/currency/rates/latest", context_path = "/currency", params(LatestRatesQuery),
    responses((status = 200, description = "Latest reference rates")))]
async fn latest_rates(
    state: web::Data<AppState>,
    query: web::Query<LatestRatesQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let (provider, base) = request_context(&state, &query.base, &query.provider).await?;
    let quotes = assert_known_symbols(&state, &query.symbols).await?;

    let cache_key = format!("rates:latest:{base}:{:?}:{provider}", quotes);
    let cache = state.json_cache(cache_names::CURRENCY);
    let cached_hit = cache.get(&cache_key).await;
    let (rows, cached): (Vec<Value>, bool) = match cached_hit {
        Some(v) => (
            v.as_array().cloned().unwrap_or_default(),
            true,
        ),
        None => {
            let mut path = format!("/v2/rates?base={base}");
            if let Some(q) = &quotes {
                path.push_str(&format!("&quotes={}", q.join(",")));
            }
            if provider != BLEND {
                path.push_str(&format!("&providers={provider}"));
            }
            let payload = ff_get(&state, &path).await?;
            let rows = parse_rate_rows(&payload)?;
            cache.insert(cache_key, Arc::new(Value::Array(rows.clone()))).await;
            (rows, false)
        }
    };

    if rows.is_empty() {
        return Err(not_found(
            "NOT_FOUND",
            "The provider has no rate for the requested base and symbols. A retired currency has no rate on any current publication day.",
            json!({ "base": base, "symbols": quotes, "provider": provider }),
        ));
    }

    let mut dates: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get("date").and_then(|v| v.as_str()).map(String::from))
        .collect();
    dates.sort();
    dates.dedup();

    let mut rates: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut row = r.clone();
            if let Some(rate) = r.get("rate").and_then(|v| v.as_f64()) {
                row["rate"] = json!(round_to(rate, RATE_DECIMALS));
            }
            row
        })
        .collect();
    rates.sort_by(|a, b| {
        let qa = a.get("quote").and_then(|v| v.as_str()).unwrap_or("");
        let qb = b.get("quote").and_then(|v| v.as_str()).unwrap_or("");
        qa.cmp(qb)
    });

    let count = rates.len();
    let body = envelope_json(
        json!({
            "base": base,
            "provider": provider,
            "asOf": dates.last().cloned().unwrap_or_default(),
            "dates": dates,
            "carriedForward": dates.len() > 1,
            "rates": rates,
            "count": count,
        }),
        &provider,
        cached,
        started,
    );
    Ok(Json(body))
}

/// `GET /currency/rates` — time series grouped by publication date.
#[utoipa::path(get, path = "/currency/rates", context_path = "/currency", params(RatesSeriesQuery),
    responses((status = 200, description = "Daily time series")))]
async fn rate_series(
    state: web::Data<AppState>,
    query: web::Query<RatesSeriesQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let (provider, base) = request_context(&state, &query.base, &query.provider).await?;
    let quotes = assert_known_symbols(&state, &query.symbols).await?;
    let (start, end) = resolve_window(&query.start_date, &query.end_date)?;

    let cache_key = format!("rates:series:{base}:{:?}:{provider}:{start}:{end}", quotes);
    let cache = state.json_cache(cache_names::CURRENCY);
    let cached_hit = cache.get(&cache_key).await;
    let (rows, cached): (Vec<Value>, bool) = match cached_hit {
        Some(v) => (v.as_array().cloned().unwrap_or_default(), true),
        None => {
            let mut path = format!("/v2/rates?base={base}&from={start}&to={end}");
            if let Some(q) = &quotes {
                path.push_str(&format!("&quotes={}", q.join(",")));
            }
            if provider != BLEND {
                path.push_str(&format!("&providers={provider}"));
            }
            let payload = ff_get(&state, &path).await?;
            let rows = parse_rate_rows(&payload)?;
            cache.insert(cache_key, Arc::new(Value::Array(rows.clone()))).await;
            (rows, false)
        }
    };

    if rows.is_empty() {
        return Err(not_found(
            "NOT_FOUND",
            "The provider published no rates in the requested window. Reference rates only exist on publication days, so a window with no publication day in it is empty.",
            json!({ "start_date": start, "end_date": end, "base": base, "provider": provider }),
        ));
    }

    let mut sorted = rows.clone();
    sorted.sort_by(|a, b| {
        let da = a.get("date").and_then(|v| v.as_str()).unwrap_or("");
        let db = b.get("date").and_then(|v| v.as_str()).unwrap_or("");
        let qa = a.get("quote").and_then(|v| v.as_str()).unwrap_or("");
        let qb = b.get("quote").and_then(|v| v.as_str()).unwrap_or("");
        da.cmp(db).then_with(|| qa.cmp(qb))
    });

    let mut by_date: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for row in &sorted {
        let date = row.get("date").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let quote = row.get("quote").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let rate = row.get("rate").and_then(|v| v.as_f64()).unwrap_or(0.0);
        by_date.entry(date).or_default().insert(quote, json!(round_to(rate, RATE_DECIMALS)));
    }
    let dates: Vec<String> = by_date.keys().cloned().collect();
    let series: Vec<Value> = by_date
        .iter()
        .map(|(date, rates)| json!({ "date": date, "rates": rates }))
        .collect();

    let (limit, offset) = pagination(query.limit, query.offset);
    let total = series.len();
    let page: Vec<Value> = series.into_iter().skip(offset).take(limit).collect();
    let all_quotes: Vec<String> = quotes.unwrap_or_else(|| {
        let mut q: Vec<String> = sorted
            .iter()
            .filter_map(|r| r.get("quote").and_then(|v| v.as_str()).map(String::from))
            .collect();
        q.sort();
        q.dedup();
        q
    });

    let body = envelope_json(
        json!({
            "base": base,
            "provider": provider,
            "startDate": start,
            "endDate": end,
            "dates": dates,
            "series": page,
            "carriedForward": dates.last().map(|d| d.as_str() < end.as_str()).unwrap_or(false)
                || dates.first().map(|d| d.as_str() < start.as_str()).unwrap_or(false),
            "quotes": all_quotes,
            "total": total,
            "limit": limit,
            "offset": offset,
        }),
        &provider,
        cached,
        started,
    );
    Ok(Json(body))
}

/// `GET /currency/rates/dates`
#[utoipa::path(get, path = "/currency/rates/dates", context_path = "/currency", params(PublicationDatesQuery),
    responses((status = 200, description = "Publication dates")))]
async fn publication_dates(
    state: web::Data<AppState>,
    query: web::Query<PublicationDatesQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let (provider, base) = request_context(&state, &query.base, &query.provider).await?;
    // One reference quote is enough to enumerate publication days cheaply.
    assert_known_currency(&state, &base).await?;
    let (start, end) = resolve_window(&query.start_date, &query.end_date)?;

    let cache_key = format!("rates:dates:{base}:{provider}:{start}:{end}");
    let cache = state.json_cache(cache_names::CURRENCY);
    let cached_hit = cache.get(&cache_key).await;
    let (rows, cached): (Vec<Value>, bool) = match cached_hit {
        Some(v) => (v.as_array().cloned().unwrap_or_default(), true),
        None => {
            let mut path = format!("/v2/rates?base={base}&quotes={base}&from={start}&to={end}");
            if provider != BLEND {
                path.push_str(&format!("&providers={provider}"));
            }
            let payload = ff_get(&state, &path).await?;
            let rows = parse_rate_rows(&payload)?;
            cache.insert(cache_key, Arc::new(Value::Array(rows.clone()))).await;
            (rows, false)
        }
    };

    if rows.is_empty() {
        return Err(not_found(
            "NOT_FOUND",
            "The provider published no rates in the requested window",
            json!({ "start_date": start, "end_date": end, "base": base, "provider": provider }),
        ));
    }

    let mut all: Vec<Value> = rows
        .iter()
        .filter_map(|r| r.get("date").and_then(|v| v.as_str()))
        .map(|date| json!({ "date": date, "weekend": is_weekend(date) }))
        .collect();
    all.sort_by(|a, b| {
        let da = a.get("date").and_then(|v| v.as_str()).unwrap_or("");
        let db = b.get("date").and_then(|v| v.as_str()).unwrap_or("");
        da.cmp(db)
    });

    let filtered: Vec<Value> = if query.weekends_only.unwrap_or(false) {
        all.into_iter().filter(|e| e.get("weekend").and_then(|v| v.as_bool()).unwrap_or(false)).collect()
    } else {
        all
    };
    if filtered.is_empty() {
        return Err(not_found(
            "NOT_FOUND",
            if query.weekends_only.unwrap_or(false) {
                "The provider published no weekend rates in the requested window. A provider pinned to a TARGET business-day calendar never does; try provider=blend."
            } else {
                "The provider published no rates in the requested window"
            },
            json!({ "start_date": start, "end_date": end, "weekendsOnly": query.weekends_only.unwrap_or(false), "provider": provider }),
        ));
    }

    let (limit, offset) = pagination(query.limit, query.offset);
    let total = filtered.len();
    let items: Vec<Value> = filtered.into_iter().skip(offset).take(limit).collect();
    let last_date = items.last().and_then(|i| i.get("date")).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let first_date = items.first().and_then(|i| i.get("date")).and_then(|v| v.as_str()).unwrap_or("").to_string();

    let body = envelope_json(
        json!({
            "items": items,
            "total": total,
            "limit": limit,
            "offset": offset,
            "base": base,
            "provider": provider,
            "startDate": start,
            "endDate": end,
            "carriedForward": last_date < end || first_date < start,
        }),
        &provider,
        cached,
        started,
    );
    Ok(Json(body))
}

/// `GET /currency/convert`
#[utoipa::path(get, path = "/currency/convert", context_path = "/currency", params(ConvertQuery),
    responses((status = 200, description = "Conversion result")))]
async fn convert(
    state: web::Data<AppState>,
    query: web::Query<ConvertQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let provider_param = &query.provider;
    let (provider, default_base) = request_context(&state, &None, provider_param).await?;

    let to = query
        .to
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            invalid("INVALID_REQUEST", "`to` is required, e.g. /currency/convert?from=USD&to=EUR&amount=100")
        })?
        .to_uppercase();
    validate_code_shape(&to, "to")?;
    if let Some(d) = &query.date {
        if !is_calendar_date(d) {
            return Err(invalid(
                "INVALID_REQUEST",
                "date must be a real calendar date formatted YYYY-MM-DD",
            ));
        }
    }
    let from = match &query.from {
        Some(f) => {
            let upper = f.trim().to_uppercase();
            validate_code_shape(&upper, "from")?;
            upper
        }
        None => default_base.clone(),
    };
    let amount = query.amount.unwrap_or(1.0);
    if !amount.is_finite() {
        return Err(invalid("INVALID_REQUEST", "amount must be a finite number"));
    }
    if amount <= 0.0 {
        return Err(invalid("INVALID_REQUEST", "amount must be greater than zero"));
    }

    let from_info = assert_known_currency(&state, &from).await?;
    let to_info = assert_known_currency(&state, &to).await?;

    let date_key = query.date.clone().unwrap_or_else(|| "latest".into());
    let cache_key = format!("convert:{from}:{to}:{date_key}:{provider}");
    let cache = state.json_cache(cache_names::CURRENCY);
    let cached_hit = cache.get(&cache_key).await;
    let (row, cached): (Value, bool) = match cached_hit {
        Some(v) => (v.as_ref().clone(), true),
        None => {
            let mut path = format!("/v2/rate/{from}/{to}");
            if let Some(d) = &query.date {
                path.push_str(&format!("?date={d}"));
            }
            if provider != BLEND {
                path.push_str(&if query.date.is_some() { "&providers=" } else { "?providers=" });
                path.push_str(&provider);
            }
            let payload = ff_get(&state, &path).await?;
            let row = parse_rate_row(&payload)?;
            cache.insert(cache_key, Arc::new(row.clone())).await;
            (row, false)
        }
    };

    let rate = row.get("rate").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let row_date = row.get("date").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let row_base = row.get("base").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let carried_forward = query.date.is_some() && row_date != query.date.as_deref().unwrap_or("");

    let body = envelope_json(
        json!({
            "from": from,
            "to": to,
            "amount": amount,
            "rate": round_to(rate, RATE_DECIMALS),
            "result": round_to(amount * rate, RATE_DECIMALS),
            "date": row_date,
            "requestedDate": query.date,
            "carriedForward": carried_forward,
            "base": row_base,
            "provider": provider,
            "fromNumeric": from_info.get("numeric"),
            "toNumeric": to_info.get("numeric"),
        }),
        &provider,
        cached,
        started,
    );
    Ok(Json(body))
}

/// `GET /currency/currencies`
#[utoipa::path(get, path = "/currency/currencies", context_path = "/currency", params(CurrenciesQuery),
    responses((status = 200, description = "Supported currency codes")))]
async fn currencies(
    state: web::Data<AppState>,
    query: web::Query<CurrenciesQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let scope = query.scope.as_deref().unwrap_or("active");
    if scope != "active" && scope != "all" {
        return Err(invalid("INVALID_REQUEST", "scope must be either \"active\" or \"all\""));
    }
    let provider = match &query.provider {
        Some(p) if !p.trim().is_empty() => {
            let norm = normalise_provider(p);
            if norm != BLEND {
                assert_known_provider(&state, &norm).await?;
            }
            norm
        }
        _ => BLEND.to_string(),
    };

    let mut reference = fetch_currencies_cached(
        &state,
        if scope == "all" { Some("all") } else { None },
        if provider == BLEND { None } else { Some(provider.as_str()) },
    )
    .await?;

    // The pinned upstream list includes retired codes regardless of scope, so
    // intersect it with the unpinned active list for scope=active.
    if provider != BLEND && scope == "active" {
        let active = fetch_currencies_cached(&state, None, None).await?;
        let active_codes: std::collections::HashSet<String> = active
            .as_array()
            .map(|a| a.iter().filter_map(|c| c.get("code").and_then(|v| v.as_str()).map(String::from)).collect())
            .unwrap_or_default();
        if let Some(arr) = reference.as_array_mut() {
            arr.retain(|c| {
                c.get("code").and_then(|v| v.as_str()).map(|k| active_codes.contains(k)).unwrap_or(false)
            });
        }
    }

    let mut items: Vec<Value> = reference.as_array().cloned().unwrap_or_default();
    items.sort_by(|a, b| {
        let ka = a.get("code").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("code").and_then(|v| v.as_str()).unwrap_or("");
        ka.cmp(kb)
    });
    if let Some(needle) = query.search.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()) {
        items.retain(|item| {
            let code = item.get("code").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
            code.contains(&needle) || name.contains(&needle)
        });
    }

    let (limit, offset) = pagination(query.limit, query.offset);
    let total = items.len();
    let page: Vec<Value> = items.into_iter().skip(offset).take(limit).collect();

    let body = envelope_json(
        json!({
            "items": page,
            "total": total,
            "limit": limit,
            "offset": offset,
            "scope": scope,
            "provider": provider,
        }),
        &provider,
        false,
        started,
    );
    Ok(Json(body))
}

/// `GET /currency/currencies/{code}`
#[utoipa::path(get, path = "/currency/currencies/{code}", context_path = "/currency",
    params(("code" = String, Path, description = "ISO 4217 code, e.g. USD")),
    responses((status = 200, description = "One currency"), (status = 404, description = "Unknown code")))]
async fn currency_by_code(state: web::Data<AppState>, path: web::Path<String>) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let code = path.into_inner().trim().to_uppercase();
    validate_code_shape(&code, "code")?;

    let known = find_currency(&state, &code).await?;
    if known.is_none() {
        return Err(not_found(
            "NOT_FOUND",
            format!("Unknown currency code: {code}"),
            json!({ "code": code, "hint": "GET /currency/currencies lists every supported code" }),
        ));
    }

    let cache = state.json_cache(cache_names::CURRENCY);
    let cache_key = format!("reference:currency:{code}");
    let cached_hit = cache.get(&cache_key).await;
    let (currency, cached): (Value, bool) = match cached_hit {
        Some(v) => (v.as_ref().clone(), true),
        None => {
            let payload = ff_get(&state, &format!("/v2/currency/{code}")).await?;
            // The single-currency endpoint answers an object; the list endpoint answers an array.
            let shaped = if payload.is_object() { json!([payload]) } else { payload };
            let mut list = parse_currencies(&shaped)?;
            let one = list.pop().ok_or_else(|| {
                ApiError::upstream(format!("{CODE_PREFIX}_PARSE_ERROR"), "Expected one currency from the upstream".to_string())
            })?;
            cache.insert(cache_key, Arc::new(one.clone())).await;
            (one, false)
        }
    };

    let body = envelope_json(
        json!({ "currency": currency, "defaultProvider": DEFAULT_PROVIDER }),
        DEFAULT_PROVIDER,
        cached,
        started,
    );
    Ok(Json(body))
}

/// `GET /currency/providers`
#[utoipa::path(get, path = "/currency/providers", context_path = "/currency",
    responses((status = 200, description = "Provider directory")))]
async fn providers(state: web::Data<AppState>) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let items = fetch_providers_cached(&state).await?;
    let mut sorted: Vec<Value> = items.as_array().cloned().unwrap_or_default();
    sorted.sort_by(|a, b| {
        let ka = a.get("key").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("key").and_then(|v| v.as_str()).unwrap_or("");
        ka.cmp(kb)
    });
    let total = sorted.len();
    let body = envelope_json(
        json!({ "items": sorted, "total": total, "default": DEFAULT_PROVIDER }),
        DEFAULT_PROVIDER,
        false,
        started,
    );
    Ok(Json(body))
}

// ------------------------------------------------------------------ health

async fn health_report(state: &AppState) -> Value {
    let probe_key = "currency_probe";
    let probe_cache = state.json_cache(cache_names::HEALTH_PROBES);
    let cached = probe_cache.get(probe_key).await;
    let (probe, was_cached) = match cached {
        Some(v) => (v.as_ref().clone(), true),
        None => {
            let report: Value = match resolve_probe(state).await {
                Ok(p) => p,
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
        "upstream": {
            "reachable": reachable,
            "provider": DEFAULT_PROVIDER,
            "base": DEFAULT_BASE,
            "asOf": probe.get("asOf"),
            "rate": probe.get("rate"),
            "error": probe.get("error"),
            "cached": was_cached,
        },
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
}

async fn resolve_probe(state: &AppState) -> Result<Value, ApiError> {
    let payload = ff_get(state, &format!("/v2/rate/{DEFAULT_BASE}/USD")).await?;
    let row = parse_rate_row(&payload)?;
    Ok(json!({
        "reachable": true,
        "asOf": row.get("date"),
        "rate": row.get("rate").and_then(|v| v.as_f64()).map(|r| round_to(r, RATE_DECIMALS)),
    }))
}

/// `GET /currency/health`
#[utoipa::path(get, path = "/currency/health", context_path = "/currency",
    responses((status = 200, description = "Liveness plus upstream probe")))]
async fn health(state: web::Data<AppState>) -> Json<Value> {
    Json(health_report(&state).await)
}

/// `GET /currency/health/ready`
#[utoipa::path(get, path = "/currency/health/ready", context_path = "/currency",
    responses((status = 200, description = "Upstream reachable"), (status = 503, description = "Upstream down")))]
async fn health_ready(state: web::Data<AppState>) -> Result<Json<Value>, ApiError> {
    let report = health_report(&state).await;
    if report.get("upstream").and_then(|u| u.get("reachable")).and_then(|v| v.as_bool()) != Some(true) {
        return Err(ApiError::unavailable(
            format!("{CODE_PREFIX}_UPSTREAM_UNAVAILABLE"),
            "The exchange rate upstream is unreachable".to_string(),
        ));
    }
    Ok(Json(json!({
        "status": "ready",
        "asOf": report.pointer("/upstream/asOf"),
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })))
}

// ---------------------------------------------------------------- OpenAPI

#[derive(OpenApi)]
#[openapi(
    info(title = "currency-api", version = "1.0.0", description = "Central bank reference exchange rates without an API key, plus amount conversion."),
    paths(latest_rates, publication_dates, rate_series, convert, currencies, currency_by_code, providers, health, health_ready)
)]
pub struct CurrencyApi;

pub fn openapi_doc() -> utoipa::openapi::OpenApi {
    let mut doc = CurrencyApi::openapi();
    doc.info.title = "currency-api".into();
    doc.servers = Some(vec![utoipa::openapi::Server::new(SERVICE_PREFIX)]);
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rate_rows() {
        let payload = json!([
            { "date": "2026-10-20", "base": "USD", "quote": "EUR", "rate": 0.89077 },
            { "garbage": true }
        ]);
        let rows = parse_rate_rows(&payload).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["quote"], "EUR");
    }

    #[test]
    fn parses_currencies_and_providers() {
        let cur = parse_currencies(&json!([{ "iso_code": "USD", "iso_numeric": "840", "name": "United States Dollar", "symbol": "$", "start_date": "1948-06-21", "end_date": "2026-10-20" }])).unwrap();
        assert_eq!(cur[0]["code"], "USD");
        assert_eq!(cur[0]["numeric"], "840");
        let prov = parse_providers(&json!([{ "key": "ECB", "name": "European Central Bank", "country_code": "EU", "terms_url": null }])).unwrap();
        assert_eq!(prov[0]["key"], "ECB");
        assert_eq!(prov[0]["countryCode"], "EU");
        assert!(prov[0].get("termsUrl").is_none());
    }

    #[test]
    fn window_resolution() {
        let (s, e) = resolve_window(&Some("2026-01-01".into()), &Some("2026-01-31".into())).unwrap();
        assert_eq!(s, "2026-01-01");
        assert_eq!(e, "2026-01-31");
        assert!(resolve_window(&Some("2026-01-31".into()), &Some("2026-01-01".into())).is_err());
        assert!(resolve_window(&Some("not-a-date".into()), &None).is_err());
    }

    #[test]
    fn pagination_clamps() {
        assert_eq!(pagination(None, None), (DEFAULT_LIMIT, 0));
        assert_eq!(pagination(Some(0), None), (1, 0));
        assert_eq!(pagination(Some(99_999), Some(5)), (MAX_LIMIT, 5));
    }
}
