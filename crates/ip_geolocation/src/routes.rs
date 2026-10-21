//! Route handlers for the ip-geolocation service.

use std::net::IpAddr;
use std::sync::Arc;

use actix_web::HttpRequest;
use serde_json::{json, Map, Value};

use master_common::error::ApiError;
use master_common::httpx;
use master_common::state::{cache_names, AppState};

use crate::{classify_address, ALLOWED_FIELDS, CODE_PREFIX, UPSTREAM_IPWHOIS};

pub fn configure(cfg: &mut actix_web::web::ServiceConfig) {
    use actix_web::web;
    let scope = web::scope("/ip-geolocation")
        .route("/validate/{address}", web::get().to(validate))
        .route("/geo/{address}", web::get().to(geo))
        .route("/asn/{address}", web::get().to(asn))
        .route("/ip/{address}", web::get().to(ip_lookup))
        .route("/ip", web::get().to(ip_self))
        .route("/health/ready", web::get().to(health_ready))
        .route("/health", web::get().to(health));
    cfg.service(scope);
}

fn parse_address(raw: &str) -> Result<IpAddr, ApiError> {
    raw.trim()
        .parse::<IpAddr>()
        .map_err(|_| {
            ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_ADDRESS"),
                format!("\"{raw}\" is not a valid IPv4 or IPv6 address"),
                json!({ "address": raw }),
            )
        })
}

/// `fields` query parameter: unknown names are a 400; the default is the full
/// catalogue.
fn resolve_field_selection(fields: Option<&String>) -> Result<Vec<String>, ApiError> {
    match fields {
        None => Ok(ALLOWED_FIELDS.iter().map(|s| s.to_string()).collect()),
        Some(csv) => {
            let names: Vec<String> = csv.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            let unknown: Vec<String> = names.iter().filter(|f| !ALLOWED_FIELDS.contains(&f.as_str())).cloned().collect();
            if !unknown.is_empty() {
                return Err(ApiError::validation_details(
                    format!("{CODE_PREFIX}_INVALID_REQUEST"),
                    format!("Unknown field name{}: {}", if unknown.len() > 1 { "s" } else { "" }, unknown.join(", ")),
                    json!({ "invalid": unknown, "allowed": ALLOWED_FIELDS }),
                ));
            }
            let mut seen = std::collections::BTreeSet::new();
            Ok(names.into_iter().filter(|f| seen.insert(f.clone())).collect())
        }
    }
}

fn location_record(record: &Value) -> Value {
    let tz = record.get("timezone").cloned().unwrap_or(json!({}));
    let flag = record.get("flag").cloned().unwrap_or(json!({}));
    json!({
        "continent": record.get("continent").cloned().unwrap_or(Value::Null),
        "continentCode": record.get("continent_code").cloned().unwrap_or(Value::Null),
        "country": record.get("country").cloned().unwrap_or(Value::Null),
        "countryCode": record.get("country_code").cloned().unwrap_or(Value::Null),
        "region": record.get("region").cloned().unwrap_or(Value::Null),
        "regionCode": record.get("region_code").cloned().unwrap_or(Value::Null),
        "city": record.get("city").cloned().unwrap_or(Value::Null),
        "postal": record.get("postal").cloned().unwrap_or(Value::Null),
        "latitude": record.get("latitude").cloned().unwrap_or(Value::Null),
        "longitude": record.get("longitude").cloned().unwrap_or(Value::Null),
        "timezoneId": tz.get("id").cloned().unwrap_or(Value::Null),
        "timezoneAbbreviation": tz.get("abbr").cloned().unwrap_or(Value::Null),
        "timezoneOffset": tz.get("utc").cloned().unwrap_or(Value::Null),
        "isDst": tz.get("current_time").and_then(|v| v.as_str()).map(|_| json!(true)).unwrap_or(Value::Null),
        "callingCode": record.get("calling_code").cloned().unwrap_or(Value::Null),
        "capital": record.get("capital").cloned().unwrap_or(Value::Null),
        "borders": record.get("borders").cloned().unwrap_or(json!([])),
        "isEu": record.get("is_eu").cloned().unwrap_or(Value::Null),
        "flagEmoji": flag.get("emoji").cloned().unwrap_or(Value::Null),
        "flagImage": flag.get("img").cloned().unwrap_or(Value::Null),
    })
}

fn network_record(record: &Value) -> Value {
    let conn = record.get("connection").cloned().unwrap_or(json!({}));
    json!({
        "asn": conn.get("asn").cloned().unwrap_or(Value::Null),
        "organization": conn.get("org").cloned().unwrap_or(Value::Null),
        "isp": conn.get("isp").cloned().unwrap_or(Value::Null),
        "domain": conn.get("domain").cloned().unwrap_or(Value::Null),
    })
}

fn geolocation_record(address: &str, record: &Value) -> Value {
    let facts = parse_address(address).ok().map(|a| classify_address(&a));
    let (ver, scope, is_public) = match &facts {
        Some(f) => (f.ip_version, f.scope, f.is_public),
        None => ("unknown", "unknown", false),
    };
    json!({
        "address": record.get("ip").and_then(|v| v.as_str()).unwrap_or(address),
        "ipVersion": ver,
        "scope": scope,
        "isPublic": is_public,
        "location": location_record(record),
        "network": network_record(record),
    })
}

fn project_fields(record: &Value, selection: &[String]) -> Value {
    let mut out = Map::new();
    for field in selection {
        let value = match field.as_str() {
            "flag" => json!({
                "emoji": record.pointer("/location/flagEmoji"),
                "image": record.pointer("/location/flagImage"),
            }),
            other => record.pointer(&format!("/location/{other}")).or_else(|| record.pointer(&format!("/network/{other}"))).cloned().unwrap_or(Value::Null),
        };
        out.insert(field.clone(), value);
    }
    Value::Object(out)
}

struct Lookup {
    record: Value,
    cached: bool,
}

/// Cached ipwho.is lookup; upstream `success:false` maps to a 400/404.
async fn lookup(state: &AppState, address: &str) -> Result<Lookup, ApiError> {
    let addr = parse_address(address)?;
    let canonical = addr.to_string();
    let cache = state.json_cache(cache_names::IP_GEOLOCATION);
    let key = format!("ipwho:{canonical}");
    if let Some(v) = cache.get(&key).await {
        return Ok(Lookup { record: v.as_ref().clone(), cached: true });
    }
    let url = format!("{UPSTREAM_IPWHOIS}/{canonical}");
    let body = httpx::get_json(state, &url, &[], CODE_PREFIX).await?.body;
    if body.get("success").and_then(|v| v.as_bool()) == Some(false) {
        let message = body.get("message").and_then(|v| v.as_str()).unwrap_or("lookup failed");
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_UPSTREAM_REJECTED"),
            format!("The geolocation provider rejected the lookup: {message}"),
            json!({ "address": canonical, "upstreamMessage": message }),
        ));
    }
    let shaped = geolocation_record(&canonical, &body);
    cache.insert(key, Arc::new(shaped.clone())).await;
    Ok(Lookup { record: shaped, cached: false })
}

fn meta_json(cached: bool, duration_ms: u64, source: &str, requested_fields: Option<Vec<String>>) -> Value {
    let mut meta = json!({
        "cached": cached,
        "source": source,
        "durationMs": duration_ms,
    });
    if let Some(fields) = requested_fields {
        meta["requestedFields"] = json!(fields);
    }
    meta
}

fn envelope(data: Value, meta: Value) -> Value {
    json!({ "success": true, "data": data, "meta": meta })
}

/// Extract the caller's address for `GET /ip-geolocation/ip`.
fn caller_address(req: &HttpRequest) -> (String, bool) {
    // Trust the proxy chain exactly like the original: the first hop in
    // X-Forwarded-For, then X-Real-IP, then the peer address.
    if let Some(xff) = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = xff.split(',').next() {
            let candidate = first.trim();
            if candidate.parse::<IpAddr>().is_ok() {
                return (candidate.to_string(), true);
            }
        }
    }
    if let Some(xri) = req.headers().get("x-real-ip").and_then(|v| v.to_str().ok()) {
        let candidate = xri.trim();
        if candidate.parse::<IpAddr>().is_ok() {
            return (candidate.to_string(), true);
        }
    }
    match req.peer_addr() {
        Some(peer) => (peer.ip().to_string(), false),
        None => ("127.0.0.1".to_string(), false),
    }
}

use actix_web::web::{self, Json};
use serde::Deserialize;
use serde_json::Value as V;
use utoipa::{IntoParams, OpenApi};

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FieldsQuery {
    /// Comma-separated projection over the documented field catalogue.
    pub fields: Option<String>,
}

/// `GET /ip-geolocation/ip` — the caller's address.
#[utoipa::path(get, path = "/ip-geolocation/ip", context_path = "/ip-geolocation", params(FieldsQuery),
    responses((status = 200, description = "Geolocation of the caller's address")))]
async fn ip_self(
    state: web::Data<AppState>,
    req: HttpRequest,
    query: web::Query<FieldsQuery>,
) -> Result<Json<V>, ApiError> {
    let started = std::time::Instant::now();
    let (address, forwarded) = caller_address(&req);
    let selection = resolve_field_selection(query.fields.as_ref())?;
    let lookup = lookup(&state, &address).await?;
    let meta = meta_json(lookup.cached, started.elapsed().as_millis() as u64, UPSTREAM_IPWHOIS, Some(selection.clone()));
    let mut meta = meta;
    if forwarded {
        meta["forwarded"] = json!(true);
    }
    let data = project_fields(&lookup.record, &selection);
    Ok(Json(envelope(data, meta)))
}

/// `GET /ip-geolocation/ip/{address}`
#[utoipa::path(get, path = "/ip-geolocation/ip/{address}", context_path = "/ip-geolocation",
    params(("address" = String, Path, description = "IPv4 or IPv6 address"), FieldsQuery),
    responses((status = 200, description = "Full geolocation record"), (status = 400, description = "Invalid address")))]
async fn ip_lookup(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<FieldsQuery>,
) -> Result<Json<V>, ApiError> {
    let started = std::time::Instant::now();
    let address = path.into_inner();
    let selection = resolve_field_selection(query.fields.as_ref())?;
    let lookup = lookup(&state, &address).await?;
    let data = project_fields(&lookup.record, &selection);
    let meta = meta_json(lookup.cached, started.elapsed().as_millis() as u64, UPSTREAM_IPWHOIS, Some(selection));
    Ok(Json(envelope(data, meta)))
}

/// `GET /ip-geolocation/geo/{address}`
#[utoipa::path(get, path = "/ip-geolocation/geo/{address}", context_path = "/ip-geolocation",
    params(("address" = String, Path)),
    responses((status = 200, description = "Geographic subset")))]
async fn geo(state: web::Data<AppState>, path: web::Path<String>) -> Result<Json<V>, ApiError> {
    let started = std::time::Instant::now();
    let address = path.into_inner();
    let lookup = lookup(&state, &address).await?;
    let record = &lookup.record;
    let data = json!({
        "address": record.get("address"),
        "ipVersion": record.get("ipVersion"),
        "country": record.pointer("/location/country"),
        "countryCode": record.pointer("/location/countryCode"),
        "continent": record.pointer("/location/continent"),
        "continentCode": record.pointer("/location/continentCode"),
        "region": record.pointer("/location/region"),
        "regionCode": record.pointer("/location/regionCode"),
        "city": record.pointer("/location/city"),
        "postal": record.pointer("/location/postal"),
        "latitude": record.pointer("/location/latitude"),
        "longitude": record.pointer("/location/longitude"),
        "timezone": record.pointer("/location/timezoneId"),
        "timezoneAbbreviation": record.pointer("/location/timezoneAbbreviation"),
        "timezoneOffset": record.pointer("/location/timezoneOffset"),
    });
    Ok(Json(envelope(
        data,
        meta_json(lookup.cached, started.elapsed().as_millis() as u64, UPSTREAM_IPWHOIS, None),
    )))
}

/// `GET /ip-geolocation/asn/{address}`
#[utoipa::path(get, path = "/ip-geolocation/asn/{address}", context_path = "/ip-geolocation",
    params(("address" = String, Path)),
    responses((status = 200, description = "ASN/ISP details")))]
async fn asn(state: web::Data<AppState>, path: web::Path<String>) -> Result<Json<V>, ApiError> {
    let started = std::time::Instant::now();
    let address = path.into_inner();
    let lookup = lookup(&state, &address).await?;
    let record = &lookup.record;
    let asn_value = record.pointer("/network/asn").cloned().unwrap_or(Value::Null);
    let data = json!({
        "address": record.get("address"),
        "ipVersion": record.get("ipVersion"),
        "asn": asn_value,
        "asnText": asn_value.as_i64().map(|v| format!("AS{v}")),
        "organization": record.pointer("/network/organization"),
        "isp": record.pointer("/network/isp"),
        "domain": record.pointer("/network/domain"),
    });
    Ok(Json(envelope(
        data,
        meta_json(lookup.cached, started.elapsed().as_millis() as u64, UPSTREAM_IPWHOIS, None),
    )))
}

/// `GET /ip-geolocation/validate/{address}` — computed locally, no upstream.
#[utoipa::path(get, path = "/ip-geolocation/validate/{address}", context_path = "/ip-geolocation",
    params(("address" = String, Path)),
    responses((status = 200, description = "Address facts"), (status = 400, description = "Invalid address")))]
async fn validate(path: web::Path<String>) -> Result<Json<V>, ApiError> {
    let started = std::time::Instant::now();
    let address = path.into_inner();
    let addr = parse_address(&address)?;
    let facts = classify_address(&addr);
    let data = json!({
        "address": address,
        "ipVersion": facts.ip_version,
        "canonical": facts.canonical,
        "hex": facts.hex,
        "scope": facts.scope,
        "isPublic": facts.is_public,
        "isLoopback": facts.is_loopback,
        "isPrivate": facts.is_private,
        "isLinkLocal": facts.is_link_local,
        "isMulticast": facts.is_multicast,
        "isUnspecified": facts.is_unspecified,
        "isBroadcast": facts.is_broadcast,
        "isCarrierGradeNat": facts.is_carrier_grade_nat,
        "isReserved": facts.is_reserved,
    });
    Ok(Json(envelope(
        data,
        meta_json(false, started.elapsed().as_millis() as u64, "local", None),
    )))
}

// ------------------------------------------------------------------ health

async fn health_report(state: &AppState) -> Value {
    let probe_key = "ipgeo_probe";
    let probe_cache = state.json_cache(cache_names::HEALTH_PROBES);
    let cached = probe_cache.get(probe_key).await;
    let (probe, was_cached) = match cached {
        Some(v) => (v.as_ref().clone(), true),
        None => {
            let url = format!("{UPSTREAM_IPWHOIS}/8.8.8.8");
            let report: Value = match httpx::get_json(state, &url, &[], CODE_PREFIX).await {
                Ok(t) => {
                    let ok = t.body.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
                    if ok {
                        json!({ "reachable": true, "probeAddress": t.body.get("ip") })
                    } else {
                        json!({ "reachable": false, "error": "Upstream reported success:false" })
                    }
                }
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
        "upstream": { "name": "ipwho.is", "probe": probe, "cached": was_cached },
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
}

/// `GET /ip-geolocation/health`
#[utoipa::path(get, path = "/ip-geolocation/health", context_path = "/ip-geolocation",
    responses((status = 200, description = "Liveness plus upstream probe")))]
async fn health(state: web::Data<AppState>) -> Json<V> {
    Json(health_report(&state).await)
}

/// `GET /ip-geolocation/health/ready`
#[utoipa::path(get, path = "/ip-geolocation/health/ready", context_path = "/ip-geolocation",
    responses((status = 200, description = "Upstream reachable"), (status = 503, description = "Upstream down")))]
async fn health_ready(state: web::Data<AppState>) -> Result<Json<V>, ApiError> {
    let report = health_report(&state).await;
    if report.get("upstream").and_then(|u| u.get("probe")).and_then(|p| p.get("reachable")).and_then(|v| v.as_bool()) != Some(true) {
        return Err(ApiError::unavailable(
            format!("{CODE_PREFIX}_UPSTREAM_UNAVAILABLE"),
            "The ipwho.is upstream is unreachable".to_string(),
        ));
    }
    Ok(Json(json!({
        "status": "ready",
        "upstream": "ipwho.is",
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })))
}

// ---------------------------------------------------------------- OpenAPI

#[derive(OpenApi)]
#[openapi(
    info(title = "ip-geolocation-api", version = "1.0.0", description = "IP geolocation, ASN/ISP intelligence and local address classification."),
    paths(ip_self, ip_lookup, geo, asn, validate, health, health_ready)
)]
pub struct IpGeoApi;

pub fn openapi_doc() -> utoipa::openapi::OpenApi {
    let mut doc = IpGeoApi::openapi();
    doc.info.title = "ip-geolocation-api".into();
    doc.servers = Some(vec![utoipa::openapi::Server::new("/ip-geolocation")]);
    doc
}
