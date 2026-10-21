//! Route handlers for the university service.

use actix_web::web::{self, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use utoipa::{IntoParams, OpenApi};

use master_common::envelope::Envelope;
use master_common::error::ApiError;
use master_common::state::AppState;

use crate::snapshot::{get_snapshot, UniversitySnapshot};
use crate::{CODE_PREFIX, SERVICE_PREFIX, SORT_KEYS, UPSTREAM_HIPO};

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

// ----------------------------------------------------------- domain utils

/// A DNS label: 1-63 chars, alphanumeric with interior hyphens only.
fn valid_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    if bytes.is_empty() || bytes.len() > 63 {
        return false;
    }
    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'a'..=b'z' | b'0'..=b'9' => {}
            b'-' => {
                if i == 0 || i == bytes.len() - 1 {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Reduces any hostname-ish string to a bare lower-case host, or `None`.
pub fn normalize_host(input: &str) -> Option<String> {
    let mut value = input.trim().to_lowercase();
    if value.is_empty() {
        return None;
    }
    // Scheme.
    if let Some(pos) = value.find("://") {
        if value[..pos].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')) && !value[..pos].is_empty() {
            value = value[pos + 3..].to_string();
        }
    }
    if let Some(rest) = value.strip_prefix("//") {
        value = rest.to_string();
    }
    // Path, query, fragment.
    if let Some(cut) = value.find(|c| c == '/' || c == '?' || c == '#') {
        value = value[..cut].to_string();
    }
    // Userinfo.
    if let Some(at) = value.rfind('@') {
        value = value[at + 1..].to_string();
    }
    // Port (incl. bracketed IPv6).
    if value.starts_with('[') {
        if let Some(end) = value.find(']') {
            let after = &value[end + 1..];
            value = format!("{}{}", &value[..=end], if after.starts_with(':') { "" } else { after });
            if let Some(colon) = value[end + 1..].strip_prefix(':') {
                if colon.chars().all(|c| c.is_ascii_digit()) {
                    value = value[..end + 1].to_string();
                }
            }
        }
    } else if let Some(colon) = value.rfind(':') {
        if value[colon + 1..].chars().all(|c| c.is_ascii_digit()) && !value[colon + 1..].is_empty() {
            value = value[..colon].to_string();
        }
    }
    // Root dot.
    while value.ends_with('.') {
        value.pop();
    }
    // www prefix.
    if let Some(rest) = value.strip_prefix("www.") {
        value = rest.to_string();
    }
    if value.len() > 253 {
        return None;
    }
    let labels: Vec<&str> = value.split('.').collect();
    if labels.len() < 2 {
        return None;
    }
    if !labels.iter().all(|l| valid_label(l)) {
        return None;
    }
    Some(value)
}

/// Suffix candidates, most specific first, down to the last two labels.
pub fn host_candidates(host: &str) -> Vec<String> {
    let labels: Vec<&str> = host.split('.').collect();
    (0..labels.len().saturating_sub(1))
        .map(|i| labels[i..].join("."))
        .collect()
}

fn require_host(input: &str) -> Result<String, ApiError> {
    normalize_host(input).ok_or_else(|| {
        ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_HOST"),
            "'domain' is not a valid host name. Pass a domain such as ox.ac.uk, or a URL such as https://dept.example.edu/path.".to_string(),
            json!({ "parameter": "domain", "value": input }),
        )
    })
}

// -------------------------------------------------------------- query DTOs

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UniversityQuery {
    /// Substring match on the institution name.
    pub search: Option<String>,
    /// Alias for `search`.
    pub name: Option<String>,
    /// Two-letter ISO code or exact country name.
    pub country: Option<String>,
    /// 1..=100 (default 20).
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// One of: name, name_desc, country, country_desc, domain.
    pub sort: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CountriesQuery {
    /// Narrow the list to one country.
    pub country: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    let scope = web::scope(SERVICE_PREFIX)
        .route("/universities/domain/{domain}", web::get().to(resolve_domain))
        .route("/universities/{id}/countries", web::get().to(countries_by_id))
        .route("/universities/{id}", web::get().to(detail))
        .route("/universities", web::get().to(list))
        .route("/countries", web::get().to(countries))
        .route("/health/ready", web::get().to(health_ready))
        .route("/health", web::get().to(health));
    cfg.service(scope);
}

fn meta_json(_state: &AppState, started: std::time::Instant, snapshot: &UniversitySnapshot) -> Value {
    json!({
        "durationMs": started.elapsed().as_millis() as u64,
        "cached": true,
        "upstream": UPSTREAM_HIPO,
        "snapshot": {
            "fetchedAt": snapshot.fetched_at,
            "ageSeconds": snapshot.age_seconds(),
            "records": snapshot.record_count,
            "domains": snapshot.domain_count,
            "countries": snapshot.country_count,
        },
    })
}

impl UniversitySnapshot {
    pub fn age_seconds(&self) -> u64 {
        chrono::DateTime::parse_from_rfc3339(&self.fetched_at)
            .map(|t| {
                let delta = chrono::Utc::now().signed_duration_since(t.with_timezone(&chrono::Utc));
                delta.num_seconds().max(0) as u64
            })
            .unwrap_or(0)
    }
}

fn resolve_country(snapshot: &UniversitySnapshot, raw: &Option<String>) -> Result<Option<(String, String)>, ApiError> {
    let Some(value) = raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let upper = value.to_uppercase();
    if upper.len() == 2 {
        if let Some(name) = snapshot.country_names.get(&upper) {
            return Ok(Some((upper.clone(), name.clone())));
        }
    }
    let lower = value.to_lowercase();
    for c in &snapshot.countries {
        if c.name.to_lowercase() == lower {
            return Ok(Some((c.code.clone(), c.name.clone())));
        }
    }
    Err(ApiError::validation_details(
        format!("{CODE_PREFIX}_UNKNOWN_COUNTRY"),
        format!("Unknown country '{value}'"),
        json!({
            "value": value,
            "hint": "Use a two-letter ISO 3166-1 alpha-2 code or an exact country name.",
            "allowedCount": snapshot.country_count,
            "sample": snapshot.countries.iter().take(10).map(|c| json!({ "code": c.code, "name": c.name })).collect::<Vec<_>>(),
            "allowed": snapshot.countries.iter().map(|c| c.code.clone()).collect::<Vec<_>>(),
        }),
    ))
}

fn resolve_page(limit: Option<usize>, offset: Option<usize>) -> (usize, usize) {
    let l = match limit {
        None => DEFAULT_LIMIT,
        Some(v) => v.clamp(1, MAX_LIMIT),
    };
    (l, offset.unwrap_or(0))
}

fn pagination(total: usize, limit: usize, offset: usize, window_len: usize) -> Value {
    let has_more = offset + window_len < total;
    json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "count": window_len,
        "hasMore": has_more,
        "nextOffset": if has_more { json!(offset + window_len) } else { Value::Null },
    })
}

fn sort_universities(records: Vec<Value>, sort: &str) -> Vec<Value> {
    let (dir, primary) = if let Some(p) = sort.strip_suffix("_desc") { (-1i32, p) } else { (1, sort) };
    let mut out = records;
    out.sort_by(|a, b| {
        let ord = match primary {
            "country" => {
                let ca = a.get("country").and_then(|v| v.as_str()).unwrap_or("");
                let cb = b.get("country").and_then(|v| v.as_str()).unwrap_or("");
                ca.cmp(cb).then_with(|| {
                    let xa = a.get("countryCode").and_then(|v| v.as_str()).unwrap_or("");
                    let xb = b.get("countryCode").and_then(|v| v.as_str()).unwrap_or("");
                    xa.cmp(xb)
                })
            }
            "domain" => {
                let da = a.get("domains").and_then(|v| v.as_array()).and_then(|a| a.first()).and_then(|v| v.as_str()).unwrap_or("");
                let db = b.get("domains").and_then(|v| v.as_array()).and_then(|a| a.first()).and_then(|v| v.as_str()).unwrap_or("");
                da.cmp(db)
            }
            _ => {
                let na = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let nb = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
                na.cmp(nb)
            }
        };
        let ia = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let ib = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let final_ord = if ord == std::cmp::Ordering::Equal { ia.cmp(ib) } else { ord };
        if dir < 0 { final_ord.reverse() } else { final_ord }
    });
    out
}

fn university_json(u: &crate::snapshot::UniversityRecord) -> Value {
    serde_json::to_value(u).unwrap_or(Value::Null)
}

/// `GET /university/universities`
#[utoipa::path(get, path = "/university/universities", context_path = "/university", params(UniversityQuery),
    responses((status = 200, description = "Search results")))]
async fn list(
    state: web::Data<AppState>,
    query: web::Query<UniversityQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let snapshot = get_snapshot(&state).await?;
    let term = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| query.name.as_deref().map(str::trim).filter(|s| !s.is_empty()))
        .map(|s| s.to_lowercase());
    let country = resolve_country(&snapshot, &query.country)?;
    let sort = match query.sort.as_deref() {
        None => "name".to_string(),
        Some(s) if SORT_KEYS.contains(&s) => s.to_string(),
        Some(s) => {
            return Err(ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_SORT"),
                format!("Unknown sort key \"{s}\""),
                json!({ "sort": s, "allowed": SORT_KEYS }),
            ));
        }
    };
    let (limit, offset) = resolve_page(query.limit, query.offset);

    let mut matched: Vec<Value> = snapshot
        .records
        .iter()
        .filter(|u| {
            if let Some(t) = &term {
                if !u.name.to_lowercase().contains(t) {
                    return false;
                }
            }
            if let Some((code, _)) = &country {
                if &u.country_code != code {
                    return false;
                }
            }
            true
        })
        .map(|u| university_json(u))
        .collect();
    matched = sort_universities(matched, &sort);
    let total = matched.len();
    let window: Vec<Value> = matched.into_iter().skip(offset).take(limit).collect();

    let body = Envelope::ok(
        json!({
            "results": window,
            "pagination": pagination(total, limit, offset, window.len()),
            "filters": {
                "search": term,
                "country": country.map(|(code, name)| json!({ "code": code, "name": name })).unwrap_or(Value::Null),
                "sort": sort,
                "tiebreaker": "id (ascending)",
            },
        }),
        meta_json(&state, started, &snapshot),
    );
    Ok(Json(body.to_json()))
}

/// `GET /university/universities/{id}`
#[utoipa::path(get, path = "/university/universities/{id}", context_path = "/university",
    params(("id" = String, Path, description = "Stable university id, e.g. university-of-zurich-ch")),
    responses((status = 200, description = "One university"), (status = 404, description = "Unknown id")))]
async fn detail(state: web::Data<AppState>, path: web::Path<String>) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let snapshot = get_snapshot(&state).await?;
    let id = path.into_inner();
    let key = id.trim().to_lowercase();
    let university = snapshot.by_id.get(&key).ok_or_else(|| {
        ApiError::not_found_details(
            format!("{CODE_PREFIX}_NOT_FOUND"),
            format!("No university with id '{id}'"),
            json!({ "id": id, "hint": "Call GET /university/universities?search=<name> to find the id." }),
        )
    })?;

    let siblings = siblings_of(&snapshot, university);
    let also_known_in: Vec<Value> = siblings
        .iter()
        .filter(|s| s.id != university.id)
        .map(|s| json!({ "id": s.id, "countryCode": s.country_code, "country": s.country }))
        .collect();
    let countries = countries_of(&snapshot, university);

    let body = Envelope::ok(
        json!({
            "university": university_json(university),
            "alsoKnownIn": also_known_in,
            "countries": countries,
        }),
        meta_json(&state, started, &snapshot),
    );
    Ok(Json(body.to_json()))
}

/// `GET /university/universities/{id}/countries`
#[utoipa::path(get, path = "/university/universities/{id}/countries", context_path = "/university",
    params(("id" = String, Path)),
    responses((status = 200, description = "Countries of an institution"), (status = 404, description = "Unknown id")))]
async fn countries_by_id(state: web::Data<AppState>, path: web::Path<String>) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let snapshot = get_snapshot(&state).await?;
    let id = path.into_inner();
    let key = id.trim().to_lowercase();
    let university = snapshot.by_id.get(&key).ok_or_else(|| {
        ApiError::not_found_details(
            format!("{CODE_PREFIX}_NOT_FOUND"),
            format!("No university with id '{id}'"),
            json!({ "id": id, "hint": "Call GET /university/universities?search=<name> to find the id." }),
        )
    })?;
    let countries = countries_of(&snapshot, university);
    let count = countries.len();
    let name = university.name.clone();

    let body = Envelope::ok(
        json!({ "id": university.id, "name": name, "countryCount": count, "countries": countries }),
        meta_json(&state, started, &snapshot),
    );
    Ok(Json(body.to_json()))
}

/// `GET /university/universities/domain/{domain}`
#[utoipa::path(get, path = "/university/universities/domain/{domain}", context_path = "/university",
    params(("domain" = String, Path, description = "A hostname, URL or email domain, e.g. ox.ac.uk")))]
async fn resolve_domain(state: web::Data<AppState>, path: web::Path<String>) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let snapshot = get_snapshot(&state).await?;
    let input = path.into_inner();
    let normalized = require_host(&input)?;
    let candidates = host_candidates(&normalized);

    for candidate in &candidates {
        if let Some(matches) = snapshot.by_domain.get(candidate) {
            if matches.is_empty() {
                continue;
            }
            let university = &matches[0];
            let extra_labels: Vec<String> = normalized
                [..normalized.len() - candidate.len()]
                .split('.')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            let country_codes: Vec<String> =
                siblings_of(&snapshot, university).iter().map(|s| s.country_code.clone()).collect();
            let matched_by = if *candidate == normalized { "exact" } else { "subdomain" };
            let body = Envelope::ok(
                json!({
                    "input": input.trim(),
                    "normalized": normalized,
                    "matchedDomain": candidate,
                    "matchedBy": matched_by,
                    "extraLabels": extra_labels,
                    "university": university_json(university),
                    "countryCodes": country_codes,
                }),
                meta_json(&state, started, &snapshot),
            );
            return Ok(Json(body.to_json()));
        }
    }

    Err(ApiError::not_found_details(
        format!("{CODE_PREFIX}_NOT_FOUND"),
        format!("'{normalized}' is not a known academic domain"),
        json!({
            "input": input.trim(),
            "normalized": normalized,
            "shortestCandidate": candidates.last().cloned(),
            "searchedCandidates": candidates.len(),
            "hint": "The dataset covers institutional domains only; commercial and government hosts are not in it.",
        }),
    ))
}

/// `GET /university/countries`
#[utoipa::path(get, path = "/university/countries", context_path = "/university", params(CountriesQuery),
    responses((status = 200, description = "Countries in the dataset")))]
async fn countries(
    state: web::Data<AppState>,
    query: web::Query<CountriesQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let snapshot = get_snapshot(&state).await?;
    let requested = query.country.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let mut results: Vec<Value> = snapshot
        .countries
        .iter()
        .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
        .collect();
    if requested.is_some() {
        let country = resolve_country(&snapshot, &query.country)?;
        let code = country.map(|(c, _)| c).unwrap_or_default();
        results.retain(|c| c.get("code").and_then(|v| v.as_str()) == Some(code.as_str()));
    }
    let (limit, offset) = resolve_page(query.limit, query.offset);
    let total = results.len();
    let window: Vec<Value> = results.into_iter().skip(offset).take(limit).collect();

    let body = Envelope::ok(
        json!({
            "results": window,
            "pagination": pagination(total, limit, offset, window.len()),
            "filter": { "country": requested.map(String::from) },
        }),
        meta_json(&state, started, &snapshot),
    );
    Ok(Json(body.to_json()))
}

// ------------------------------------------------------------------ health

async fn health_report(state: &AppState) -> Value {
    let started = std::time::Instant::now();
    match get_snapshot(state).await {
        Ok(snapshot) => json!({
            "status": "ok",
            "uptimeSeconds": state.uptime_seconds(),
            "upstream": { "reachable": true, "dataset": UPSTREAM_HIPO },
            "snapshot": {
                "fetchedAt": snapshot.fetched_at,
                "records": snapshot.record_count,
                "domains": snapshot.domain_count,
                "countries": snapshot.country_count,
                "durationMs": started.elapsed().as_millis() as u64,
            },
            "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        }),
        Err(e) => json!({
            "status": "degraded",
            "uptimeSeconds": state.uptime_seconds(),
            "upstream": { "reachable": false, "dataset": UPSTREAM_HIPO, "error": e.to_string() },
            "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        }),
    }
}

/// `GET /university/health`
#[utoipa::path(get, path = "/university/health", context_path = "/university",
    responses((status = 200, description = "Liveness plus dataset probe")))]
async fn health(state: web::Data<AppState>) -> Json<Value> {
    Json(health_report(&state).await)
}

/// `GET /university/health/ready`
#[utoipa::path(get, path = "/university/health/ready", context_path = "/university",
    responses((status = 200, description = "Dataset loaded"), (status = 503, description = "Dataset unavailable")))]
async fn health_ready(state: web::Data<AppState>) -> Result<Json<Value>, ApiError> {
    let snapshot = get_snapshot(&state).await?;
    Ok(Json(json!({
        "status": "ready",
        "records": snapshot.record_count,
        "asOf": snapshot.fetched_at,
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })))
}

// ----------------------------------------------------------------- helpers

fn siblings_of<'s>(
    snapshot: &'s UniversitySnapshot,
    u: &crate::snapshot::UniversityRecord,
) -> Vec<std::sync::Arc<crate::snapshot::UniversityRecord>> {
    if let Some(bucket) = snapshot.by_name.get(&u.name.to_lowercase()) {
        return bucket.clone();
    }
    match snapshot.by_id.get(&u.id) {
        Some(existing) => vec![existing.clone()],
        None => vec![std::sync::Arc::new(u.clone())],
    }
}

fn countries_of(
    snapshot: &UniversitySnapshot,
    u: &crate::snapshot::UniversityRecord,
) -> Vec<Value> {
    let mut grouped: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
    for sibling in siblings_of(snapshot, u) {
        let entry = grouped.entry(sibling.country_code.clone()).or_insert_with(|| {
            json!({
                "countryCode": sibling.country_code,
                "country": sibling.country,
                "statesProvinces": [],
                "domains": [],
                "webPages": [],
            })
        });
        if let Some(sp) = &sibling.state_province {
            entry["statesProvinces"].as_array_mut().unwrap().push(json!(sp));
        }
        for d in &sibling.domains {
            entry["domains"].as_array_mut().unwrap().push(json!(d));
        }
        for w in &sibling.web_pages {
            entry["webPages"].as_array_mut().unwrap().push(json!(w));
        }
    }
    let mut out: Vec<Value> = grouped.into_values().collect();
    for e in &mut out {
        e["statesProvinces"] = json!(unique_sorted_strings(&e["statesProvinces"]));
        e["domains"] = json!(unique_sorted_strings(&e["domains"]));
        e["webPages"] = json!(unique_sorted_strings(&e["webPages"]));
    }
    out.sort_by(|a, b| {
        let ca = a.get("country").and_then(|v| v.as_str()).unwrap_or("");
        let cb = b.get("country").and_then(|v| v.as_str()).unwrap_or("");
        ca.cmp(cb).then_with(|| {
            let xa = a.get("countryCode").and_then(|v| v.as_str()).unwrap_or("");
            let xb = b.get("countryCode").and_then(|v| v.as_str()).unwrap_or("");
            xa.cmp(xb)
        })
    });
    out
}

fn unique_sorted_strings(v: &Value) -> Vec<String> {
    let mut set: Vec<String> = v
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    set.sort();
    set.dedup();
    set
}

// ---------------------------------------------------------------- OpenAPI

#[derive(OpenApi)]
#[openapi(
    info(title = "university-api", version = "1.0.0", description = "Searchable directory of world universities with stable ids, multi-country grouping and academic domain resolution."),
    paths(list, detail, countries_by_id, resolve_domain, countries, health, health_ready)
)]
pub struct UniversityApi;

pub fn openapi_doc() -> utoipa::openapi::OpenApi {
    let mut doc = UniversityApi::openapi();
    doc.info.title = "university-api".into();
    doc.servers = Some(vec![utoipa::openapi::Server::new(SERVICE_PREFIX)]);
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_hostnames() {
        assert_eq!(normalize_host("https://dept.example.edu/path"), Some("dept.example.edu".into()));
        assert_eq!(normalize_host("WWW.Oxford.AC.UK/"), Some("oxford.ac.uk".into()));
        assert_eq!(normalize_host("user@mail.ox.ac.uk"), Some("mail.ox.ac.uk".into()));
        assert_eq!(normalize_host("ox.ac.uk:443"), Some("ox.ac.uk".into()));
        assert_eq!(normalize_host("nohost"), None);
        assert_eq!(normalize_host(""), None);
    }

    #[test]
    fn builds_suffix_candidates() {
        // Most specific first, down to the shortest two-label suffix — the
        // dataset is consulted in this order (matches the original's loop).
        assert_eq!(
            host_candidates("dept.sub.example.co.uk"),
            vec!["dept.sub.example.co.uk", "sub.example.co.uk", "example.co.uk", "co.uk"]
        );
        assert_eq!(host_candidates("ox.ac.uk"), vec!["ox.ac.uk", "ac.uk"]);
    }

    #[test]
    fn pagination_shape() {
        let p = pagination(100, 20, 20, 20);
        assert_eq!(p["hasMore"], json!(true));
        assert_eq!(p["nextOffset"], json!(40));
        let p2 = pagination(50, 20, 40, 10);
        assert_eq!(p2["hasMore"], json!(false));
        assert_eq!(p2["nextOffset"], Value::Null);
    }

    #[test]
    fn sorts_desc() {
        let a = json!({"id": "a", "name": "Alpha", "country": "Aaa", "countryCode": "AA", "domains": ["a.edu"]});
        let b = json!({"id": "b", "name": "Beta", "country": "Bbb", "countryCode": "BB", "domains": ["b.edu"]});
        let out = sort_universities(vec![a.clone(), b.clone()], "name_desc");
        assert_eq!(out[0]["id"], "b");
        let out2 = sort_universities(vec![a.clone(), b.clone()], "name");
        assert_eq!(out2[0]["id"], "a");
    }
}
