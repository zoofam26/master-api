//! Resilient upstream helpers: GET/POST with timeouts, bounded retries with
//! exponential backoff + full jitter, response timing, and a cached-JSON
//! helper that adds single-flight deduplication on top of the cache registry.

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use crate::error::ApiError;
use crate::state::AppState;

/// A fetched JSON body plus how long the upstream call took.
#[derive(Debug, Clone)]
pub struct Timed {
    pub body: Value,
    pub duration_ms: u64,
}

fn backoff_delay(base_ms: u64, attempt: u32) -> std::time::Duration {
    // Full jitter: uniform in [0, base * 2^attempt].
    let max = base_ms.saturating_mul(1u64 << attempt.min(6));
    let jitter = rand::random::<u64>() % (max + 1).max(1);
    std::time::Duration::from_millis(jitter)
}

fn is_retryable(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// Perform a GET and parse the body as JSON, with retries on transport errors,
/// 429 and 5xx. `query` entries are appended as-is; use empty strings for
/// flag-style params. Returns the body and elapsed milliseconds.
pub async fn get_json(
    state: &AppState,
    url: &str,
    query: &[(&str, &str)],
    code_prefix: &str,
) -> Result<Timed, ApiError> {
    get_json_with_headers(state, url, query, &[], code_prefix).await
}

/// [`get_json`] with extra request headers (User-Agent, Accept-Language, ...).
pub async fn get_json_with_headers(
    state: &AppState,
    url: &str,
    query: &[(&str, &str)],
    headers: &[(&str, &str)],
    code_prefix: &str,
) -> Result<Timed, ApiError> {
    let started = Instant::now();
    let attempts = state.settings.upstream_retries + 1;
    let mut last_err: Option<ApiError> = None;

    for attempt in 0..attempts {
        if attempt > 0 {
            tokio::time::sleep(backoff_delay(state.settings.retry_base_delay_ms, attempt - 1)).await;
        }

        let mut req = state.http.get(url).query(query);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match resp.json::<Value>().await {
                        Ok(body) => {
                            return Ok(Timed { body, duration_ms: started.elapsed().as_millis() as u64 });
                        }
                        Err(e) => {
                            last_err = Some(ApiError::upstream(
                                format!("{code_prefix}_UPSTREAM"),
                                format!("Upstream returned an unparseable body: {e}"),
                            ));
                        }
                    }
                } else {
                    let code = status.as_u16();
                    let text = resp.text().await.unwrap_or_default();
                    let err = ApiError::from_upstream_status(code, code_prefix, &text);
                    if is_retryable(code) && attempt + 1 < attempts {
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            }
            Err(e) => {
                let err = if e.is_timeout() {
                    ApiError::timeout(
                        format!("{code_prefix}_UPSTREAM_TIMEOUT"),
                        format!("Upstream did not respond within {}s", state.settings.upstream_timeout.as_secs()),
                    )
                } else {
                    ApiError::upstream(
                        format!("{code_prefix}_UNREACHABLE"),
                        format!("Could not reach upstream: {e}"),
                    )
                };
                if attempt + 1 < attempts {
                    last_err = Some(err);
                    continue;
                }
                return Err(err);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| ApiError::upstream(format!("{code_prefix}_UPSTREAM"), "Upstream request failed")))
}

/// Perform a POST with a JSON body and parse the response as JSON.
pub async fn post_json(
    state: &AppState,
    url: &str,
    body: &Value,
    headers: &[(&str, &str)],
    code_prefix: &str,
) -> Result<Timed, ApiError> {
    let started = Instant::now();
    let attempts = state.settings.upstream_retries + 1;
    let mut last_err: Option<ApiError> = None;

    for attempt in 0..attempts {
        if attempt > 0 {
            tokio::time::sleep(backoff_delay(state.settings.retry_base_delay_ms, attempt - 1)).await;
        }
        let mut req = state.http.post(url).json(body);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match resp.json::<Value>().await {
                        Ok(b) => {
                            return Ok(Timed { body: b, duration_ms: started.elapsed().as_millis() as u64 });
                        }
                        Err(e) => {
                            last_err = Some(ApiError::upstream(
                                format!("{code_prefix}_UPSTREAM"),
                                format!("Upstream returned an unparseable body: {e}"),
                            ));
                        }
                    }
                } else {
                    let code = status.as_u16();
                    let text = resp.text().await.unwrap_or_default();
                    let err = ApiError::from_upstream_status(code, code_prefix, &text);
                    if is_retryable(code) && attempt + 1 < attempts {
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            }
            Err(e) => {
                let err = if e.is_timeout() {
                    ApiError::timeout(
                        format!("{code_prefix}_UPSTREAM_TIMEOUT"),
                        "Upstream did not respond in time".to_string(),
                    )
                } else {
                    ApiError::upstream(
                        format!("{code_prefix}_UNREACHABLE"),
                        format!("Could not reach upstream: {e}"),
                    )
                };
                if attempt + 1 < attempts {
                    last_err = Some(err);
                    continue;
                }
                return Err(err);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| ApiError::upstream(format!("{code_prefix}_UPSTREAM"), "Upstream request failed")))
}

/// Perform a GET and parse the body as JSON, treating 204 / empty bodies as
/// `None` (some upstreams answer "no data" with 204 instead of `[]`).
pub async fn get_json_opt(
    state: &AppState,
    url: &str,
    query: &[(&str, &str)],
    code_prefix: &str,
) -> Result<Option<Value>, ApiError> {
    let started = Instant::now();
    let attempts = state.settings.upstream_retries + 1;
    let mut last_err: Option<ApiError> = None;
    for attempt in 0..attempts {
        if attempt > 0 {
            tokio::time::sleep(backoff_delay(state.settings.retry_base_delay_ms, attempt - 1)).await;
        }
        match state.http.get(url).query(query).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status == reqwest::StatusCode::NO_CONTENT {
                    return Ok(None);
                }
                if status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    if body.trim().is_empty() {
                        return Ok(None);
                    }
                    match serde_json::from_str::<Value>(&body) {
                        Ok(v) => {
                            let _ = started;
                            return Ok(Some(v));
                        }
                        Err(e) => {
                            last_err = Some(ApiError::upstream(
                                format!("{code_prefix}_PARSE_ERROR"),
                                format!("Upstream returned an unparseable body: {e}"),
                            ));
                        }
                    }
                } else {
                    let code = status.as_u16();
                    let text = resp.text().await.unwrap_or_default();
                    let err = ApiError::from_upstream_status(code, code_prefix, &text);
                    if is_retryable(code) && attempt + 1 < attempts {
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            }
            Err(e) => {
                let err = if e.is_timeout() {
                    ApiError::timeout(
                        format!("{code_prefix}_UPSTREAM_TIMEOUT"),
                        "Upstream did not respond in time".to_string(),
                    )
                } else {
                    ApiError::upstream(
                        format!("{code_prefix}_UNREACHABLE"),
                        format!("Could not reach upstream: {e}"),
                    )
                };
                if attempt + 1 < attempts {
                    last_err = Some(err);
                    continue;
                }
                return Err(err);
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| ApiError::upstream(format!("{code_prefix}_UPSTREAM"), "Upstream request failed")))
}

/// Download a raw binary payload (e.g. ZIP feeds), with retries.
pub async fn get_bytes(
    state: &AppState,
    url: &str,
    code_prefix: &str,
) -> Result<(bytes::Bytes, u64), ApiError> {
    let started = Instant::now();
    let attempts = state.settings.upstream_retries + 1;
    let mut last_err: Option<ApiError> = None;
    for attempt in 0..attempts {
        if attempt > 0 {
            tokio::time::sleep(backoff_delay(state.settings.retry_base_delay_ms, attempt - 1)).await;
        }
        match state.http.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match resp.bytes().await {
                        Ok(b) => return Ok((b, started.elapsed().as_millis() as u64)),
                        Err(e) => {
                            last_err = Some(ApiError::upstream(
                                format!("{code_prefix}_UPSTREAM"),
                                format!("Failed reading upstream body: {e}"),
                            ));
                        }
                    }
                } else {
                    let code = status.as_u16();
                    let err = ApiError::from_upstream_status(code, code_prefix, "");
                    if is_retryable(code) && attempt + 1 < attempts {
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            }
            Err(e) => {
                let err = if e.is_timeout() {
                    ApiError::timeout(
                        format!("{code_prefix}_UPSTREAM_TIMEOUT"),
                        "Upstream did not respond in time".to_string(),
                    )
                } else {
                    ApiError::upstream(
                        format!("{code_prefix}_UNREACHABLE"),
                        format!("Could not reach upstream: {e}"),
                    )
                };
                if attempt + 1 < attempts {
                    last_err = Some(err);
                    continue;
                }
                return Err(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| ApiError::upstream(format!("{code_prefix}_UPSTREAM"), "Upstream request failed")))
}

/// Result of [`cached_json`].
pub struct CachedJson {
    pub body: Arc<Value>,
    pub cached: bool,
    pub duration_ms: u64,
}

/// Fetch JSON through a named cache with single-flight: on a cache miss the
/// loader runs at most once even under concurrent identical requests; the
/// error is **not** cached. On a hit, `duration_ms` is 0 and `cached` is true.
pub async fn cached_json<F, Fut>(
    state: &AppState,
    cache_name: &str,
    key: &str,
    load: F,
) -> Result<CachedJson, ApiError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Value, ApiError>>,
{
    let cache = state.json_cache(cache_name);
    let started = Instant::now();
    let key_owned = key.to_string();
    let value = cache
        .try_get_with(key_owned, async move {
            let loaded = load().await?;
            Ok(Arc::new(loaded))
        })
        .await
        .map_err(|e: Arc<ApiError>| ApiError::internal(format!("{e}")))?;
    // `try_get_with` shares the loader across waiters; distinguish a real
    // upstream call (elapsed > 0) from a cache hit.
    let elapsed = started.elapsed().as_millis() as u64;
    Ok(CachedJson {
        body: value,
        cached: elapsed == 0,
        duration_ms: elapsed,
    })
}

/// [`post_json`] with owned header values (dynamic client versions etc.).
pub async fn post_json_owned(
    state: &AppState,
    url: &str,
    body: &Value,
    headers: &[(String, String)],
    code_prefix: &str,
) -> Result<Timed, ApiError> {
    let refs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    post_json(state, url, body, &refs, code_prefix).await
}
