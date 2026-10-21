//! Route handlers for the countries service.

use actix_web::web::{self, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use utoipa::{IntoParams, OpenApi};

use master_common::envelope::Envelope;
use master_common::error::ApiError;
use master_common::state::AppState;

use crate::model::{
    compare_optional_f64, compare_optional_i64, compare_optional_text, compare_text,
    country_stub, field_catalogue_json, is_known_field, normalise_text, project_country,
    DEFAULT_FIELDS, SORT_KEYS,
};
use crate::snapshot::get_snapshot;
use crate::{CODE_PREFIX, SERVICE_PREFIX};

/// Query for `GET /countries/countries`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListCountriesQuery {
    /// Substring match over common/official names, alt-spellings and capitals.
    pub search: Option<String>,
    pub region: Option<String>,
    pub subregion: Option<String>,
    pub continent: Option<String>,
    /// Filter by currency code (e.g. `tzs`, case-insensitive).
    pub currency: Option<String>,
    /// Filter by language code (e.g. `eng`, case-insensitive).
    pub language: Option<String>,
    pub population_min: Option<i64>,
    pub population_max: Option<i64>,
    /// Strict `true`/`false`.
    pub landlocked: Option<bool>,
    /// Strict `true`/`false`.
    pub un_member: Option<bool>,
    /// 1..=250 (default 20).
    pub limit: Option<usize>,
    /// Hard-capped offset to keep a typo from scanning past the dataset.
    pub offset: Option<usize>,
    /// One of the documented sort keys (default `name`).
    pub sort: Option<String>,
    /// `asc` (default) or `desc`.
    pub order: Option<String>,
    /// Comma-separated projection; unknown names are rejected with a 400.
    pub fields: Option<String>,
}

/// Query for `GET /countries/countries/{code}` and nested routes.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FieldsQuery {
    /// Comma-separated projection; unknown names are rejected with a 400.
    pub fields: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct NeighboursQuery {
    /// `asc` (default) or `desc`.
    pub order: Option<String>,
    pub fields: Option<String>,
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    let scope = web::scope(SERVICE_PREFIX)
        // Static routes first so literals are never swallowed by `{code}`.
        .route("/fields", web::get().to(fields))
        .route("/regions", web::get().to(regions))
        .route("/countries", web::get().to(list_countries))
        .route("/countries/{code}/neighbours", web::get().to(neighbours))
        .route("/countries/{code}/currencies", web::get().to(currencies))
        .route("/countries/{code}/languages", web::get().to(languages))
        .route("/countries/{code}", web::get().to(get_country))
        .route("/health/ready", web::get().to(health_ready))
        .route("/health", web::get().to(health));
    cfg.service(scope);
}

fn parse_fields(raw: &Option<String>) -> Result<Vec<String>, ApiError> {
    match raw {
        None => Ok(DEFAULT_FIELDS.iter().map(|s| s.to_string()).collect()),
        Some(csv) => {
            let fields: Vec<String> = csv
                .split(',')
                .map(|f| f.trim().to_string())
                .filter(|f| !f.is_empty())
                .collect();
            let invalid: Vec<String> =
                fields.iter().filter(|f| !is_known_field(f)).cloned().collect();
            if !invalid.is_empty() {
                return Err(ApiError::validation_details(
                    format!("{CODE_PREFIX}_UNKNOWN_FIELD"),
                    format!(
                        "Unknown field name{}: {}",
                        if invalid.len() > 1 { "s" } else { "" },
                        invalid.join(", ")
                    ),
                    json!({ "invalid": invalid, "allowed": field_names_json() }),
                ));
            }
            let mut seen = std::collections::BTreeSet::new();
            Ok(fields.into_iter().filter(|f| seen.insert(f.clone())).collect())
        }
    }
}

fn field_names_json() -> Value {
    Value::Array(
        crate::model::FIELD_NAMES
            .iter()
            .map(|s| Value::String(s.to_string()))
            .collect(),
    )
}

fn resolve_limit(limit: Option<usize>) -> Result<usize, ApiError> {
    const DEFAULT_LIMIT: usize = 20;
    const MAX_LIMIT: usize = 250;
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

fn canonical_filter(
    param: &str,
    value: &Option<String>,
    allowed: &[String],
) -> Result<Option<String>, ApiError> {
    match value {
        None => Ok(None),
        Some(v) => {
            let hit = allowed.iter().find(|c| c.eq_ignore_ascii_case(v));
            match hit {
                Some(canonical) => Ok(Some(canonical.clone())),
                None => Err(ApiError::validation_details(
                    format!("{CODE_PREFIX}_UNKNOWN_{0}", param.to_uppercase()),
                    format!("Unknown {param} \"{v}\""),
                    json!({ param: v, "allowed": allowed }),
                )),
            }
        }
    }
}

fn resolve_country<'s>(
    snapshot: &'s crate::snapshot::Snapshot,
    raw: &str,
) -> Result<(&'s std::sync::Arc<crate::model::Country>, &'static str), ApiError> {
    let term = raw.trim();
    if term.is_empty() {
        return Err(ApiError::not_found_details(
            format!("{CODE_PREFIX}_NOT_FOUND"),
            "No country code was supplied",
            json!({ "requested": raw }),
        ));
    }
    let upper = term.to_uppercase();
    let numeric = if term.chars().all(|c| c.is_ascii_digit()) && term.len() <= 3 {
        format!("{:0>3}", term)
    } else {
        upper.clone()
    };
    if let Some(c) = snapshot.by_alpha2.get(&upper) {
        return Ok((c, "alpha2"));
    }
    if let Some(c) = snapshot.by_alpha3.get(&upper) {
        return Ok((c, "alpha3"));
    }
    if let Some(c) = snapshot.by_numeric.get(&numeric) {
        return Ok((c, "numeric"));
    }
    if let Some(c) = snapshot.by_ioc.get(&upper) {
        return Ok((c, "ioc"));
    }
    if let Some(c) = snapshot.by_name.get(&normalise_text(term)) {
        return Ok((c, "name"));
    }
    Err(ApiError::not_found_details(
        format!("{CODE_PREFIX}_NOT_FOUND"),
        format!("No country matches \"{term}\""),
        json!({
            "requested": term,
            "accepted": ["alpha-2 code", "alpha-3 code", "numeric code", "IOC code", "country name"],
            "hint": "Call GET /countries/countries?limit=250 to list every country."
        }),
    ))
}

fn snapshot_meta(snapshot: &crate::snapshot::Snapshot) -> Value {
    json!({
        "fetchedAt": snapshot.fetched_at,
        "countries": snapshot.countries.len(),
        "durationMs": snapshot.duration_ms,
        "sources": snapshot.sources,
    })
}

/// `GET /countries/fields`
#[utoipa::path(get, path = "/countries/fields", context_path = "/countries",
    responses((status = 200, description = "Field catalogue")))]
async fn fields(state: web::Data<AppState>) -> Result<Json<Value>, ApiError> {
    let snapshot = get_snapshot(&state).await?;
    let sources: Vec<String> = field_catalogue_json()
        .as_array()
        .map(|arr| {
            let mut set: Vec<String> = Vec::new();
            for item in arr {
                if let Some(s) = item.get("source").and_then(|v| v.as_str()) {
                    if !set.iter().any(|x| x == s) {
                        set.push(s.to_string());
                    }
                }
            }
            set
        })
        .unwrap_or_default();
    let body = Envelope::ok(
        json!({
            "count": field_catalogue_json().as_array().map(|a| a.len()).unwrap_or(0),
            "default": DEFAULT_FIELDS,
            "sortKeys": SORT_KEYS,
            "sources": sources,
            "fields": field_catalogue_json(),
        }),
        json!({ "snapshot": { "fetchedAt": Value::Null, "countries": 0, "durationMs": 0, "sources": snapshot.sources } }),
    );
    Ok(Json(body.to_json()))
}

/// `GET /countries/regions`
#[utoipa::path(get, path = "/countries/regions", context_path = "/countries",
    responses((status = 200, description = "Reference aggregates")))]
async fn regions(state: web::Data<AppState>) -> Result<Json<Value>, ApiError> {
    let snapshot = get_snapshot(&state).await?;
    let body = Envelope::ok(
        json!({
            "totals": snapshot.totals,
            "regions": snapshot.regions,
            "subregions": snapshot.subregions,
            "continents": snapshot.continents,
        }),
        json!({ "count": snapshot.regions.len(), "snapshot": snapshot_meta(&snapshot) }),
    );
    Ok(Json(body.to_json()))
}

/// `GET /countries/countries`
#[utoipa::path(get, path = "/countries/countries", context_path = "/countries",
    params(ListCountriesQuery),
    responses((status = 200, description = "Filtered, sorted, paginated countries")))]
async fn list_countries(
    state: web::Data<AppState>,
    query: web::Query<ListCountriesQuery>,
) -> Result<Json<Value>, ApiError> {
    let snapshot = get_snapshot(&state).await?;
    let limit = resolve_limit(query.limit)?;
    let offset = query.offset.unwrap_or(0);
    if offset > 100_000 {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_OFFSET"),
            "offset must not exceed 100000",
            json!({ "offset": offset }),
        ));
    }
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
    let order = match query.order.as_deref() {
        None => "asc".to_string(),
        Some(o) if o == "asc" || o == "desc" => o.to_string(),
        Some(o) => {
            return Err(ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_ORDER"),
                "order must be either \"asc\" or \"desc\"",
                json!({ "order": o }),
            ));
        }
    };
    let fields = parse_fields(&query.fields)?;

    let region = canonical_filter("region", &query.region, &snapshot.region_names)?;
    let subregion = canonical_filter("subregion", &query.subregion, &snapshot.subregion_names)?;
    let continent = canonical_filter("continent", &query.continent, &snapshot.continent_names)?;
    let currency = canonical_filter("currency", &query.currency, &snapshot.currency_codes)?;
    let language = canonical_filter("language", &query.language, &snapshot.language_codes)?;
    if let (Some(min), Some(max)) = (query.population_min, query.population_max) {
        if min > max {
            return Err(ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_POPULATION_WINDOW"),
                "population_min cannot be greater than population_max",
                json!({ "population_min": min, "population_max": max }),
            ));
        }
    }
    let needle = query.search.as_deref().map(normalise_text).unwrap_or_default();

    let mut matched: Vec<&std::sync::Arc<crate::model::Country>> = snapshot
        .countries
        .iter()
        .filter(|c| {
            if !needle.is_empty() {
                let haystack: Vec<String> = std::iter::once(c.name_common.clone())
                    .chain(std::iter::once(c.name_official.clone()))
                    .chain(c.alt_spellings.iter().cloned())
                    .chain(c.capitals.iter().cloned())
                    .collect();
                if !haystack.iter().any(|v| normalise_text(v).contains(&needle)) {
                    return false;
                }
            }
            if let Some(r) = &region {
                if &c.region != r {
                    return false;
                }
            }
            if let Some(sr) = &subregion {
                if c.subregion.as_deref() != Some(sr.as_str()) {
                    return false;
                }
            }
            if let Some(cont) = &continent {
                if c.continent_name.as_deref() != Some(cont.as_str()) {
                    return false;
                }
            }
            if let Some(cur) = &currency {
                if !c.currencies.iter().any(|item| item.code.to_lowercase() == *cur) {
                    return false;
                }
            }
            if let Some(lang) = &language {
                if !c.languages.iter().any(|item| item.code.to_lowercase() == *lang) {
                    return false;
                }
            }
            if let Some(ll) = query.landlocked {
                if c.landlocked != ll {
                    return false;
                }
            }
            if let Some(un) = query.un_member {
                if c.un_member != un {
                    return false;
                }
            }
            if let Some(min) = query.population_min {
                if c.population.unwrap_or(-1) < min {
                    return false;
                }
            }
            if let Some(max) = query.population_max {
                if c.population.unwrap_or(-1) > max {
                    return false;
                }
            }
            true
        })
        .collect();

    let direction = if order == "desc" { -1 } else { 1 };
    matched.sort_by(|a, b| {
        let primary = match sort.as_str() {
            "name" => compare_text(&a.name_common, &b.name_common),
            "officialName" => compare_text(&a.name_official, &b.name_official),
            "alpha2" => compare_text(&a.alpha2, &b.alpha2),
            "alpha3" => compare_text(&a.alpha3, &b.alpha3),
            "numeric" => compare_optional_text(&a.numeric, &b.numeric),
            "population" => compare_optional_i64(&a.population, &b.population),
            "area" => compare_optional_f64(&a.area, &b.area),
            "region" => compare_text(&a.region, &b.region),
            "subregion" => compare_optional_text(&a.subregion, &b.subregion),
            "continent" => compare_optional_text(&a.continent_name, &b.continent_name),
            "capital" => compare_optional_text(&a.capitals.first().cloned(), &b.capitals.first().cloned()),
            "landlocked" => a.landlocked.cmp(&b.landlocked),
            "independent" => a.independent.cmp(&b.independent),
            "unMember" => a.un_member.cmp(&b.un_member),
            "status" => compare_text(&a.status, &b.status),
            _ => std::cmp::Ordering::Equal,
        };
        let tie = compare_text(&a.name_common, &b.name_common)
            .then_with(|| compare_text(&a.alpha3, &b.alpha3));
        if primary == std::cmp::Ordering::Equal {
            tie
        } else if direction == 1 {
            primary
        } else {
            primary.reverse()
        }
    });

    let total = matched.len();
    let page: Vec<Value> = matched
        .iter()
        .skip(offset)
        .take(limit)
        .map(|c| project_country(c, &fields))
        .collect();

    let body = Envelope::ok(
        json!({ "countries": page }),
        json!({
            "count": page.len(),
            "total": total,
            "limit": limit,
            "offset": offset,
            "sort": sort,
            "order": order,
            "fields": match query.fields.as_ref() {
                Some(f) => Value::Array(f.split(',').map(|s| Value::String(s.trim().to_string())).collect()),
                None => Value::Null,
            },
            "snapshot": snapshot_meta(&snapshot),
        }),
    );
    Ok(Json(body.to_json()))
}

/// `GET /countries/countries/{code}`
#[utoipa::path(get, path = "/countries/countries/{code}", context_path = "/countries",
    params(("code" = String, Path, description = "alpha-2/alpha-3/numeric/IOC code or country name"), FieldsQuery),
    responses((status = 200, description = "One country"), (status = 404, description = "No match")))]
async fn get_country(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<FieldsQuery>,
) -> Result<Json<Value>, ApiError> {
    let snapshot = get_snapshot(&state).await?;
    let code = path.into_inner();
    let (country, kind) = resolve_country(&snapshot, &code)?;
    let fields = if query.fields.as_deref().map(|f| !f.trim().is_empty()).unwrap_or(false) {
        parse_fields(&query.fields)?
    } else {
        crate::model::FIELD_NAMES.iter().map(|s| s.to_string()).collect()
    };
    let body = Envelope::ok(
        json!({ "country": project_country(country, &fields) }),
        json!({ "snapshot": snapshot_meta(&snapshot), "resolvedBy": kind, "requested": code }),
    );
    Ok(Json(body.to_json()))
}

/// `GET /countries/countries/{code}/neighbours`
#[utoipa::path(get, path = "/countries/countries/{code}/neighbours", context_path = "/countries",
    params(("code" = String, Path), NeighboursQuery),
    responses((status = 200, description = "Border countries")))]
async fn neighbours(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<NeighboursQuery>,
) -> Result<Json<Value>, ApiError> {
    let snapshot = get_snapshot(&state).await?;
    let code = path.into_inner();
    let (country, _) = resolve_country(&snapshot, &code)?;
    let fields = parse_fields(&query.fields)?;
    let order = match query.order.as_deref() {
        None => "asc",
        Some(o) if o == "asc" || o == "desc" => o,
        Some(o) => {
            return Err(ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_ORDER"),
                "order must be either \"asc\" or \"desc\"",
                json!({ "order": o }),
            ));
        }
    };
    let resolved = snapshot.neighbours.get(&country.alpha2).cloned().unwrap_or_default();
    let ordered: Vec<Value> = if order == "desc" {
        resolved.iter().rev().map(|c| project_country(c, &fields)).collect()
    } else {
        resolved.iter().map(|c| project_country(c, &fields)).collect()
    };
    let count = ordered.len();
    let body = Envelope::ok(
        json!({ "country": country_stub(country), "count": count, "neighbours": ordered }),
        json!({ "order": order, "fields": query.fields.clone().map(Value::String).unwrap_or(Value::Null), "snapshot": snapshot_meta(&snapshot) }),
    );
    Ok(Json(body.to_json()))
}

/// `GET /countries/countries/{code}/currencies`
#[utoipa::path(get, path = "/countries/countries/{code}/currencies", context_path = "/countries",
    params(("code" = String, Path)),
    responses((status = 200, description = "Currencies of a country")))]
async fn currencies(state: web::Data<AppState>, path: web::Path<String>) -> Result<Json<Value>, ApiError> {
    let snapshot = get_snapshot(&state).await?;
    let code = path.into_inner();
    let (country, _) = resolve_country(&snapshot, &code)?;
    let list: Vec<Value> = country
        .currencies
        .iter()
        .map(|cur| json!({ "code": cur.code, "name": cur.name, "symbol": cur.symbol }))
        .collect();
    let count = list.len();
    let body = Envelope::ok(
        json!({ "country": country_stub(country), "count": count, "currencies": list }),
        json!({ "snapshot": snapshot_meta(&snapshot) }),
    );
    Ok(Json(body.to_json()))
}

/// `GET /countries/countries/{code}/languages`
#[utoipa::path(get, path = "/countries/countries/{code}/languages", context_path = "/countries",
    params(("code" = String, Path)),
    responses((status = 200, description = "Languages of a country")))]
async fn languages(state: web::Data<AppState>, path: web::Path<String>) -> Result<Json<Value>, ApiError> {
    let snapshot = get_snapshot(&state).await?;
    let code = path.into_inner();
    let (country, _) = resolve_country(&snapshot, &code)?;
    let list: Vec<Value> = country
        .languages
        .iter()
        .map(|l| json!({ "code": l.code, "name": l.name }))
        .collect();
    let count = list.len();
    let body = Envelope::ok(
        json!({ "country": country_stub(country), "count": count, "languages": list }),
        json!({ "snapshot": snapshot_meta(&snapshot) }),
    );
    Ok(Json(body.to_json()))
}

/// Health report for the countries service.
async fn health_report(state: &AppState) -> Value {
    let started = std::time::Instant::now();
    match get_snapshot(state).await {
        Ok(snapshot) => json!({
            "status": "ok",
            "uptimeSeconds": state.uptime_seconds(),
            "upstream": {
                "ok": true,
                "countries": snapshot.countries.len(),
                "fetchedAt": snapshot.fetched_at,
                "durationMs": started.elapsed().as_millis() as u64,
            },
            "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        }),
        Err(e) => json!({
            "status": "degraded",
            "uptimeSeconds": state.uptime_seconds(),
            "upstream": { "ok": false, "countries": 0, "fetchedAt": Value::Null, "error": e.to_string(), "durationMs": started.elapsed().as_millis() as u64 },
            "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        }),
    }
}

/// `GET /countries/health`
#[utoipa::path(get, path = "/countries/health", context_path = "/countries",
    responses((status = 200, description = "Liveness plus a real snapshot load probe")))]
async fn health(state: web::Data<AppState>) -> Json<Value> {
    Json(health_report(&state).await)
}

/// `GET /countries/health/ready`
#[utoipa::path(get, path = "/countries/health/ready", context_path = "/countries",
    responses((status = 200, description = "Snapshot loads"), (status = 503, description = "Snapshot unavailable")))]
async fn health_ready(state: web::Data<AppState>) -> Result<Json<Value>, ApiError> {
    let snapshot = get_snapshot(&state).await?;
    Ok(Json(json!({
        "status": "ready",
        "countries": snapshot.countries.len(),
        "asOf": snapshot.fetched_at,
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })))
}

// ---------------------------------------------------------------- OpenAPI

#[derive(OpenApi)]
#[openapi(
    info(title = "countries-api", version = "1.0.0", description = "Country reference data: search, filter, sort, paginate and resolve neighbours across a 250-country snapshot."),
    paths(fields, regions, list_countries, get_country, neighbours, currencies, languages, health, health_ready)
)]
pub struct CountriesApi;

pub fn openapi_doc() -> utoipa::openapi::OpenApi {
    let mut doc = CountriesApi::openapi();
    doc.info.title = "countries-api".into();
    doc.servers = Some(vec![utoipa::openapi::Server::new("/countries")]);
    doc
}
