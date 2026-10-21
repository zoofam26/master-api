//! USGS FDSN parsing: GeoJSON features → this service's Earthquake shape,
//! the summary aggregator, and the earthquake route handlers.

use std::sync::Arc;

use actix_web::web::{self, Json};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use utoipa::{IntoParams, OpenApi};

use master_common::error::ApiError;
use master_common::httpx;
use master_common::state::{cache_names, AppState};

use crate::aq;
use crate::{
    classify_magnitude, energy_joules, energy_proxy, magnitude_scale, round, CODE_PREFIX,
    DEFAULT_LIMIT, DEFAULT_WINDOW_DAYS, DEPENDENCY, EVENT_STATUSES, MAX_LIMIT, ORDER_BY,
    SERVICE_PREFIX, SUMMARY_SAMPLE_LIMIT, SUMMARY_TOP_REGIONS, TIEBREAKER, UPSTREAM_USGS,
};

// ------------------------------------------------------------------ model

fn null_or_str(v: Option<&Value>) -> Option<String> {
    v.and_then(|x| x.as_str()).map(String::from)
}

fn region_from_place(place: &Option<String>) -> Option<String> {
    place
        .as_deref()
        .and_then(|p| p.rsplit(',').next())
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
}

fn iso_instant(ms: Option<i64>) -> String {
    chrono::DateTime::from_timestamp_millis(ms.unwrap_or(0))
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

fn iso_instant_opt(ms: Option<&Value>) -> Option<String> {
    ms.and_then(|v| v.as_i64()).map(|ms| {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            .unwrap_or_default()
    })
}

/// GeoJSON feature → Earthquake record (all documented fields).
pub fn parse_feature(feature: &Value) -> Value {
    let props = feature.get("properties").cloned().unwrap_or(Value::Null);
    let geom = feature.get("geometry").cloned().unwrap_or(Value::Null);
    let id = feature.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let p = |k: &str| props.get(k);

    let mag_type = null_or_str(p("magType"));
    let magnitude = p("mag").and_then(|v| v.as_f64());
    let place = null_or_str(p("place"));
    let alert = null_or_str(p("alert"));
    let review = null_or_str(p("status")).unwrap_or_else(|| "unknown".into());
    let coords = geom
        .get("coordinates")
        .and_then(|v| v.as_array())
        .map(|a| {
            json!({
                "longitude": a.first().and_then(|x| x.as_f64()).unwrap_or(0.0),
                "latitude": a.get(1).and_then(|x| x.as_f64()).unwrap_or(0.0),
                "depthKm": a.get(2).and_then(|x| x.as_f64()),
            })
        })
        .unwrap_or_else(|| json!({ "longitude": 0.0, "latitude": 0.0, "depthKm": Value::Null }));

    json!({
        "id": id,
        "magnitude": magnitude,
        "magnitudeType": mag_type,
        "magnitudeScale": magnitude_scale(mag_type.as_deref()),
        "magnitudeCategory": classify_magnitude(magnitude),
        "place": place,
        "region": region_from_place(&place),
        "time": iso_instant(p("time").and_then(|v| v.as_i64())),
        "updated": iso_instant_opt(p("updated")),
        "depthKm": coords.get("depthKm").cloned().unwrap_or(Value::Null),
        "coordinates": { "longitude": coords.get("longitude"), "latitude": coords.get("latitude") },
        "tsunami": p("tsunami").and_then(|v| v.as_i64()).map(|v| v != 0).unwrap_or(false),
        "alert": alert,
        "significance": p("sig").and_then(|v| v.as_i64()),
        "feltReports": p("felt").and_then(|v| v.as_i64()),
        "cdi": p("cdi").and_then(|v| v.as_f64()),
        "mmi": p("mmi").and_then(|v| v.as_f64()),
        "reviewStatus": review,
        "eventType": null_or_str(p("type")),
        "network": null_or_str(p("net")),
        "code": null_or_str(p("code")),
        "title": null_or_str(p("title")),
        "url": null_or_str(p("url")),
        "detailUrl": null_or_str(p("detail")),
    })
}

pub fn is_valid_event_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !id.contains('/')
}

// ------------------------------------------------------------------ window

fn parse_instant_ms(v: &str) -> Option<i64> {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(v) {
        return Some(t.timestamp_millis());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
    }
    None
}

struct Window {
    starttime: String,
    endtime: String,
}

/// Resolve the query window, anchored to the start of the current minute so
/// the default (last N days) window produces a stable cache key.
fn resolve_window(
    starttime: &Option<String>,
    endtime: &Option<String>,
) -> Result<Window, ApiError> {
    let now_ms = (Utc::now().timestamp_millis() / 60_000) * 60_000;
    let end_ms = match endtime {
        Some(v) => parse_instant_ms(v).ok_or_else(|| {
            ApiError::validation(format!("{CODE_PREFIX}_INVALID_REQUEST"), "starttime and endtime must be parseable ISO-8601 values")
        })?,
        None => now_ms,
    };
    let start_ms = match starttime {
        Some(v) => parse_instant_ms(v).ok_or_else(|| {
            ApiError::validation(format!("{CODE_PREFIX}_INVALID_REQUEST"), "starttime and endtime must be parseable ISO-8601 values")
        })?,
        None => end_ms - DEFAULT_WINDOW_DAYS * 24 * 3600 * 1000,
    };
    if start_ms > end_ms {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "starttime must not be after endtime",
            json!({ "starttime": starttime, "endtime": endtime }),
        ));
    }
    let fmt = |ms: i64| {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            .unwrap_or_default()
    };
    Ok(Window { starttime: fmt(start_ms), endtime: fmt(end_ms) })
}

// ------------------------------------------------------------------ client

fn build_url(
    method: &str,
    params: &[(&str, String)],
    geojson: bool,
) -> String {
    let mut url = format!("{UPSTREAM_USGS}/{method}?");
    let mut all: Vec<(String, String)> = Vec::new();
    if geojson {
        all.push(("format".into(), "geojson".into()));
    }
    all.extend(params.iter().map(|(k, v)| (k.to_string(), v.clone())));
    url.push_str(
        &all.iter()
            .map(|(k, v)| format!("{}={}", k, crate::aq::enc(v)))
            .collect::<Vec<_>>()
            .join("&"),
    );
    url
}

async fn usgs_list(state: &AppState, params: Vec<(String, String)>) -> Result<Vec<Value>, ApiError> {
    let url = build_url("query", &params.iter().map(|(k, v)| (k.as_str(), v.clone())).collect::<Vec<_>>(), true);
    let body = httpx::get_json(state, &url, &[], CODE_PREFIX).await?.body;
    Ok(body
        .get("features")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(parse_feature).collect())
        .unwrap_or_default())
}

async fn usgs_count(state: &AppState, params: Vec<(String, String)>) -> Result<i64, ApiError> {
    // Without `format=geojson` the count endpoint returns plain text "N".
    let url = build_url("count", &params.iter().map(|(k, v)| (k.as_str(), v.clone())).collect::<Vec<_>>(), false);
    let started = std::time::Instant::now();
    let resp = state
        .http
        .get(&url)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                ApiError::timeout(format!("{CODE_PREFIX}_UPSTREAM_TIMEOUT"), "USGS count timed out".to_string())
            } else {
                ApiError::upstream(format!("{CODE_PREFIX}_UNREACHABLE"), format!("Could not reach USGS: {e}"))
            }
        })?;
    let _ = started;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(ApiError::from_upstream_status(status.as_u16(), CODE_PREFIX, &text));
    }
    let text = resp.text().await.unwrap_or_default();
    Ok(text.trim().parse::<i64>().unwrap_or(0))
}

// ------------------------------------------------------------------ query DTO

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EarthquakeQuery {
    /// ISO-8601 instant or date (default: endtime - 30 days).
    pub starttime: Option<String>,
    /// ISO-8601 instant or date (default: now, minute-anchored).
    pub endtime: Option<String>,
    pub minmagnitude: Option<f64>,
    pub maxmagnitude: Option<f64>,
    pub minlatitude: Option<f64>,
    pub minlongitude: Option<f64>,
    pub maxlatitude: Option<f64>,
    pub maxlongitude: Option<f64>,
    /// Shorthand rectangle `minLon,minLat,maxLon,maxLat`.
    pub bbox: Option<String>,
    /// `all`, `automatic`, `reviewed` or `deleted`.
    pub status: Option<String>,
    pub limit: Option<usize>,
    /// Page offset, starts at 1 like the upstream.
    pub offset: Option<usize>,
    /// One of the FDSN orderby facets.
    pub orderby: Option<String>,
    /// Stable sort with an id tiebreaker (default true).
    pub stable: Option<bool>,
    // summary-specific
    pub sample_limit: Option<usize>,
    pub top_regions: Option<usize>,
}

struct Filters {
    params: Vec<(String, String)>,
}

fn effective_status(status: &Option<String>) -> Result<String, ApiError> {
    match status {
        None => Ok("all".to_string()),
        Some(s) if EVENT_STATUSES.contains(&s.as_str()) => Ok(s.clone()),
        Some(s) => Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            format!("status must be one of: {}", EVENT_STATUSES.join(", ")),
            json!({ "status": s, "allowed": EVENT_STATUSES }),
        )),
    }
}

fn resolve_filters(query: &EarthquakeQuery) -> Result<(Filters, Window), ApiError> {
    let window = resolve_window(&query.starttime, &query.endtime)?;
    if let (Some(min), Some(max)) = (query.minmagnitude, query.maxmagnitude) {
        if min > max {
            return Err(ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_REQUEST"),
                "minmagnitude must not be greater than maxmagnitude",
                json!({ "minmagnitude": min, "maxmagnitude": max }),
            ));
        }
    }
    // bbox expands into the four rectangle parameters; both forms together are a 400.
    let mut rect: Vec<(String, f64)> = Vec::new();
    if let Some(bbox) = &query.bbox {
        let parts: Vec<&str> = bbox.split(',').map(str::trim).collect();
        if parts.len() != 4 || parts.iter().any(|p| p.parse::<f64>().is_err()) {
            return Err(ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_REQUEST"),
                "bbox must be four comma-separated numbers: minLon,minLat,maxLon,maxLat",
                json!({ "bbox": bbox }),
            ));
        }
        if query.minlatitude.is_some() || query.minlongitude.is_some() || query.maxlatitude.is_some() || query.maxlongitude.is_some() {
            return Err(ApiError::validation(
                format!("{CODE_PREFIX}_INVALID_REQUEST"),
                "bbox cannot be combined with minlatitude/minlongitude/maxlatitude/maxlongitude",
            ));
        }
        rect = vec![
            ("minlongitude".into(), parts[0].parse().unwrap()),
            ("minlatitude".into(), parts[1].parse().unwrap()),
            ("maxlongitude".into(), parts[2].parse().unwrap()),
            ("maxlatitude".into(), parts[3].parse().unwrap()),
        ];
    } else {
        if let Some(v) = query.minlatitude {
            rect.push(("minlatitude".into(), v));
        }
        if let Some(v) = query.minlongitude {
            rect.push(("minlongitude".into(), v));
        }
        if let Some(v) = query.maxlatitude {
            rect.push(("maxlatitude".into(), v));
        }
        if let Some(v) = query.maxlongitude {
            rect.push(("maxlongitude".into(), v));
        }
    }
    for (name, value) in &rect {
        if name.contains("latitude") && !(-90.0..=90.0).contains(value) {
            return Err(ApiError::validation(format!("{CODE_PREFIX}_INVALID_REQUEST"), "latitude must be between -90 and 90"));
        }
        if name.contains("longitude") && !(-180.0..=180.0).contains(value) {
            return Err(ApiError::validation(format!("{CODE_PREFIX}_INVALID_REQUEST"), "longitude must be between -180 and 180"));
        }
    }

    let status = effective_status(&query.status)?;
    let mut params: Vec<(String, String)> = vec![
        ("starttime".into(), window.starttime.clone()),
        ("endtime".into(), window.endtime.clone()),
    ];
    // USGS rejects `status=all` outright ("Unknown parameter"), and `all` is
    // the documented default, so the parameter is only sent when narrowed.
    if status != "all" {
        params.push(("status".into(), status.clone()));
    }
    if let Some(v) = query.minmagnitude {
        params.push(("minmagnitude".into(), v.to_string()));
    }
    if let Some(v) = query.maxmagnitude {
        params.push(("maxmagnitude".into(), v.to_string()));
    }
    params.extend(rect.into_iter().map(|(k, v)| (k, v.to_string())));
    Ok((Filters { params }, window))
}

fn resolve_order(query: &EarthquakeQuery) -> Result<String, ApiError> {
    match &query.orderby {
        None => Ok("time".into()),
        Some(o) if ORDER_BY.contains(&o.as_str()) => Ok(o.clone()),
        Some(o) => Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            format!("orderby must be one of: {}", ORDER_BY.join(", ")),
            json!({ "orderby": o, "allowed": ORDER_BY }),
        )),
    }
}

/// Stable sort with the id tiebreaker; a missing magnitude is pushed to the end.
fn sort_stably(items: &mut [Value], order_by: &str, enabled: bool) {
    if !enabled {
        return;
    }
    let descending = order_by == "time" || order_by == "magnitude" || order_by == "area";
    let by_magnitude = order_by == "magnitude" || order_by == "magnitude-asc";
    items.sort_by(|a, b| {
        if by_magnitude {
            let a_null = a.get("magnitude").map(|v| v.is_null()).unwrap_or(true);
            let b_null = b.get("magnitude").map(|v| v.is_null()).unwrap_or(true);
            if a_null != b_null {
                return if a_null { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less };
            }
        }
        let primary = match order_by {
            "time" | "time-asc" => {
                let ta = a.get("time").and_then(|v| v.as_str()).unwrap_or("");
                let tb = b.get("time").and_then(|v| v.as_str()).unwrap_or("");
                ta.cmp(tb)
            }
            "magnitude" | "magnitude-asc" => {
                let ma = a.get("magnitude").and_then(|v| v.as_f64());
                let mb = b.get("magnitude").and_then(|v| v.as_f64());
                match (ma, mb) {
                    (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
                    _ => std::cmp::Ordering::Equal,
                }
            }
            _ => {
                let ia = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let ib = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
                ia.cmp(ib)
            }
        };
        let ia = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let ib = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let full = primary.then_with(|| ia.cmp(ib));
        if descending { full.reverse() } else { full }
    });
}

// ------------------------------------------------------------------ summary

pub fn aggregate(earthquakes: &[Value], total: i64, sample_limit: usize, top_regions: usize) -> Value {
    let analysed = earthquakes.len();
    let mut known: Vec<f64> = Vec::new();
    let mut counts: Map<String, Value> = Map::new();
    let mut region_counts: std::collections::HashMap<String, (usize, Option<f64>)> = std::collections::HashMap::new();
    let mut alerts = json!({ "green": 0, "yellow": 0, "orange": 0, "red": 0 });
    let mut review = json!({ "automatic": 0, "reviewed": 0, "deleted": 0, "unknown": 0 });
    let mut without_region = 0usize;
    let mut tsunami_count = 0usize;
    let mut proxy_sum = 0f64;
    let mut joules_sum = 0f64;
    let mut strongest: Option<&Value> = None;

    for e in earthquakes {
        let cat = e.get("magnitudeCategory").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        let next = counts.get(&cat).and_then(|v| v.as_u64()).unwrap_or(0) + 1;
        counts.insert(cat, json!(next));

        let region = e.get("region").and_then(|v| v.as_str());
        match region {
            None => without_region += 1,
            Some(r) => {
                let entry = region_counts.entry(r.to_string()).or_insert((0, None));
                entry.0 += 1;
                let m = e.get("magnitude").and_then(|v| v.as_f64());
                entry.1 = match (entry.1, m) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (None, Some(b)) => Some(b),
                    (a, None) => a,
                };
            }
        }

        if e.get("tsunami").and_then(|v| v.as_bool()).unwrap_or(false) {
            tsunami_count += 1;
        }
        if let Some(alert) = e.get("alert").and_then(|v| v.as_str()) {
            if let Some(slot) = alerts.get_mut(alert) {
                *slot = json!(slot.as_u64().unwrap_or(0) + 1);
            }
        }
        let status = e.get("reviewStatus").and_then(|v| v.as_str()).unwrap_or("unknown");
        let status_key = if review.get(status).is_some() { status } else { "unknown" };
        if let Some(slot) = review.get_mut(status_key) {
            *slot = json!(slot.as_u64().unwrap_or(0) + 1);
        }

        if let Some(m) = e.get("magnitude").and_then(|v| v.as_f64()) {
            known.push(m);
            proxy_sum += energy_proxy(m);
            joules_sum += energy_joules(m);
            let better = strongest
                .map(|s| s.get("magnitude").and_then(|v| v.as_f64()).unwrap_or(f64::NEG_INFINITY) < m)
                .unwrap_or(true);
            if better {
                strongest = Some(e);
            }
        }
    }

    known.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = if known.is_empty() {
        None
    } else if known.len() % 2 == 1 {
        Some(known[known.len() / 2])
    } else {
        Some((known[known.len() / 2 - 1] + known[known.len() / 2]) / 2.0)
    };

    let mut buckets: Vec<Value> = crate::MAGNITUDE_BUCKETS
        .iter()
        .map(|(cat, label, min, max)| {
            let count = counts.get(*cat).and_then(|v| v.as_u64()).unwrap_or(0);
            json!({
                "category": cat,
                "label": label,
                "minInclusive": min,
                "maxExclusive": max,
                "count": count,
                "share": if analysed == 0 { 0.0 } else { round(count as f64 / analysed as f64, 4) },
            })
        })
        .collect();
    let unknown_count = counts.get("unknown").and_then(|v| v.as_u64()).unwrap_or(0);
    if unknown_count > 0 {
        buckets.push(json!({
            "category": "unknown", "label": "Unknown", "minInclusive": Value::Null, "maxExclusive": Value::Null,
            "count": unknown_count,
            "share": if analysed == 0 { 0.0 } else { round(unknown_count as f64 / analysed as f64, 4) },
        }));
    }

    let mut by_region: Vec<Value> = region_counts
        .iter()
        .map(|(region, (count, max_mag))| {
            json!({
                "region": region,
                "count": count,
                "maxMagnitude": max_mag,
                "share": if analysed == 0 { 0.0 } else { round(*count as f64 / analysed as f64, 4) },
            })
        })
        .collect();
    by_region.sort_by(|a, b| {
        let ca = a.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        let cb = b.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        cb.cmp(&ca).then_with(|| {
            let ra = a.get("region").and_then(|v| v.as_str()).unwrap_or("");
            let rb = b.get("region").and_then(|v| v.as_str()).unwrap_or("");
            ra.cmp(rb)
        })
    });
    by_region.truncate(top_regions.max(1));

    json!({
        "total": total,
        "analysed": analysed,
        "truncated": (analysed as i64) < total,
        "sampleLimit": sample_limit,
        "magnitudes": {
            "max": known.last(),
            "min": known.first(),
            "mean": if known.is_empty() { Value::Null } else { json!(round(known.iter().sum::<f64>() / known.len() as f64, 4)) },
            "median": median.map(|m| round(m, 4)),
            "known": known.len(),
        },
        "buckets": buckets,
        "byRegion": by_region,
        "withoutRegion": without_region,
        "energy": {
            "magnitudeProxy": round(proxy_sum, 3),
            "estimatedJoules": round(joules_sum, 3),
            "formula": "sum(10^(1.5 * M))",
            "note": "magnitudeProxy is a relative Gutenberg-Richter magnitude proxy, not a physical measurement. estimatedJoules applies log10(E_ergs) = 1.5M + 4.8 and is only as good as the magnitude type. Both are estimates and are summed over the analysed sample, not the whole window, when truncated is true.",
            "basedOn": if (analysed as i64) < total { "sample" } else { "analysed" },
        },
        "tsunamiCount": tsunami_count,
        "alerts": alerts,
        "reviewStatuses": review,
        "strongest": strongest.cloned(),
    })
}

// ------------------------------------------------------------------ routes

pub fn configure(cfg: &mut web::ServiceConfig) {
    let scope = web::scope(SERVICE_PREFIX)
        // Literal routes first so "air-quality" is never swallowed by "{id}".
        .route("/air-quality/aqi", web::get().to(crate::aq::aqi))
        .route("/air-quality", web::get().to(crate::aq::forecast))
        .route("/summary", web::get().to(summary))
        .route("/health/ready", web::get().to(health_ready))
        .route("/health", web::get().to(health))
        .route("/{id}", web::get().to(get_one))
        .route("", web::get().to(list))
        .route("/", web::get().to(list));
    cfg.service(scope);
}

fn requested_echo(starttime: &Option<String>, endtime: &Option<String>) -> Value {
    let mut echo = Map::new();
    if let Some(s) = starttime {
        echo.insert("starttime".into(), json!(s));
    }
    if let Some(e) = endtime {
        echo.insert("endtime".into(), json!(e));
    }
    Value::Object(echo)
}

/// `GET /earthquake` — filtered, paginated, stably sorted list.
#[utoipa::path(get, path = "/earthquake", context_path = "/earthquake", params(EarthquakeQuery),
    responses((status = 200, description = "Event list")))]
async fn list(
    state: web::Data<AppState>,
    query: web::Query<EarthquakeQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let (filters, window) = resolve_filters(&query)?;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let offset = query.offset.unwrap_or(1).max(1);
    let order_by = resolve_order(&query)?;
    let stable = query.stable.unwrap_or(true);

    let mut params = filters.params.clone();
    params.push(("limit".into(), limit.to_string()));
    params.push(("offset".into(), offset.to_string()));
    params.push(("orderby".into(), order_by.clone()));

    let mut earthquakes = usgs_list(&state, params).await?;
    sort_stably(&mut earthquakes, &order_by, stable);
    let has_more = !earthquakes.is_empty() && earthquakes.len() == limit && limit > 0;
    let count = earthquakes.len();

    let body = json!({
        "success": true,
        "data": { "earthquakes": earthquakes },
        "meta": {
            "count": count,
            "limit": limit,
            "offset": offset,
            "nextOffset": if has_more { json!(offset + limit) } else { Value::Null },
            "window": { "starttime": window.starttime, "endtime": window.endtime },
            "requested": requested_echo(&query.starttime, &query.endtime),
            "orderBy": order_by,
            "tiebreaker": TIEBREAKER,
            "upstream": UPSTREAM_USGS,
            "durationMs": started.elapsed().as_millis() as u64,
            "cached": false,
        },
    });
    Ok(Json(body))
}

/// `GET /earthquake/summary`
#[utoipa::path(get, path = "/earthquake/summary", context_path = "/earthquake", params(EarthquakeQuery),
    responses((status = 200, description = "Aggregate statistics")))]
async fn summary(
    state: web::Data<AppState>,
    query: web::Query<EarthquakeQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let (filters, window) = resolve_filters(&query)?;
    let sample_limit = query.sample_limit.unwrap_or(SUMMARY_SAMPLE_LIMIT).min(SUMMARY_SAMPLE_LIMIT);
    let top_regions = query.top_regions.unwrap_or(SUMMARY_TOP_REGIONS);

    let cache_key = format!(
        "summary:{:?}:{:?}:{:?}:{sample_limit}:{top_regions}",
        window.starttime, window.endtime, filters.params
    );
    let cache = state.json_cache(cache_names::EARTHQUAKE);
    if let Some(hit) = cache.get(&cache_key).await {
        let mut data = hit.as_ref().clone();
        data["window"] = json!({ "starttime": window.starttime, "endtime": window.endtime });
        return Ok(Json(json!({
            "success": true,
            "data": data,
            "meta": {
                "requested": requested_echo(&query.starttime, &query.endtime),
                "upstream": [UPSTREAM_USGS, format!("{UPSTREAM_USGS}/count")],
                "durationMs": started.elapsed().as_millis() as u64,
                "cached": true,
            },
        })));
    }

    // One /query supplies the sample; one /count supplies the exact total.
    let mut list_params = filters.params.clone();
    list_params.push(("limit".into(), sample_limit.to_string()));
    list_params.push(("offset".into(), "1".into()));
    list_params.push(("orderby".into(), "time".into()));
    let mut count_params = filters.params.clone();
    count_params.push(("starttime".into(), window.starttime.clone()));
    count_params.push(("endtime".into(), window.endtime.clone()));

    let (earthquakes, total) = tokio::join!(
        usgs_list(&state, list_params),
        usgs_count(&state, count_params)
    );
    let earthquakes = earthquakes?;
    let total = total?;
    let mut aggregates = aggregate(&earthquakes, total, sample_limit, top_regions);
    aggregates["window"] = json!({ "starttime": window.starttime, "endtime": window.endtime });
    cache.insert(cache_key, Arc::new(aggregates.clone())).await;

    let body = json!({
        "success": true,
        "data": aggregates,
        "meta": {
            "requested": requested_echo(&query.starttime, &query.endtime),
            "upstream": [UPSTREAM_USGS, format!("{UPSTREAM_USGS}/count")],
            "durationMs": started.elapsed().as_millis() as u64,
            "cached": false,
        },
    });
    Ok(Json(body))
}

/// `GET /earthquake/{id}`
#[utoipa::path(get, path = "/earthquake/{id}", context_path = "/earthquake",
    params(("id" = String, Path, description = "USGS event id, e.g. us6000txtc")),
    responses((status = 200, description = "One event"), (status = 404, description = "Unknown event")))]
async fn get_one(state: web::Data<AppState>, path: web::Path<String>) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let id = path.into_inner();
    if !is_valid_event_id(&id) {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "eventId must be 1-64 alphanumeric characters (dashes and underscores allowed)",
            json!({ "eventId": id }),
        ));
    }

    let cache = state.json_cache(cache_names::EARTHQUAKE);
    let cache_key = format!("event:{id}");
    let (earthquake, cached): (Value, bool) = match cache.get(&cache_key).await {
        Some(v) => (v.as_ref().clone(), true),
        None => {
            let url = build_url("query", &[("eventid", id.clone())], true);
            let body = httpx::get_json(&state, &url, &[], CODE_PREFIX).await?.body;
            // USGS answers eventid queries with a bare Feature (not a collection).
            let feature = if body.get("type").and_then(|v| v.as_str()) == Some("Feature") {
                Some(body.clone())
            } else {
                body.get("features")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .cloned()
            };
            let parsed = parse_feature(&feature.ok_or_else(|| {
                ApiError::not_found_details(
                    format!("{CODE_PREFIX}_NOT_FOUND"),
                    format!("No earthquake with id \"{id}\""),
                    json!({ "eventId": id, "dependency": DEPENDENCY }),
                )
            })?);
            cache.insert(cache_key, Arc::new(parsed.clone())).await;
            (parsed, false)
        }
    };

    let body = json!({
        "success": true,
        "data": { "earthquake": earthquake },
        "meta": {
            "upstream": UPSTREAM_USGS,
            "durationMs": started.elapsed().as_millis() as u64,
            "cached": cached,
        },
    });
    Ok(Json(body))
}

// ------------------------------------------------------------------ health

async fn health_report(state: &AppState) -> Value {
    let probe_key = "earthquake_probe";
    let probe_cache = state.json_cache(cache_names::HEALTH_PROBES);
    let cached = probe_cache.get(probe_key).await;
    let (probe, was_cached) = match cached {
        Some(v) => (v.as_ref().clone(), true),
        None => {
            // One byte-sized count over the last five minutes.
            let end = (Utc::now().timestamp_millis() / 60_000) * 60_000;
            let start = end - 5 * 60 * 1000;
            let fmt = |ms: i64| {
                chrono::DateTime::from_timestamp_millis(ms)
                    .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
                    .unwrap_or_default()
            };
            let report: Value = match usgs_count(
                state,
                vec![
                    ("starttime".into(), fmt(start)),
                    ("endtime".into(), fmt(end)),
                ],
            )
            .await
            {
                Ok(count) => json!({ "reachable": true, "eventsLast5Min": count }),
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
        "upstream": { "provider": DEPENDENCY, "endpoint": UPSTREAM_USGS, "probe": probe, "cached": was_cached },
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
}

/// `GET /earthquake/health`
#[utoipa::path(get, path = "/earthquake/health", context_path = "/earthquake",
    responses((status = 200, description = "Liveness plus upstream probe")))]
async fn health(state: web::Data<AppState>) -> Json<Value> {
    Json(health_report(&state).await)
}

/// `GET /earthquake/health/ready`
#[utoipa::path(get, path = "/earthquake/health/ready", context_path = "/earthquake",
    responses((status = 200, description = "Upstream reachable"), (status = 503, description = "Upstream down")))]
async fn health_ready(state: web::Data<AppState>) -> Result<Json<Value>, ApiError> {
    let report = health_report(&state).await;
    if report.get("upstream").and_then(|u| u.get("probe")).and_then(|p| p.get("reachable")).and_then(|v| v.as_bool()) != Some(true) {
        return Err(ApiError::unavailable(
            format!("{CODE_PREFIX}_UPSTREAM_UNAVAILABLE"),
            "The USGS upstream is unreachable".to_string(),
        ));
    }
    Ok(Json(json!({
        "status": "ready",
        "upstream": UPSTREAM_USGS,
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })))
}

// ---------------------------------------------------------------- OpenAPI

#[derive(OpenApi)]
#[openapi(
    info(title = "earthquake-api", version = "1.0.0", description = "Earthquake queries over USGS FDSN plus Open-Meteo air quality."),
    paths(list, summary, get_one, health, health_ready, aq::aqi, aq::forecast)
)]
pub struct EarthquakeApi;

pub fn openapi_doc() -> utoipa::openapi::OpenApi {
    let mut doc = EarthquakeApi::openapi();
    doc.info.title = "earthquake-api".into();
    doc.servers = Some(vec![utoipa::openapi::Server::new(SERVICE_PREFIX)]);
    doc
}
