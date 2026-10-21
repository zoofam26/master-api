//! GTFS feed parsing: zip → typed tables, service-day resolution and
//! departure planning. Ported from `transit-api/src/transit/parsers/*`.

use std::collections::HashMap;
use std::io::Read;

use chrono::Datelike;
use chrono_tz::Tz;
use serde_json::{json, Value};

use master_common::error::ApiError;

use crate::CODE_PREFIX;

/// Parsed GTFS tables (only what the routes need).
pub struct FeedSnapshot {
    #[allow(dead_code)]
    pub agency_name: Option<String>,
    pub agencies: Vec<Value>,
    pub routes: Vec<RouteRow>,
    pub route_by_id: HashMap<String, usize>,
    pub stops: Vec<StopRow>,
    pub stop_by_id: HashMap<String, usize>,
    /// trip_id → (route_id, direction_id, headsign)
    pub trips: HashMap<String, (String, Option<i64>, Option<String>)>,
    /// (service_id) → active on weekday at index (0=Mon..6=Sun), start, end
    pub calendars: Vec<(String, [bool; 7], String, String)>,
    /// service_id → list of (date, exception 1=add 2=remove)
    pub calendar_dates: HashMap<String, Vec<(String, i64)>>,
    /// stop_id → [(trip_id, departure_seconds, sequence)]
    pub stop_times: HashMap<String, Vec<(String, u32, u32)>>,
    pub stop_times_loaded: bool,
    pub source_url: String,
    pub bytes: usize,
    pub rows: usize,
}

pub struct RouteRow {
    pub id: String,
    pub short_name: Option<String>,
    pub long_name: Option<String>,
    pub route_type: i64,
    pub color: Option<String>,
    pub text_color: Option<String>,
}

pub struct StopRow {
    pub id: String,
    pub name: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub zone_id: Option<String>,
    pub parent_station: Option<String>,
}

const MAX_STOP_TIMES: usize = 4_000_000;

/// Minimal RFC-4180 CSV reader (handles quoted fields with "" escapes).
pub fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut field = String::new();
    let mut row: Vec<String> = Vec::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        field.push('"');
                    } else {
                        in_quotes = false;
                    }
                }
                _ => field.push(c),
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => {
                    row.push(std::mem::take(&mut field));
                }
                '\r' => {}
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                }
                _ => field.push(c),
            }
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// Reads the named .txt table out of an in-memory GTFS zip.
fn read_zip_entry(bytes: &[u8], name: &str) -> Result<Option<String>, ApiError> {
    let reader = std::io::Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| zip_err(format!("The GTFS feed is not a readable zip: {e}")))?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| zip_err(format!("Failed reading zip entry: {e}")))?;
        // Feeds often wrap tables in a versioned folder (gtfs/agency.txt).
        if file.name().ends_with(name) && !file.name().contains("_full") {
            let mut buf = String::new();
            file.read_to_string(&mut buf)
                .map_err(|e| zip_err(format!("Failed reading {name}: {e}")))?;
            return Ok(Some(buf));
        }
    }
    Ok(None)
}

fn zip_err(msg: String) -> ApiError {
    ApiError::upstream(format!("{CODE_PREFIX}_PARSE_ERROR"), msg)
}

fn csv_records(text: &str) -> Vec<HashMap<String, String>> {
    let rows = parse_csv(text);
    if rows.is_empty() {
        return Vec::new();
    }
    let header: Vec<String> = rows[0].iter().map(|h| h.trim().to_string()).collect();
    rows[1..]
        .iter()
        .filter(|r| !r.iter().all(|f| f.trim().is_empty()))
        .map(|r| {
            let mut m = HashMap::new();
            for (i, value) in r.iter().enumerate() {
                if i < header.len() {
                    m.insert(header[i].clone(), value.trim().to_string());
                }
            }
            m
        })
        .collect()
}

fn parse_i64(v: Option<&String>) -> Option<i64> {
    v.and_then(|s| s.parse().ok())
}

fn parse_f64(v: Option<&String>) -> Option<f64> {
    v.and_then(|s| s.parse().ok())
}

/// `HH:MM:SS` (hours may exceed 23) → seconds.
pub fn gtfs_time_to_seconds(v: &str) -> Option<u32> {
    let parts: Vec<&str> = v.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: i64 = parts[0].trim().parse().ok()?;
    let m: i64 = parts[1].trim().parse().ok()?;
    let s: i64 = parts[2].trim().parse().ok()?;
    if h < 0 || m < 0 || s < 0 {
        return None;
    }
    Some(((h * 3600) + (m * 60) + s) as u32)
}

/// Unzips and indexes a GTFS feed.
pub fn build_feed_snapshot(agency: &crate::AgencyEntry, bytes: &[u8]) -> Result<FeedSnapshot, ApiError> {
    let need = |t: Option<String>, name: &str| -> Result<String, ApiError> {
        t.ok_or_else(|| zip_err(format!("The feed is missing required table {name}")))
    };

    let agency_text = read_zip_entry(bytes, "agency.txt")?;
    let routes_text = need(read_zip_entry(bytes, "routes.txt")?, "routes.txt")?;
    let stops_text = need(read_zip_entry(bytes, "stops.txt")?, "stops.txt")?;
    let trips_text = read_zip_entry(bytes, "trips.txt")?;
    let calendar_text = read_zip_entry(bytes, "calendar.txt")?;
    let calendar_dates_text = read_zip_entry(bytes, "calendar_dates.txt")?;
    let stop_times_text = if agency.publishes_stop_times {
        read_zip_entry(bytes, "stop_times.txt")?
    } else {
        None
    };

    // agencies
    let mut agencies = Vec::new();
    let mut agency_name = None;
    if let Some(text) = &agency_text {
        for rec in csv_records(text) {
            let name = rec.get("agency_name").cloned();
            if agency_name.is_none() {
                agency_name = name.clone();
            }
            agencies.push(json!({
                "id": rec.get("agency_id").cloned(),
                "name": name,
                "url": rec.get("agency_url").cloned(),
                "timezone": rec.get("agency_timezone").cloned(),
                "phone": rec.get("agency_phone").cloned(),
                "lang": rec.get("agency_lang").cloned(),
            }));
        }
    }

    // routes
    let mut routes = Vec::new();
    let mut route_by_id = HashMap::new();
    for rec in csv_records(&routes_text) {
        let Some(id) = rec.get("route_id").cloned().filter(|s| !s.is_empty()) else { continue };
        let route = RouteRow {
            id: id.clone(),
            short_name: rec.get("route_short_name").cloned().filter(|s| !s.is_empty()),
            long_name: rec.get("route_long_name").cloned().filter(|s| !s.is_empty()),
            route_type: parse_i64(rec.get("route_type")).unwrap_or(3),
            color: rec.get("route_color").cloned().filter(|s| !s.is_empty()),
            text_color: rec.get("route_text_color").cloned().filter(|s| !s.is_empty()),
        };
        route_by_id.insert(id, routes.len());
        routes.push(route);
    }

    // stops
    let mut stops = Vec::new();
    let mut stop_by_id = HashMap::new();
    for rec in csv_records(&stops_text) {
        let Some(id) = rec.get("stop_id").cloned().filter(|s| !s.is_empty()) else { continue };
        let stop = StopRow {
            id: id.clone(),
            name: rec.get("stop_name").cloned().filter(|s| !s.is_empty()),
            lat: parse_f64(rec.get("stop_lat")),
            lon: parse_f64(rec.get("stop_lon")),
            zone_id: rec.get("zone_id").cloned().filter(|s| !s.is_empty()),
            parent_station: rec.get("parent_station").cloned().filter(|s| !s.is_empty()),
        };
        stop_by_id.insert(id, stops.len());
        stops.push(stop);
    }

    // trips
    let mut trips: HashMap<String, (String, Option<i64>, Option<String>)> = HashMap::new();
    if let Some(text) = &trips_text {
        for rec in csv_records(text) {
            let Some(trip_id) = rec.get("trip_id").cloned().filter(|s| !s.is_empty()) else { continue };
            let route_id = rec.get("route_id").cloned().unwrap_or_default();
            trips.insert(
                trip_id,
                (
                    route_id,
                    parse_i64(rec.get("direction_id")),
                    rec.get("trip_headsign").cloned().filter(|s| !s.is_empty()),
                ),
            );
        }
    }

    // calendars
    let mut calendars = Vec::new();
    if let Some(text) = &calendar_text {
        for rec in csv_records(text) {
            let Some(service_id) = rec.get("service_id").cloned() else { continue };
            let weekdays = [
                parse_i64(rec.get("monday")).unwrap_or(0) == 1,
                parse_i64(rec.get("tuesday")).unwrap_or(0) == 1,
                parse_i64(rec.get("wednesday")).unwrap_or(0) == 1,
                parse_i64(rec.get("thursday")).unwrap_or(0) == 1,
                parse_i64(rec.get("friday")).unwrap_or(0) == 1,
                parse_i64(rec.get("saturday")).unwrap_or(0) == 1,
                parse_i64(rec.get("sunday")).unwrap_or(0) == 1,
            ];
            calendars.push((
                service_id,
                weekdays,
                rec.get("start_date").cloned().unwrap_or_default(),
                rec.get("end_date").cloned().unwrap_or_default(),
            ));
        }
    }

    // calendar_dates
    let mut calendar_dates: HashMap<String, Vec<(String, i64)>> = HashMap::new();
    if let Some(text) = &calendar_dates_text {
        for rec in csv_records(text) {
            let Some(service_id) = rec.get("service_id").cloned() else { continue };
            let Some(date) = rec.get("date").cloned() else { continue };
            let exception = parse_i64(rec.get("exception_type")).unwrap_or(1);
            calendar_dates.entry(service_id).or_default().push((date, exception));
        }
    }

    // stop_times (bounded)
    let mut stop_times: HashMap<String, Vec<(String, u32, u32)>> = HashMap::new();
    let mut stop_times_loaded = false;
    let mut rows = agencies.len() + routes.len() + stops.len();
    if let Some(text) = &stop_times_text {
        let mut count = 0usize;
        for rec in csv_records(text) {
            count += 1;
            if count > MAX_STOP_TIMES {
                break;
            }
            let Some(stop_id) = rec.get("stop_id").cloned().filter(|s| !s.is_empty()) else { continue };
            let Some(trip_id) = rec.get("trip_id").cloned().filter(|s| !s.is_empty()) else { continue };
            let departure = rec
                .get("departure_time")
                .or_else(|| rec.get("arrival_time"))
                .map(|s| s.as_str())
                .and_then(gtfs_time_to_seconds);
            let Some(departure) = departure else { continue };
            let seq = parse_i64(rec.get("stop_sequence")).unwrap_or(0) as u32;
            stop_times.entry(stop_id).or_default().push((trip_id, departure, seq));
        }
        rows += count;
        stop_times_loaded = count <= MAX_STOP_TIMES;
    }

    Ok(FeedSnapshot {
        agency_name,
        agencies,
        routes,
        route_by_id,
        stops,
        stop_by_id,
        trips,
        calendars,
        calendar_dates,
        stop_times,
        stop_times_loaded,
        source_url: agency.feed_url.to_string(),
        bytes: bytes.len(),
        rows,
    })
}

/// Service ids active on a local service date (calendar ∪/∖ calendar_dates).
pub fn active_services(snapshot: &FeedSnapshot, date: &str) -> Vec<String> {
    use chrono::NaiveDate;
    let weekday_idx = NaiveDate::parse_from_str(date, "%Y%m%d")
        .map(|d| d.weekday().num_days_from_monday() as usize)
        .unwrap_or(0);
    let mut active: Vec<String> = Vec::new();
    for (service_id, weekdays, start, end) in &snapshot.calendars {
        let in_range = start.as_str() <= date && date <= end.as_str();
        if in_range && weekdays.get(weekday_idx).copied().unwrap_or(false) {
            active.push(service_id.clone());
        }
    }
    if let Some(dates) = snapshot.calendar_dates.get("*") {
        for (d, exception) in dates {
            if d == date && *exception == 1 && !active.contains(&"*".to_string()) {
                active.push("*".to_string());
            } else if d == date && *exception == 2 {
                active.retain(|s| s != "*");
            }
        }
    }
    for (service_id, exceptions) in &snapshot.calendar_dates {
        for (d, exception) in exceptions {
            if d != date {
                continue;
            }
            match exception {
                1 => {
                    if !active.contains(service_id) {
                        active.push(service_id.clone());
                    }
                }
                2 => active.retain(|s| s != service_id),
                _ => {}
            }
        }
    }
    active
}

/// yyyyMMdd of a local date.
pub fn service_date_key(date: &chrono::NaiveDate) -> String {
    date.format("%Y%m%d").to_string()
}

/// Current local datetime in the agency timezone.
pub fn local_now(tz: &str) -> Option<chrono::DateTime<chrono_tz::Tz>> {
    let tz: Tz = tz.parse().ok()?;
    Some(chrono::Utc::now().with_timezone(&tz))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::find_agency;

    #[test]
    fn parses_quoted_csv() {
        let rows = parse_csv("a,b,c\n\"x,1\",\"he said \"\"hi\"\"\",z\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][0], "x,1");
        assert_eq!(rows[1][1], "he said \"hi\"");
    }

    #[test]
    fn gtfs_times() {
        assert_eq!(gtfs_time_to_seconds("08:30:15"), Some(8 * 3600 + 30 * 60 + 15));
        assert_eq!(gtfs_time_to_seconds("25:00:00"), Some(90_000));
        assert_eq!(gtfs_time_to_seconds("bad"), None);
        assert_eq!(gtfs_time_to_seconds("-1:00:00"), None);
    }

    #[test]
    fn service_activity() {
        let snapshot = FeedSnapshot {
            agency_name: None,
            agencies: vec![],
            routes: vec![],
            route_by_id: HashMap::new(),
            stops: vec![],
            stop_by_id: HashMap::new(),
            trips: HashMap::new(),
            calendars: vec![(
                "S1".into(),
                [true, true, true, true, true, false, false],
                "20250101".into(),
                "20311231".into(),
            )],
            calendar_dates: HashMap::new(),
            stop_times: HashMap::new(),
            stop_times_loaded: false,
            source_url: "test".into(),
            bytes: 0,
            rows: 0,
        };
        // 2026-01-05 is a Monday.
        assert_eq!(active_services(&snapshot, "20260105"), vec!["S1"]);
        // 2026-01-03 is a Saturday.
        assert!(active_services(&snapshot, "20260103").is_empty());
    }

    #[test]
    fn registry_integrity() {
        assert_eq!(crate::AGENCIES.len(), 5);
        for a in crate::AGENCIES {
            assert!(a.feed_url.starts_with("https://"));
            assert!(a.timezone.contains('/'));
            assert!(find_agency(a.id).is_some());
        }
    }
}
