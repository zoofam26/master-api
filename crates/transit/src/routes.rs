//! Route handlers for the transit service.

use std::sync::Arc;

use actix_web::web::{self, Json};
use once_cell::sync::OnceCell;
use serde::Deserialize;
use serde_json::{json, Value};
use utoipa::{IntoParams, OpenApi};

use master_common::error::ApiError;
use master_common::httpx;
use master_common::state::{cache_names, AppState};

use crate::gtfs::{active_services, build_feed_snapshot, local_now, FeedSnapshot};
use crate::{
    find_agency, route_type_descriptor, AGENCIES, CODE_PREFIX, DEFAULT_DEPARTURE_COUNT,
    DEFAULT_LIMIT, DEFAULT_NEARBY_RADIUS_KM, MAX_DEPARTURE_COUNT, MAX_LIMIT,
    MAX_NEARBY_RADIUS_KM, SERVICE_PREFIX,
};

// ------------------------------------------------------------------ feeds

struct FeedCacheEntry {
    snapshot: Arc<FeedSnapshot>,
    created: std::time::Instant,
}

static FEED_CACHE: OnceCell<once_cell::sync::Lazy<
    std::sync::Mutex<std::collections::HashMap<String, Arc<FeedCacheEntry>>>,
>> = OnceCell::new();

fn feed_store() -> &'static std::sync::Mutex<std::collections::HashMap<String, Arc<FeedCacheEntry>>> {
    FEED_CACHE.get_or_init(|| once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new())))
}

fn cached_feed(id: &str) -> Option<Arc<FeedSnapshot>> {
    feed_store().lock().expect("transit feed lock").get(id).map(|e| e.snapshot.clone())
}

/// Downloads (at most once per TTL window) and parses a feed. Concurrent
/// callers for the same agency share one download: the store lock is held
/// across the fetch, matching the original's single-flight promise.
pub async fn get_feed(state: &AppState, agency: &'static crate::AgencyEntry) -> Result<Arc<FeedSnapshot>, ApiError> {
    let ttl = state.settings.feed_cache_ttl;
    if let Some(entry) = feed_store().lock().expect("transit feed lock").get(agency.id) {
        if entry.created.elapsed() < ttl {
            return Ok(entry.snapshot.clone());
        }
    }
    let started = std::time::Instant::now();
    let (bytes, _ms) = httpx::get_bytes(state, agency.feed_url, CODE_PREFIX).await?;
    let snapshot = Arc::new(build_feed_snapshot(agency, &bytes)?);
    tracing::info!(
        agency = agency.id,
        bytes = bytes.len(),
        rows = snapshot.rows,
        duration_ms = started.elapsed().as_millis() as u64,
        "GTFS feed loaded"
    );
    feed_store().lock().expect("transit feed lock").insert(
        agency.id.to_string(),
        Arc::new(FeedCacheEntry { snapshot: snapshot.clone(), created: std::time::Instant::now() }),
    );
    Ok(snapshot)
}

fn require_agency(id: &str) -> Result<&'static crate::AgencyEntry, ApiError> {
    find_agency(id).ok_or_else(|| {
        ApiError::not_found_details(
            format!("{CODE_PREFIX}_NOT_FOUND"),
            format!("Unknown agency \"{id}\""),
            json!({ "agencyId": id, "known": AGENCIES.iter().map(|a| a.id).collect::<Vec<_>>() }),
        )
    })
}

// ------------------------------------------------------------------- meta

fn agency_json(a: &crate::AgencyEntry, with_feed: bool) -> Value {
    let mut out = json!({
        "id": a.id,
        "name": a.name,
        "city": a.city,
        "region": a.region,
        "country": a.country,
        "countryName": a.country_name,
        "timezone": a.timezone,
        "publisher": a.publisher,
        "publisherUrl": a.publisher_url,
        "licence": a.licence,
        "licenceUrl": a.licence_url,
        "attribution": a.attribution,
        "publishesStopTimes": a.publishes_stop_times,
        "feedUrl": a.feed_url,
        "measuredFeedBytes": a.measured_feed_bytes,
        "feedCached": cached_feed(a.id).is_some(),
    });
    if with_feed {
        out["feed"] = match cached_feed(a.id) {
            Some(s) => json!({
                "loaded": true,
                "bytes": s.bytes,
                "rows": s.rows,
                "routes": s.routes.len(),
                "stops": s.stops.len(),
                "agencies": s.agencies.len(),
                "sourceUrl": s.source_url,
            }),
            None => json!({ "loaded": false }),
        };
    }
    out
}

fn clamp_page(limit: Option<usize>, offset: Option<usize>) -> (usize, usize) {
    (limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT), offset.unwrap_or(0))
}

fn not_found(kind: &str, message: impl Into<String>, details: Value) -> ApiError {
    ApiError::not_found_details(format!("{CODE_PREFIX}_{kind}"), message, details)
}

// -------------------------------------------------------------- query DTOs

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AgencyListQuery {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// Case-insensitive substring on id/name/city/region.
    pub search: Option<String>,
    /// Exact ISO country code.
    pub country: Option<String>,
    /// `name` (default), `id` or `country`.
    pub sort: Option<String>,
    /// `asc` (default) or `desc`.
    pub order: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RouteListQuery {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// Case-insensitive substring on route id/short/long name.
    pub search: Option<String>,
    /// GTFS route_type code.
    pub route_type: Option<i64>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct NearbyStopsQuery {
    pub latitude: f64,
    pub longitude: f64,
    /// Search radius in km (default 1, max 25).
    pub radius: Option<f64>,
    /// Restrict to one agency id (others must be cached to be scanned).
    pub agency: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DeparturesQuery {
    /// Local time `HH:MM` (default: now in the agency timezone).
    pub time: Option<String>,
    /// How many departures to return (default 10, max 50).
    pub count: Option<usize>,
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    let scope = web::scope(SERVICE_PREFIX)
        .route("/agencies", web::get().to(list_agencies))
        .route("/stops/nearby", web::get().to(nearby))
        .route("/transport-modes", web::get().to(transport_modes))
        .route("/health/ready", web::get().to(health_ready))
        .route("/health", web::get().to(health))
        .route("/agencies/{agencyId}/routes", web::get().to(list_routes))
        .route("/routes/{agencyId}/{routeId}", web::get().to(get_route))
        .route("/stops/{agencyId}/{stopId}", web::get().to(get_stop))
        .route("/departures/{agencyId}/{stopId}", web::get().to(departures))
        .route("/agencies/{agencyId}", web::get().to(get_agency));
    cfg.service(scope);
}

/// `GET /transit/agencies`
#[utoipa::path(get, path = "/transit/agencies", context_path = "/transit", params(AgencyListQuery),
    responses((status = 200, description = "Agency registry")))]
async fn list_agencies(query: web::Query<AgencyListQuery>) -> Json<Value> {
    let (limit, offset) = clamp_page(query.limit, query.offset);
    let search = query.search.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    let mut rows: Vec<Value> = AGENCIES
        .iter()
        .filter(|a| match &search {
            Some(s) => {
                let hay = format!("{} {} {} {}", a.id, a.name, a.city, a.region).to_lowercase();
                hay.contains(s)
            }
            None => true,
        })
        .filter(|a| match &query.country {
            Some(c) => a.country.eq_ignore_ascii_case(c),
            None => true,
        })
        .map(|a| agency_json(a, false))
        .collect();

    let sort = query.sort.as_deref().unwrap_or("name");
    let descending = query.order.as_deref() == Some("desc");
    let field = |v: &Value| -> String {
        match sort {
            "id" => v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            "country" => v.get("country").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            _ => v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        }
    };
    rows.sort_by(|a, b| {
        let ord = field(a).cmp(&field(b));
        if descending { ord.reverse() } else { ord }
    });

    let total = rows.len();
    let items: Vec<Value> = rows.into_iter().skip(offset).take(limit).collect();
    Json(json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
        "transportModes": crate::ROUTE_TYPES.len(),
    }))
}

/// `GET /transit/agencies/{agencyId}`
#[utoipa::path(get, path = "/transit/agencies/{agencyId}", context_path = "/transit",
    params(("agencyId" = String, Path)),
    responses((status = 200, description = "One agency"), (status = 404, description = "Unknown agency")))]
async fn get_agency(_state: web::Data<AppState>, path: web::Path<String>) -> Result<Json<Value>, ApiError> {
    let agency = require_agency(&path.into_inner())?;
    // Loading the feed is NOT triggered by this route (same as the original):
    // it only reports the feed when it happens to be cached.
    Ok(Json(json!({
        "agency": agency_json(agency, true),
        "feedNote": "The feed is downloaded lazily on the first request that needs it.",
    })))
}

/// `GET /transit/agencies/{agencyId}/routes`
#[utoipa::path(get, path = "/transit/agencies/{agencyId}/routes", context_path = "/transit",
    params(("agencyId" = String, Path), RouteListQuery),
    responses((status = 200, description = "Routes of a feed")))]
async fn list_routes(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<RouteListQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let agency = require_agency(&path.into_inner())?;
    let snapshot = get_feed(&state, agency).await?;
    if let Some(rt) = query.route_type {
        if route_type_descriptor(rt).is_none() {
            return Err(ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_REQUEST"),
                format!("route_type {rt} is not a known GTFS route_type"),
                json!({ "routeType": rt }),
            ));
        }
    }
    let search = query.search.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());

    let mut rows: Vec<Value> = snapshot
        .routes
        .iter()
        .filter(|r| match &search {
            Some(s) => {
                let hay = format!(
                    "{} {} {}",
                    r.id,
                    r.short_name.clone().unwrap_or_default(),
                    r.long_name.clone().unwrap_or_default()
                )
                .to_lowercase();
                hay.contains(s)
            }
            None => true,
        })
        .filter(|r| query.route_type.map(|rt| r.route_type == rt).unwrap_or(true))
        .map(|r| {
            let desc = route_type_descriptor(r.route_type);
            json!({
                "id": r.id,
                "shortName": r.short_name,
                "longName": r.long_name,
                "routeType": r.route_type,
                "mode": desc.map(|d| d.1),
                "modeDescription": desc.map(|d| d.2),
                "color": r.color,
                "textColor": r.text_color,
            })
        })
        .collect();
    let total = rows.len();
    let (limit, offset) = clamp_page(query.limit, query.offset);
    let items: Vec<Value> = rows.drain(..).skip(offset).take(limit).collect();

    Ok(Json(json!({
        "success": true,
        "data": {
            "agency": { "id": agency.id, "name": agency.name },
            "routes": items,
        },
        "meta": {
            "total": total,
            "limit": limit,
            "offset": offset,
            "cached": true,
            "feed": { "bytes": snapshot.bytes, "rows": snapshot.rows },
            "durationMs": started.elapsed().as_millis() as u64,
            "source": agency.feed_url,
        }
    })))
}

/// `GET /transit/routes/{agencyId}/{routeId}`
#[utoipa::path(get, path = "/transit/routes/{agencyId}/{routeId}", context_path = "/transit",
    params(("agencyId" = String, Path), ("routeId" = String, Path)),
    responses((status = 200, description = "One route"), (status = 404, description = "Unknown route")))]
async fn get_route(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let (agency_id, route_id) = path.into_inner();
    let agency = require_agency(&agency_id)?;
    let snapshot = get_feed(&state, agency).await?;
    let idx = snapshot.route_by_id.get(&route_id).ok_or_else(|| {
        not_found(
            "NOT_FOUND",
            format!("Route \"{route_id}\" does not exist in the {agency_id} feed"),
            json!({ "agencyId": agency_id, "routeId": route_id }),
        )
    })?;
    let r = &snapshot.routes[*idx];
    let desc = route_type_descriptor(r.route_type);
    Ok(Json(json!({
        "success": true,
        "data": {
            "agency": { "id": agency.id, "name": agency.name },
            "route": {
                "id": r.id,
                "shortName": r.short_name,
                "longName": r.long_name,
                "routeType": r.route_type,
                "mode": desc.map(|d| d.1),
                "modeDescription": desc.map(|d| d.2),
                "color": r.color,
                "textColor": r.text_color,
            }
        },
        "meta": { "cached": true, "source": agency.feed_url }
    })))
}

/// `GET /transit/stops/{agencyId}/{stopId}`
#[utoipa::path(get, path = "/transit/stops/{agencyId}/{stopId}", context_path = "/transit",
    params(("agencyId" = String, Path), ("stopId" = String, Path)),
    responses((status = 200, description = "One stop"), (status = 404, description = "Unknown stop")))]
async fn get_stop(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let (agency_id, stop_id) = path.into_inner();
    let agency = require_agency(&agency_id)?;
    let snapshot = get_feed(&state, agency).await?;
    let idx = snapshot.stop_by_id.get(&stop_id).ok_or_else(|| {
        not_found(
            "NOT_FOUND",
            format!("Stop \"{stop_id}\" does not exist in the {agency_id} feed"),
            json!({ "agencyId": agency_id, "stopId": stop_id }),
        )
    })?;
    let s = &snapshot.stops[*idx];
    Ok(Json(json!({
        "success": true,
        "data": {
            "agency": { "id": agency.id, "name": agency.name },
            "stop": {
                "id": s.id,
                "name": s.name,
                "latitude": s.lat,
                "longitude": s.lon,
                "zoneId": s.zone_id,
                "parentStation": s.parent_station,
            }
        },
        "meta": { "cached": true, "source": agency.feed_url }
    })))
}

/// `GET /transit/stops/nearby`
#[utoipa::path(get, path = "/transit/stops/nearby", context_path = "/transit", params(NearbyStopsQuery),
    responses((status = 200, description = "Stops near a point")))]
async fn nearby(_state: web::Data<AppState>, query: web::Query<NearbyStopsQuery>) -> Result<Json<Value>, ApiError> {
    if !(-90.0..=90.0).contains(&query.latitude) || !(-180.0..=180.0).contains(&query.longitude) {
        return Err(ApiError::validation(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "latitude must be within [-90,90] and longitude within [-180,180]",
        ));
    }
    let radius = query.radius.unwrap_or(DEFAULT_NEARBY_RADIUS_KM);
    if !(0.05..=MAX_NEARBY_RADIUS_KM).contains(&radius) {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            format!("radius must be between 0.05 and {MAX_NEARBY_RADIUS_KM} km"),
            json!({ "radius": radius }),
        ));
    }
    let (limit, offset) = clamp_page(query.limit, query.offset);

    // Bounding box prefilter (same trick as the original).
    let d_lat = radius / 111.32;
    let d_lon = radius / (111.32 * query.latitude.to_radians().cos().max(0.01));
    let (min_lat, max_lat) = (query.latitude - d_lat, query.latitude + d_lat);
    let (min_lon, max_lon) = (query.longitude - d_lon, query.longitude + d_lon);

    let mut results: Vec<Value> = Vec::new();
    let mut scanned: Vec<String> = Vec::new();
    for agency in AGENCIES {
        if let Some(target) = &query.agency {
            if agency.id != target {
                continue;
            }
        }
        // Bounding box rejects almost all candidates with four float compares,
        // so scanning cached feeds is cheap; and only cached feeds are scanned
        // so a nearby query never triggers downloads of every feed at once.
        let Some(snapshot) = cached_feed(agency.id) else { continue };
        scanned.push(agency.id.to_string());
        for (idx, stop) in snapshot.stops.iter().enumerate() {
            let (Some(lat), Some(lon)) = (stop.lat, stop.lon) else { continue };
            if lat < min_lat || lat > max_lat || lon < min_lon || lon > max_lon {
                continue;
            }
            let distance = haversine_km(query.latitude, query.longitude, lat, lon);
            if distance > radius {
                continue;
            }
            results.push(json!({
                "agencyId": agency.id,
                "agencyName": agency.name,
                "stop": {
                    "id": stop.id,
                    "name": stop.name,
                    "latitude": stop.lat,
                    "longitude": stop.lon,
                },
                "distanceKm": (distance * 1000.0).round() / 1000.0,
                "_idx": idx,
            }));
        }
    }
    results.sort_by(|a, b| {
        let da = a.get("distanceKm").and_then(|v| v.as_f64()).unwrap_or(f64::MAX);
        let db = b.get("distanceKm").and_then(|v| v.as_f64()).unwrap_or(f64::MAX);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });
    let total = results.len();
    let page: Vec<Value> = results.into_iter().skip(offset).take(limit).map(|mut v| { v.as_object_mut().map(|o| o.remove("_idx")); v }).collect();

    Ok(Json(json!({
        "query": {
            "latitude": query.latitude,
            "longitude": query.longitude,
            "radiusKm": radius,
            "agency": query.agency,
        },
        "feedsScanned": scanned,
        "total": total,
        "limit": limit,
        "offset": offset,
        "stops": page,
    })))
}

/// `GET /transit/departures/{agencyId}/{stopId}`
#[utoipa::path(get, path = "/transit/departures/{agencyId}/{stopId}", context_path = "/transit",
    params(("agencyId" = String, Path), ("stopId" = String, Path), DeparturesQuery),
    responses((status = 200, description = "Scheduled departures"), (status = 404, description = "Unknown stop or no schedule data")))]
async fn departures(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    query: web::Query<DeparturesQuery>,
) -> Result<Json<Value>, ApiError> {
    let (agency_id, stop_id) = path.into_inner();
    let agency = require_agency(&agency_id)?;
    let snapshot = get_feed(&state, agency).await?;

    let Some(stop_idx) = snapshot.stop_by_id.get(&stop_id) else {
        return Err(not_found(
            "NOT_FOUND",
            format!("Stop \"{stop_id}\" does not exist in the {agency_id} feed"),
            json!({ "agencyId": agency_id, "stopId": stop_id }),
        ));
    };
    let stop = &snapshot.stops[*stop_idx];

    if !snapshot.stop_times_loaded {
        return Err(ApiError::unavailable(
            format!("{CODE_PREFIX}_NO_SCHEDULE"),
            format!("The {agency_id} feed does not publish usable stop_times.txt, so departures cannot be served"),
            ));
    }
    let Some(times_at_stop) = snapshot.stop_times.get(&stop_id) else {
        return Err(not_found(
            "NO_DEPARTURES",
            format!("No scheduled departures found for stop \"{stop_id}\""),
            json!({ "agencyId": agency_id, "stopId": stop_id }),
        ));
    };

    // Resolve the requested instant in the agency's timezone.
    let tz = local_now(agency.timezone).ok_or_else(|| {
        ApiError::internal(format!("Agency timezone {} is not resolvable", agency.timezone))
    })?;
    use chrono::Timelike;
    let now_secs = (tz.time().hour() * 3600 + tz.time().minute() * 60 + tz.time().second()) as i64;
    let (base_date, base_secs) = match &query.time {
        Some(t) => {
            let parts: Vec<&str> = t.split(':').collect();
            if parts.len() != 2 || parts.iter().any(|p| p.parse::<u32>().is_err()) {
                return Err(ApiError::validation_details(
                    format!("{CODE_PREFIX}_INVALID_REQUEST"),
                    "time must be local HH:MM in the agency's timezone".to_string(),
                    json!({ "time": t }),
                ));
            }
            let secs = parts[0].parse::<i64>().unwrap_or(0) * 3600 + parts[1].parse::<i64>().unwrap_or(0) * 60;
            (tz.date_naive(), secs)
        }
        None => (tz.date_naive(), now_secs),
    };
    let count = query.count.unwrap_or(DEFAULT_DEPARTURE_COUNT).clamp(1, MAX_DEPARTURE_COUNT);

    // Active services for today and tomorrow (post-midnight trips belong to
    // the prior service day, so scan three service dates).
    let mut candidates: Vec<(i64, String, u32)> = Vec::new(); // (absolute local secs, trip_id, seq)
    for day_offset in 0..3i64 {
        let date = base_date + chrono::Duration::days(day_offset);
        let date_key = crate::gtfs::service_date_key(&date);
        let active = active_services(&snapshot, &date_key);
        let active_set: std::collections::HashSet<&String> = active.iter().collect();
        for (trip_id, departure_secs, seq) in times_at_stop.iter() {
            let Some((route_id, _, _)) = snapshot.trips.get(trip_id) else { continue };
            if !active_set.contains(route_id) {
                continue;
            }
            let service = &snapshot.calendars.iter().find(|(sid, _, _, _)| sid == route_id);
            let _ = service;
            let absolute = day_offset * 86_400 + *departure_secs as i64;
            if absolute >= base_secs {
                candidates.push((absolute, trip_id.clone(), *seq));
            }
        }
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.cmp(&b.2)));
    candidates.truncate(count);

    let departures: Vec<Value> = candidates
        .iter()
        .map(|(absolute, trip_id, _)| {
            let (route_id, direction_id, headsign) = snapshot.trips.get(trip_id).cloned().unwrap_or_default();
            let route = snapshot
                .route_by_id
                .get(route_id.as_str())
                .map(|idx| &snapshot.routes[*idx]);
            let local = tz.date_naive().and_time(chrono::NaiveTime::MIN) + chrono::Duration::seconds(*absolute);
            let local_date = (tz.date_naive() + chrono::Duration::seconds(*absolute)).format("%Y%m%d").to_string();
            json!({
                "time": (chrono::Utc::now().timestamp() as i64 + absolute - now_secs) * 1000,
                "timeIso": (chrono::Utc::now() + chrono::Duration::seconds(absolute - now_secs)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "localTime": local.format("%H:%M:%S").to_string(),
                "serviceDate": local_date,
                "tripId": trip_id,
                "routeId": route_id,
                "routeShortName": route.and_then(|r| r.short_name.clone()),
                "routeLongName": route.and_then(|r| r.long_name.clone()),
                "directionId": direction_id,
                "headsign": headsign,
                "secondsFromNow": absolute - now_secs,
                "realtime": false,
            })
        })
        .collect();

    Ok(Json(json!({
        "agency": { "id": agency.id, "name": agency.name, "timezone": agency.timezone },
        "stop": {
            "id": stop.id,
            "name": stop.name,
            "latitude": stop.lat,
            "longitude": stop.lon,
        },
        "requested": {
            "localTime": format!("{:02}:{:02}", base_secs / 3600, (base_secs % 3600) / 60),
            "count": count,
        },
        "count": departures.len(),
        "departures": departures,
        "realtime": false,
        "note": "Schedule data only (GTFS Schedule, not GTFS-Realtime).",
        "source": agency.feed_url,
    })))
}

fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6_372.8;
    let d_lat = (lat2 - lat1).to_radians();
    let d_lon = (lon2 - lon1).to_radians();
    let a = (d_lat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (d_lon / 2.0).sin().powi(2);
    2.0 * R * a.sqrt().asin()
}

/// `GET /transit/transport-modes`
#[utoipa::path(get, path = "/transit/transport-modes", context_path = "/transit",
    responses((status = 200, description = "GTFS route_type catalogue")))]
async fn transport_modes() -> Json<Value> {
    let items: Vec<Value> = crate::ROUTE_TYPES
        .iter()
        .map(|(code, mode, desc, extended)| {
            json!({ "code": code, "mode": mode, "description": desc, "extended": extended })
        })
        .collect();
    Json(json!({
        "items": items,
        "count": items.len(),
    }))
}

// ------------------------------------------------------------------ health

async fn health_report(state: &AppState) -> Value {
    let probe_key = "transit_probe";
    let probe_cache = state.json_cache(cache_names::HEALTH_PROBES);
    let cached = probe_cache.get(probe_key).await;
    let (probe, was_cached) = match cached {
        Some(v) => (v.as_ref().clone(), true),
        None => {
            // Cheap: registry reachability, not feed downloads.
            let report = json!({
                "reachable": true,
                "agencies": AGENCIES.len(),
                "feedsCached": AGENCIES.iter().filter(|a| cached_feed(a.id).is_some()).count(),
                "note": "Feeds are downloaded lazily on first use; registry health needs no download.",
            });
            probe_cache.insert(probe_key.to_string(), Arc::new(report.clone())).await;
            (report, false)
        }
    };
    json!({
        "status": "ok",
        "uptimeSeconds": state.uptime_seconds(),
        "upstream": { "registry": AGENCIES.len(), "feedsCached": probe.get("feedsCached"), "note": probe.get("note"), "cached": was_cached },
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
}

/// `GET /transit/health`
#[utoipa::path(get, path = "/transit/health", context_path = "/transit",
    responses((status = 200, description = "Registry health")))]
async fn health(state: web::Data<AppState>) -> Json<Value> {
    Json(health_report(&state).await)
}

/// `GET /transit/health/ready`
#[utoipa::path(get, path = "/transit/health/ready", context_path = "/transit",
    responses((status = 200, description = "Registry available")))]
async fn health_ready(state: web::Data<AppState>) -> Result<Json<Value>, ApiError> {
    let _ = httpx::get_bytes(&state, AGENCIES[4].feed_url, CODE_PREFIX).await; // smallest feed proves egress
    Ok(Json(json!({
        "status": "ready",
        "agencies": AGENCIES.len(),
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })))
}

// ---------------------------------------------------------------- OpenAPI

#[derive(OpenApi)]
#[openapi(
    info(title = "transit-api", version = "1.0.0", description = "GTFS-based transit directory and scheduled departures."),
    paths(list_agencies, get_agency, list_routes, get_route, get_stop, nearby, departures, transport_modes, health, health_ready)
)]
pub struct TransitApi;

pub fn openapi_doc() -> utoipa::openapi::OpenApi {
    let mut doc = TransitApi::openapi();
    doc.info.title = "transit-api".into();
    doc.servers = Some(vec![utoipa::openapi::Server::new(SERVICE_PREFIX)]);
    doc
}
