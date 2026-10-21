//! Route handlers for the books service.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use actix_web::web::{self, Json};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use utoipa::{IntoParams, OpenApi};

use master_common::envelope::Envelope;
use master_common::error::ApiError;
use master_common::httpx;
use master_common::state::{cache_names, AppState};

use crate::isbn::parse_isbn;
use crate::olparse::{
    compose_query, is_author_key, is_valid_sort, is_work_key, parse_author_summary, parse_edition,
    parse_search_response, parse_subject_response, parse_work_detail, parse_ol_id,
};
use crate::{CODE_PREFIX, SERVICE_PREFIX, UPSTREAM_NAME, UPSTREAM_OPENLIBRARY};

const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 100;
const MAX_AUTHORS: usize = 10;
/// Open Library's guideline: identify yourself and stay well under their limits.
const MIN_INTERVAL_MS: u64 = 1_100;

static NEXT_SLOT_MS: AtomicU64 = AtomicU64::new(0);
static OL_GATE: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Serialises upstream starts with a minimum spacing, honouring Open Library's
/// published rate guidance without needing to be asked.
async fn ol_throttle() {
    let _guard = OL_GATE.lock().await;
    let next = NEXT_SLOT_MS.load(Ordering::Relaxed);
    let now = now_ms();
    if now < next {
        tokio::time::sleep(Duration::from_millis(next - now)).await;
    }
    NEXT_SLOT_MS.store(now_ms() + MIN_INTERVAL_MS, Ordering::Relaxed);
}

/// Percent-encode a URL path/query component (RFC 3986 unreserved set kept).
pub fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn require_key(raw: &str, valid: fn(&str) -> bool, label: &str, example: &str) -> Result<String, ApiError> {
    let trimmed = raw.trim();
    if valid(trimmed) {
        return Ok(trimmed.to_string());
    }
    Err(ApiError::validation_details(
        format!("{CODE_PREFIX}_INVALID_REQUEST"),
        format!("A {label} looks like \"{example}\"; \"{trimmed}\" is not one"),
        json!({ "received": trimmed, "example": example }),
    ))
}

fn require_page_size(limit: Option<usize>) -> Result<usize, ApiError> {
    match limit {
        None => Ok(DEFAULT_LIMIT),
        Some(v) if (1..=MAX_LIMIT).contains(&v) => Ok(v),
        Some(v) => Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_LIMIT"),
            format!("limit must be an integer between 1 and {MAX_LIMIT}"),
            json!({ "limit": v, "max": MAX_LIMIT }),
        )),
    }
}

fn require_offset(offset: Option<usize>) -> Result<usize, ApiError> {
    match offset {
        None => Ok(0),
        Some(v) if v <= 1_000_000 => Ok(v),
        Some(v) => Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_OFFSET"),
            "offset must not exceed 1000000".to_string(),
            json!({ "offset": v }),
        )),
    }
}

fn page_info(limit: usize, offset: usize, returned: usize, total: Option<i64>) -> Value {
    let known = total.is_some();
    let more = known && returned >= limit && ((offset as i64) + returned as i64) < total.unwrap();
    let mut out = json!({ "limit": limit, "offset": offset, "returned": returned });
    if known {
        out["total"] = json!(total.unwrap());
        out["hasMore"] = json!(more);
    }
    if more {
        out["nextOffset"] = json!(offset + limit);
    }
    out
}

fn meta_json(cached: bool, duration_ms: u64, sources: Vec<String>) -> Value {
    json!({
        "durationMs": duration_ms,
        "cached": cached,
        "upstream": UPSTREAM_NAME,
        "sources": sources,
    })
}

/// Authenticated GET against Open Library (serialised, JSON), using the shared
/// resilient client with its retries/timeouts.
async fn ol_get_json(state: &AppState, url: &str, endpoint: &str) -> Result<Value, ApiError> {
    ol_throttle().await;
    let headers = [("user-agent", "master-api book-api/1.0 (+https://github.com/zoofam26/master-api)")];
    let _ = endpoint;
    httpx::get_json_with_headers(state, url, &[], &headers, CODE_PREFIX)
        .await
        .map(|t| t.body)
}

// ------------------------------------------------------------- query DTOs

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BookSearchQuery {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// Free-text query.
    pub q: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    /// Solr subject clause merged into q.
    pub subject: Option<String>,
    /// Three-letter MARC code, e.g. `eng`.
    pub language: Option<String>,
    pub publisher: Option<String>,
    /// Strict `true`/`false`.
    #[serde(rename = "hasFulltext")]
    pub has_fulltext: Option<bool>,
    /// One of: relevance, new, old, title, rating, key.
    #[serde(rename = "sort")]
    pub sort: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SubjectQuery {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// The subject name, e.g. `science_fiction`.
    pub q: Option<String>,
    /// Alias for `q`.
    pub name: Option<String>,
    pub sort: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CountryQuery {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// Case-insensitive substring match on the common or official name.
    pub q: Option<String>,
    /// Case-insensitive substring match on the region.
    pub region: Option<String>,
    /// Strict `true`/`false`.
    #[serde(rename = "unMember")]
    pub un_member: Option<bool>,
    /// Strict `true`/`false`.
    pub independent: Option<bool>,
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    let scope = web::scope(SERVICE_PREFIX)
        // Literals first so they are never swallowed by "books/{key}".
        .route("/books/search", web::get().to(search))
        .route("/books/isbn/{isbn}", web::get().to(by_isbn))
        .route("/books/{key}/authors", web::get().to(work_authors))
        .route("/authors/{authorKey}", web::get().to(author))
        .route("/subjects", web::get().to(subjects))
        .route("/countries", web::get().to(countries))
        .route("/health/ready", web::get().to(health_ready))
        .route("/health", web::get().to(health))
        .route("/books/{key}", web::get().to(work));
    cfg.service(scope);
}

// ---------------------------------------------------------------- handlers

/// `GET /books/books/search`
#[utoipa::path(get, path = "/books/books/search", context_path = "/books", params(BookSearchQuery),
    responses((status = 200, description = "Search results")))]
async fn search(
    state: web::Data<AppState>,
    query: web::Query<BookSearchQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    if query.q.as_deref().map(str::trim).filter(|s| !s.is_empty()).is_none()
        && query.title.as_deref().map(str::trim).filter(|s| !s.is_empty()).is_none()
        && query.author.as_deref().map(str::trim).filter(|s| !s.is_empty()).is_none()
        && query.subject.as_deref().map(str::trim).filter(|s| !s.is_empty()).is_none()
    {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "Supply at least one of q, title, author or subject: an unfiltered Open Library search is an expensive full-index query",
            json!({ "accepted": ["q", "title", "author", "subject"] }),
        ));
    }
    for (name, value, max) in [
        ("q", &query.q, 512usize),
        ("title", &query.title, 512),
        ("author", &query.author, 512),
        ("subject", &query.subject, 256),
        ("language", &query.language, 16),
        ("publisher", &query.publisher, 256),
    ] {
        if let Some(v) = value {
            if v.len() > max {
                return Err(ApiError::validation_details(
                    format!("{CODE_PREFIX}_INVALID_REQUEST"),
                    format!("{name} must be at most {max} characters"),
                    json!({ "parameter": name, "maxLength": max }),
                ));
            }
        }
    }
    let sort = match query.sort.as_deref() {
        None => "relevance".to_string(),
        Some(s) if is_valid_sort(s) => s.to_string(),
        Some(s) => {
            return Err(ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_REQUEST"),
                format!("Unknown sort facet \"{s}\""),
                json!({ "sort": s, "allowed": crate::olparse::SEARCH_SORTS }),
            ));
        }
    };
    let limit = require_page_size(query.limit)?;
    let offset = require_offset(query.offset)?;

    let q = compose_query(query.q.as_deref(), query.subject.as_deref());
    // Canonical URL, used for both the cache key and the meta.sources entry.
    let mut url = format!("{UPSTREAM_OPENLIBRARY}/search.json?");
    let mut pairs: Vec<(&str, String)> = Vec::new();
    if let Some(q) = &q {
        pairs.push(("q", q.clone()));
    }
    if let Some(t) = query.title.as_deref().filter(|s| !s.trim().is_empty()) {
        pairs.push(("title", t.to_string()));
    }
    if let Some(a) = query.author.as_deref().filter(|s| !s.trim().is_empty()) {
        pairs.push(("author", a.to_string()));
    }
    if let Some(p) = query.publisher.as_deref().filter(|s| !s.trim().is_empty()) {
        pairs.push(("publisher", p.to_string()));
    }
    if let Some(l) = query.language.as_deref().filter(|s| !s.trim().is_empty()) {
        pairs.push(("language", l.to_lowercase()));
    }
    if let Some(h) = query.has_fulltext {
        pairs.push(("has_fulltext", h.to_string()));
    }
    let upstream_sort = match sort.as_str() {
        "relevance" => None,
        other => Some(other.to_string()),
    };
    if let Some(s) = upstream_sort {
        pairs.push(("sort", s));
    }
    pairs.push(("limit", limit.to_string()));
    pairs.push(("offset", offset.to_string()));
    pairs.push((
        "fields",
        "key,title,subtitle,author_name,author_key,first_publish_year,edition_count,has_fulltext,cover_i,language,ratings_average,ratings_count".to_string(),
    ));
    url.push_str(
        &pairs
            .iter()
            .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
            .collect::<Vec<_>>()
            .join("&"),
    );

    let cache = state.json_cache(cache_names::BOOKS);
    let cached_hit = cache.get(&url).await;
    let (parsed, cached) = match cached_hit {
        Some(v) => (v, true),
        None => {
            let raw = ol_get_json(&state, &url, "search.json").await?;
            let (total, off, works) =
                parse_search_response(&raw, offset, &sort).map_err(|m| ApiError::upstream(format!("{CODE_PREFIX}_PARSE_ERROR"), m))?;
            let v = json!({ "total": total, "offset": off, "works": works });
            cache.insert(url.clone(), std::sync::Arc::new(v.clone())).await;
            (std::sync::Arc::new(v), false)
        }
    };

    let works = parsed
        .get("works")
        .cloned()
        .unwrap_or_else(|| Value::Array(vec![]));
    let returned = works.as_array().map(|a| a.len()).unwrap_or(0);
    let total = parsed.get("total").and_then(|v| v.as_i64());

    let mut query_echo = Map::new();
    for (key, val) in [
        ("q", &query.q),
        ("title", &query.title),
        ("author", &query.author),
        ("subject", &query.subject),
        ("publisher", &query.publisher),
    ] {
        if let Some(v) = val {
            query_echo.insert(key.to_string(), json!(v));
        }
    }
    if let Some(l) = &query.language {
        query_echo.insert("language".into(), json!(l.to_lowercase()));
    }
    if let Some(h) = query.has_fulltext {
        query_echo.insert("hasFulltext".into(), json!(h));
    }
    query_echo.insert("sort".into(), json!(sort));
    query_echo.insert("tiebreaker".into(), json!("work key ascending"));

    let body = Envelope::ok(
        json!({
            "query": Value::Object(query_echo),
            "pageInfo": page_info(limit, offset, returned, total),
            "works": works,
        }),
        meta_json(cached, started.elapsed().as_millis() as u64, vec![url]),
    );
    Ok(Json(body.to_json()))
}

/// `GET /books/books/{key}`
#[utoipa::path(get, path = "/books/books/{key}", context_path = "/books",
    params(("key" = String, Path, description = "Open Library work key, e.g. OL45804W")),
    responses((status = 200, description = "Work detail"), (status = 404, description = "Unknown work")))]
async fn work(state: web::Data<AppState>, path: web::Path<String>) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let work_key = require_key(&path.into_inner(), is_work_key, "work key", "OL45804W")?;
    let url = format!("{UPSTREAM_OPENLIBRARY}/works/{work_key}.json");

    let cache = state.json_cache(cache_names::BOOKS);
    let cache_key = format!("work:{work_key}");
    let cached_hit = cache.get(&cache_key).await;
    let (detail, cached) = match cached_hit {
        Some(v) => (v, true),
        None => {
            let raw = ol_get_json(&state, &url, &format!("works/{work_key}")).await?;
            let parsed = parse_work_detail(&raw);
            if parsed.get("key").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                return Err(ApiError::not_found_details(
                    format!("{CODE_PREFIX}_NOT_FOUND"),
                    format!("No Open Library work matches {work_key}"),
                    json!({ "key": work_key }),
                ));
            }
            cache.insert(cache_key.clone(), std::sync::Arc::new(parsed.clone())).await;
            (std::sync::Arc::new(parsed), false)
        }
    };

    let body = Envelope::ok(
        detail.as_ref().clone(),
        meta_json(cached, started.elapsed().as_millis() as u64, vec![url]),
    );
    Ok(Json(body.to_json()))
}

/// `GET /books/books/isbn/{isbn}`
#[utoipa::path(get, path = "/books/books/isbn/{isbn}", context_path = "/books",
    params(("isbn" = String, Path, description = "ISBN-10 or ISBN-13, hyphens allowed")),
    responses((status = 200, description = "Edition lookup"), (status = 400, description = "Invalid ISBN"), (status = 404, description = "Unknown edition")))]
async fn by_isbn(state: web::Data<AppState>, path: web::Path<String>) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let raw = path.into_inner();
    let isbn = parse_isbn(&raw)?;
    let url = format!("{UPSTREAM_OPENLIBRARY}/isbn/{}.json", enc(&isbn.isbn13));

    let cache = state.json_cache(cache_names::BOOKS);
    let cache_key = format!("isbn:{}", isbn.isbn13);
    let cached_hit = cache.get(&cache_key).await;
    let (mut lookup, cached) = match cached_hit {
        Some(v) => (v.as_ref().clone(), true),
        None => {
            let edition_raw = ol_get_json(&state, &url, &format!("isbn/{}", isbn.value)).await?;
            let edition = parse_edition(&edition_raw);
            if edition.get("key").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                return Err(ApiError::not_found_details(
                    format!("{CODE_PREFIX}_NOT_FOUND"),
                    format!("Open Library has no edition for ISBN {}", isbn.value),
                    json!({ "isbn": isbn.value }),
                ));
            }
            // Enrich with the work + author names; failures must not break the lookup.
            let mut work: Option<Value> = None;
            let mut authors: Vec<Value> = Vec::new();
            if let Some(work_key) = edition.get("workKey").and_then(|v| v.as_str()).map(String::from) {
                match load_work_summary(&state, &work_key).await {
                    Ok((summary, refs)) => {
                        work = Some(summary);
                        authors = resolve_author_names(&state, refs).await;
                    }
                    Err(e) => {
                        tracing::warn!(isbn = %isbn.value, error = %e, "could not enrich ISBN with work");
                    }
                }
            }
            let mut v = json!({ "isbn": Value::Null, "edition": edition });
            if let Some(w) = work {
                v["work"] = w;
            }
            v["authors"] = Value::Array(authors);
            cache.insert(cache_key.clone(), std::sync::Arc::new(v.clone())).await;
            (v, false)
        }
    };

    // The identity block is rebuilt from *this* request, not the cache, so a
    // hyphenated ISBN-10 request reports the form the caller sent.
    lookup["isbn"] = json!({
        "input": isbn.input,
        "value": isbn.value,
        "type": if isbn.is_isbn13 { "isbn13" } else { "isbn10" },
        "isbn10": if isbn.isbn10.is_empty() { Value::Null } else { json!(isbn.isbn10) },
        "isbn13": isbn.isbn13,
        "checkDigit": isbn.check_digit,
        "group": isbn.group,
        "languageGroup": if isbn.language_group.is_empty() { Value::Null } else { json!(isbn.language_group) },
    });

    let body = Envelope::ok(
        lookup,
        meta_json(cached, started.elapsed().as_millis() as u64, vec![url]),
    );
    Ok(Json(body.to_json()))
}

/// `GET /books/books/{key}/authors`
#[utoipa::path(get, path = "/books/books/{key}/authors", context_path = "/books",
    params(("key" = String, Path)),
    responses((status = 200, description = "Authors of a work")))]
async fn work_authors(state: web::Data<AppState>, path: web::Path<String>) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let work_key = require_key(&path.into_inner(), is_work_key, "work key", "OL45804W")?;
    let url = format!("{UPSTREAM_OPENLIBRARY}/works/{work_key}.json");

    let (detail, cached) = load_work_summary(&state, &work_key)
        .await
        .map(|(v, refs)| (v, refs))?;
    let authors = resolve_author_names(&state, cached).await;
    let resolved = authors.iter().filter(|a| a.get("name").is_some()).count();
    let key = detail.get("key").cloned().unwrap_or(Value::Null);

    let body = Envelope::ok(
        json!({ "work": key, "authors": authors, "resolved": resolved }),
        meta_json(false, started.elapsed().as_millis() as u64, vec![url]),
    );
    Ok(Json(body.to_json()))
}

/// `GET /books/authors/{authorKey}`
#[utoipa::path(get, path = "/books/authors/{authorKey}", context_path = "/books",
    params(("authorKey" = String, Path)),
    responses((status = 200, description = "Author summary"), (status = 404, description = "Unknown author")))]
async fn author(state: web::Data<AppState>, path: web::Path<String>) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let author_key = require_key(&path.into_inner(), is_author_key, "author key", "OL26320A")?;
    let url = format!("{UPSTREAM_OPENLIBRARY}/authors/{author_key}.json");

    let cache = state.json_cache(cache_names::BOOKS);
    let cache_key = format!("author:{author_key}");
    let cached_hit = cache.get(&cache_key).await;
    let (summary, cached) = match cached_hit {
        Some(v) => (v, true),
        None => {
            let raw = ol_get_json(&state, &url, &format!("authors/{author_key}")).await?;
            let parsed = parse_author_summary(&raw);
            if parsed.get("key").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                return Err(ApiError::not_found_details(
                    format!("{CODE_PREFIX}_NOT_FOUND"),
                    format!("No Open Library author matches {author_key}"),
                    json!({ "key": author_key }),
                ));
            }
            cache.insert(cache_key.clone(), std::sync::Arc::new(parsed.clone())).await;
            (std::sync::Arc::new(parsed), false)
        }
    };

    let body = Envelope::ok(
        summary.as_ref().clone(),
        meta_json(cached, started.elapsed().as_millis() as u64, vec![url]),
    );
    Ok(Json(body.to_json()))
}

/// `GET /books/subjects`
#[utoipa::path(get, path = "/books/subjects", context_path = "/books", params(SubjectQuery),
    responses((status = 200, description = "Works under a subject")))]
async fn subjects(
    state: web::Data<AppState>,
    query: web::Query<SubjectQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let name = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| query.name.as_deref().map(str::trim).filter(|s| !s.is_empty()));
    let name = match name {
        Some(n) => n.to_string(),
        None => {
            return Err(ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_REQUEST"),
                "A subject name is required",
                json!({ "parameter": "q", "example": "/books/subjects?q=science_fiction" }),
            ));
        }
    };
    let sort = match query.sort.as_deref() {
        None => "relevance".to_string(),
        Some(s) if is_valid_sort(s) => s.to_string(),
        Some(s) => {
            return Err(ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_REQUEST"),
                format!("Unknown sort facet \"{s}\""),
                json!({ "sort": s, "allowed": crate::olparse::SEARCH_SORTS }),
            ));
        }
    };
    let limit = require_page_size(query.limit)?;
    let offset = require_offset(query.offset)?;
    let upstream_sort = if sort == "relevance" { None } else { Some(sort.clone()) };

    let mut url = format!("{UPSTREAM_OPENLIBRARY}/subjects/{}.json?", enc(&name));
    let mut pairs: Vec<(&str, String)> = vec![
        ("limit", limit.to_string()),
        ("offset", offset.to_string()),
    ];
    if let Some(s) = upstream_sort {
        pairs.push(("sort", s));
    }
    url.push_str(
        &pairs
            .iter()
            .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
            .collect::<Vec<_>>()
            .join("&"),
    );

    let cache = state.json_cache(cache_names::BOOKS);
    let cached_hit = cache.get(&url).await;
    let (browse, cached) = match cached_hit {
        Some(v) => (v, true),
        None => {
            let raw = ol_get_json(&state, &url, &format!("subjects/{name}")).await?;
            let parsed = parse_subject_response(&raw, &sort)
                .map_err(|m| ApiError::upstream(format!("{CODE_PREFIX}_PARSE_ERROR"), m))?;
            cache.insert(url.clone(), std::sync::Arc::new(parsed.clone())).await;
            (std::sync::Arc::new(parsed), false)
        }
    };

    let works = browse.get("works").cloned().unwrap_or(Value::Array(vec![]));
    let returned = works.as_array().map(|a| a.len()).unwrap_or(0);
    let work_count = browse.get("workCount").and_then(|v| v.as_i64());
    let mut data = browse.as_ref().clone();
    data["sort"] = json!(sort);
    data["pageInfo"] = page_info(limit, offset, returned, work_count);

    let body = Envelope::ok(
        data,
        meta_json(cached, started.elapsed().as_millis() as u64, vec![url]),
    );
    Ok(Json(body.to_json()))
}

/// `GET /books/countries`
#[utoipa::path(get, path = "/books/countries", context_path = "/books", params(CountryQuery),
    responses((status = 200, description = "Country reference listing")))]
async fn countries(
    state: web::Data<AppState>,
    query: web::Query<CountryQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let url = "embedded:svc-countries::dataset_raw_public".to_string();

    let cache = state.json_cache(cache_names::BOOKS);
    let cached_hit = cache.get("countries").await;
    let (all, cached) = match cached_hit {
        Some(v) => (v, true),
        None => {
            let raw = svc_countries::dataset_raw_public();
            let parsed = parse_countries(&raw)?;
            cache
                .insert("countries".to_string(), std::sync::Arc::new(parsed.clone()))
                .await;
            (std::sync::Arc::new(parsed), false)
        }
    };

    // Filter.
    let needle = query.q.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    let region = query.region.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    let matched: Vec<Value> = all
        .as_array()
        .map(|a| a.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|c| {
            if let Some(un) = query.un_member {
                if c.get("unMember").and_then(|v| v.as_bool()).unwrap_or(false) != un {
                    return false;
                }
            }
            if let Some(ind) = query.independent {
                if c.get("independent").and_then(|v| v.as_bool()).unwrap_or(false) != ind {
                    return false;
                }
            }
            if let Some(r) = &region {
                let hay = c.get("region").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                if !hay.contains(r) {
                    return false;
                }
            }
            if let Some(n) = &needle {
                let hay = format!(
                    "{} {} {} {}",
                    c.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                    c.get("officialName").and_then(|v| v.as_str()).unwrap_or(""),
                    c.get("cca2").and_then(|v| v.as_str()).unwrap_or(""),
                    c.get("cca3").and_then(|v| v.as_str()).unwrap_or("")
                )
                .to_lowercase();
                if !hay.contains(n) {
                    return false;
                }
            }
            true
        })
        .collect();

    let limit = require_page_size(query.limit)?;
    let offset = require_offset(query.offset)?;
    let total = matched.len() as i64;
    let page: Vec<Value> = matched
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect();

    let body = Envelope::ok(
        json!({
            "pageInfo": page_info(limit, offset, page.len(), Some(total)),
            "countries": page,
        }),
        meta_json(cached, started.elapsed().as_millis() as u64, vec![url]),
    );
    Ok(Json(body.to_json()))
}

// ---------------------------------------------------------------- helpers

async fn load_work_summary(state: &AppState, work_key: &str) -> Result<(Value, Vec<Value>), ApiError> {
    let url = format!("{UPSTREAM_OPENLIBRARY}/works/{work_key}.json");
    let cache = state.json_cache(cache_names::BOOKS);
    let cache_key = format!("work:{work_key}");
    let cached_hit = cache.get(&cache_key).await;
    let detail: Value = match cached_hit {
        Some(v) => v.as_ref().clone(),
        None => {
            let raw = ol_get_json(state, &url, &format!("works/{work_key}")).await?;
            let parsed = parse_work_detail(&raw);
            cache.insert(cache_key.clone(), std::sync::Arc::new(parsed.clone())).await;
            parsed
        }
    };
    if detail.get("key").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
        return Err(ApiError::not_found_details(
            format!("{CODE_PREFIX}_NOT_FOUND"),
            format!("No Open Library work matches {work_key}"),
            json!({ "key": work_key }),
        ));
    }
    let refs = detail
        .get("authors")
        .cloned()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    Ok((detail, refs))
}

/// Resolves author keys to names, bounded by `MAX_AUTHORS`, tolerating failures.
async fn resolve_author_names(state: &AppState, refs: Vec<Value>) -> Vec<Value> {
    let mut resolvable: Vec<(usize, String)> = Vec::new();
    for (idx, r) in refs.iter().enumerate() {
        let key = r.get("key").and_then(|v| v.as_str()).unwrap_or("");
        let has_name = r.get("name").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false);
        if !key.is_empty() && !has_name && resolvable.len() < MAX_AUTHORS {
            resolvable.push((idx, key.to_string()));
        }
    }
    if resolvable.is_empty() {
        return refs;
    }

    let cache = state.json_cache(cache_names::BOOKS);
    let mut by_index: std::collections::HashMap<usize, Value> = std::collections::HashMap::new();
    for (idx, key) in resolvable {
        let cache_key = format!("author:{key}");
        let summary = match cache.get(&cache_key).await {
            Some(v) => Some(v.as_ref().clone()),
            None => {
                let url = format!("{UPSTREAM_OPENLIBRARY}/authors/{key}.json");
                match ol_get_json(state, &url, &format!("authors/{key}")).await {
                    Ok(raw) => {
                        let parsed = parse_author_summary(&raw);
                        cache.insert(cache_key.clone(), std::sync::Arc::new(parsed.clone())).await;
                        Some(parsed)
                    }
                    Err(e) => {
                        tracing::warn!(author = %key, error = %e, "could not resolve author");
                        None
                    }
                }
            }
        };
        if let Some(s) = summary {
            by_index.insert(idx, s);
        }
    }

    refs.into_iter()
        .enumerate()
        .map(|(idx, r)| match by_index.get(&idx) {
            None => r,
            Some(summary) => {
                let mut merged = r.clone();
                if let Some(n) = summary.get("name") {
                    merged["name"] = n.clone();
                }
                if let Some(b) = summary.get("birthDate") {
                    if !b.is_null() {
                        merged["birthDate"] = b.clone();
                    }
                }
                if let Some(d) = summary.get("deathDate") {
                    if !d.is_null() {
                        merged["deathDate"] = d.clone();
                    }
                }
                if merged.get("photo").map(|p| p.is_null()).unwrap_or(true) {
                    if let Some(p) = summary.get("photo") {
                        merged["photo"] = p.clone();
                    }
                }
                merged
            }
        })
        .collect()
}

// ------------------------------------------------------------------ health

async fn health_report(state: &AppState) -> Value {
    let probe_key = "books_probe";
    let probe_cache = state.json_cache(cache_names::HEALTH_PROBES);
    let cached = probe_cache.get(probe_key).await;
    let (probe, was_cached) = match cached {
        Some(v) => (v.as_ref().clone(), true),
        None => {
            let started = std::time::Instant::now();
            let url = format!("{UPSTREAM_OPENLIBRARY}/works/OL45804W.json");
            let result = ol_get_json(state, &url, "health-probe").await;
            let report = match result {
                Ok(raw) => {
                    let record = parse_ol_id(raw.get("key")).unwrap_or_else(|| "OL45804W".to_string());
                    json!({
                        "reachable": true,
                        "latencyMs": started.elapsed().as_millis() as u64,
                        "record": record,
                    })
                }
                Err(e) => json!({
                    "reachable": false,
                    "latencyMs": 0,
                    "error": e.to_string(),
                }),
            };
            probe_cache.insert(probe_key.to_string(), std::sync::Arc::new(report.clone())).await;
            (report, false)
        }
    };

    let reachable = probe.get("reachable").and_then(|v| v.as_bool()).unwrap_or(false);
    json!({
        "status": if reachable { "ok" } else { "degraded" },
        "uptimeSeconds": state.uptime_seconds(),
        "upstream": { "name": UPSTREAM_NAME, "probe": probe, "cached": was_cached },
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
}

/// `GET /books/health`
#[utoipa::path(get, path = "/books/health", context_path = "/books",
    responses((status = 200, description = "Liveness plus upstream probe")))]
async fn health(state: web::Data<AppState>) -> Json<Value> {
    Json(health_report(&state).await)
}

/// `GET /books/health/ready`
#[utoipa::path(get, path = "/books/health/ready", context_path = "/books",
    responses((status = 200, description = "Upstream reachable"), (status = 503, description = "Upstream down")))]
async fn health_ready(state: web::Data<AppState>) -> Result<Json<Value>, ApiError> {
    let report = health_report(&state).await;
    if report.get("upstream").and_then(|u| u.get("reachable")).and_then(|v| v.as_bool()) != Some(true) {
        return Err(ApiError::unavailable(
            format!("{CODE_PREFIX}_UPSTREAM_UNAVAILABLE"),
            "The Open Library upstream is unreachable".to_string(),
        ));
    }
    Ok(Json(json!({
        "status": "ready",
        "upstream": UPSTREAM_NAME,
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })))
}

// -------------------------------------------------------------- countries

/// Projects the mledoze dataset onto the same country record the original
/// served from the `world-countries` CDN dataset.
fn parse_countries(raw: &Value) -> Result<Value, ApiError> {
    let entries = raw
        .as_array()
        .ok_or_else(|| ApiError::upstream(format!("{CODE_PREFIX}_PARSE_ERROR"), "Country dataset is not an array".to_string()))?;
    let mut countries: Vec<Value> = Vec::new();
    for entry in entries {
        let cca2 = crate::olparse::as_string(entry.get("cca2"));
        let cca3 = crate::olparse::as_string(entry.get("cca3"));
        let (Some(cca2), Some(cca3)) = (cca2, cca3) else { continue };
        let mut out = json!({
            "name": crate::olparse::as_string(entry.pointer("/name/common")).unwrap_or_else(|| cca3.clone()),
            "cca2": cca2.to_uppercase(),
            "cca3": cca3.to_uppercase(),
            "capital": crate::olparse::as_string_array(entry.get("capital")),
            "tld": crate::olparse::as_string_array(entry.get("tld")),
            "currencies": parse_currencies(entry.get("currencies")),
            "unMember": crate::olparse::as_bool(entry.get("unMember")).unwrap_or(false),
            "independent": crate::olparse::as_bool(entry.get("independent")).unwrap_or(false),
        });
        if let Some(o) = crate::olparse::as_string(entry.pointer("/name/official")) {
            out["officialName"] = json!(o);
        }
        if let Some(r) = crate::olparse::as_string(entry.get("region")) {
            out["region"] = json!(r);
        }
        if let Some(r) = crate::olparse::as_string(entry.get("subregion")) {
            out["subregion"] = json!(r);
        }
        if let Some(a) = crate::olparse::as_number(entry.get("area")) {
            out["areaKm2"] = json!(a);
        }
        if let Some(f) = crate::olparse::as_string(entry.get("flag")) {
            out["flag"] = json!(f);
        }
        countries.push(out);
    }
    if countries.is_empty() {
        return Err(ApiError::upstream(format!("{CODE_PREFIX}_PARSE_ERROR"), "Country dataset contained no usable entries".to_string()));
    }
    countries.sort_by(|a, b| {
        let ka = a.get("cca2").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("cca2").and_then(|v| v.as_str()).unwrap_or("");
        ka.cmp(kb)
    });
    Ok(Value::Array(countries))
}

fn parse_currencies(value: Option<&Value>) -> Vec<Value> {
    let mut out: Vec<Value> = match value {
        Some(Value::Object(map)) => map
            .iter()
            .map(|(code, entry)| {
                let mut c = json!({ "code": code });
                if let Some(n) = crate::olparse::as_string(entry.get("name")) {
                    c["name"] = json!(n);
                }
                if let Some(s) = crate::olparse::as_string(entry.get("symbol")) {
                    c["symbol"] = json!(s);
                }
                c
            })
            .collect(),
        _ => Vec::new(),
    };
    out.sort_by(|a, b| {
        let ka = a.get("code").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("code").and_then(|v| v.as_str()).unwrap_or("");
        ka.cmp(kb)
    });
    out
}

// ---------------------------------------------------------------- OpenAPI

#[derive(OpenApi)]
#[openapi(
    info(title = "book-api", version = "1.0.0", description = "Book search, work/author records, ISBN resolution and subject browsing over Open Library."),
    paths(search, work, by_isbn, work_authors, author, subjects, countries, health, health_ready)
)]
pub struct BooksApi;

pub fn openapi_doc() -> utoipa::openapi::OpenApi {
    let mut doc = BooksApi::openapi();
    doc.info.title = "book-api".into();
    doc.servers = Some(vec![utoipa::openapi::Server::new(SERVICE_PREFIX)]);
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_info_shapes() {
        let p = page_info(10, 0, 10, Some(25));
        assert_eq!(p["hasMore"], json!(true));
        assert_eq!(p["nextOffset"], json!(10));
        let p2 = page_info(10, 20, 5, Some(25));
        assert_eq!(p2["hasMore"], json!(false));
        assert!(p2.get("nextOffset").is_none());
        let p3 = page_info(10, 0, 3, None);
        assert!(p3.get("hasMore").is_none());
    }

    #[test]
    fn key_validation() {
        assert!(require_key("OL45804W", is_work_key, "work key", "OL45804W").is_ok());
        assert!(require_key("/works/OL45804W", is_work_key, "work key", "OL45804W").is_err());
        assert!(require_key("OL26320A", is_author_key, "author key", "OL26320A").is_ok());
        assert!(require_key("OL26320W", is_author_key, "author key", "OL26320A").is_err());
    }

    #[test]
    fn percent_encoding() {
        assert_eq!(enc("science fiction"), "science%20fiction");
        assert_eq!(enc("a/b?c=d"), "a%2Fb%3Fc%3Dd");
        assert_eq!(enc("safe-._~"), "safe-._~");
    }

    #[test]
    fn countries_projection_from_mledoze() {
        let raw = svc_countries::dataset_raw_public();
        let parsed = parse_countries(&raw).unwrap();
        let arr = parsed.as_array().unwrap();
        assert!(arr.len() >= 250);
        let de = arr
            .iter()
            .find(|c| c.get("cca2").and_then(|v| v.as_str()) == Some("DE"))
            .unwrap();
        assert_eq!(de["name"], "Germany");
        assert!(de["capital"].as_array().unwrap().iter().any(|c| c == "Berlin"));
    }
}
