//! The only place that talks to the geolocation provider (ipwho.is) and the
//! projection catalogue applied on top of its documents.
//!
//! Ported 1:1 from `ip-geolocation-api/src/geolocation/client/ipwho.client.ts`,
//! `client/ipwho.parser.ts` and `dto/fields.ts`. The provider signals
//! application-level failures with `success: false` inside an HTTP 200, so the
//! flag is authoritative: a bad address becomes a 400, a reserved range a 404,
//! anything else a 502.

use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Map, Value};

use master_common::error::ApiError;
use master_common::httpx;
use master_common::state::{cache_names, AppState};

use crate::ipaddr::{parse_ip_address, ParsedIpAddress};
use crate::{CODE_PREFIX, UPSTREAM_IPWHOIS};

/// The service's own domain model. Nothing upstream is ever returned to a
/// caller: the provider's field names (`continent_code`, `connection.asn`, …)
/// stop in this module's parser.
#[derive(Debug, Clone, Serialize)]
pub struct GeolocationRecord {
    pub address: String,
    pub ip_version: u8,
    pub scope: String,
    pub is_public: bool,
    pub location: LocationRecord,
    pub network: NetworkRecord,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocationRecord {
    pub continent: Option<String>,
    pub continent_code: Option<String>,
    pub country: Option<String>,
    pub country_code: Option<String>,
    pub region: Option<String>,
    pub region_code: Option<String>,
    pub city: Option<String>,
    pub postal: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub timezone_id: Option<String>,
    pub timezone_abbreviation: Option<String>,
    pub timezone_offset: Option<String>,
    pub is_dst: Option<bool>,
    pub calling_code: Option<String>,
    pub capital: Option<String>,
    pub borders: Vec<String>,
    pub is_eu: Option<bool>,
    pub flag_emoji: Option<String>,
    pub flag_image: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkRecord {
    pub asn: Option<i64>,
    pub organization: Option<String>,
    pub isp: Option<String>,
    pub domain: Option<String>,
}

/* ------------------------------------------------------- value coercion */

fn as_str(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Some(Value::Number(n)) => n.as_f64().filter(|v| v.is_finite()).map(|v| {
            if v.fract() == 0.0 && v.abs() < 1e15 {
                format!("{}", v as i64)
            } else {
                v.to_string()
            }
        }),
        _ => None,
    }
}

fn as_num(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(n)) => n.as_f64().filter(|v| v.is_finite()),
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().parse::<f64>().ok().filter(|v| v.is_finite()),
        _ => None,
    }
}

fn as_bool(value: Option<&Value>) -> Option<bool> {
    match value {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::String(s)) if s == "true" => Some(true),
        Some(Value::String(s)) if s == "false" => Some(false),
        _ => None,
    }
}

fn as_csv(value: Option<&Value>) -> Vec<String> {
    as_str(value)
        .map(|text| {
            text.split(',')
                .map(|entry| entry.trim().to_string())
                .filter(|entry| !entry.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/* ------------------------------------------------------------- mapping */

fn map_location(payload: &Value) -> LocationRecord {
    LocationRecord {
        continent: as_str(payload.get("continent")),
        continent_code: as_str(payload.get("continent_code")),
        country: as_str(payload.get("country")),
        country_code: as_str(payload.get("country_code")).map(|c| c.to_uppercase()),
        region: as_str(payload.get("region")),
        region_code: as_str(payload.get("region_code")),
        city: as_str(payload.get("city")),
        postal: as_str(payload.get("postal")),
        latitude: as_num(payload.get("latitude")),
        longitude: as_num(payload.get("longitude")),
        timezone_id: payload.get("timezone").and_then(|t| as_str(t.get("id"))),
        timezone_abbreviation: payload.get("timezone").and_then(|t| as_str(t.get("abbr"))),
        timezone_offset: payload.get("timezone").and_then(|t| as_str(t.get("utc"))),
        is_dst: payload.get("timezone").and_then(|t| as_bool(t.get("is_dst"))),
        calling_code: as_str(payload.get("calling_code")),
        capital: as_str(payload.get("capital")),
        borders: as_csv(payload.get("borders")),
        is_eu: as_bool(payload.get("is_eu")),
        flag_emoji: payload.get("flag").and_then(|f| as_str(f.get("emoji"))),
        flag_image: payload.get("flag").and_then(|f| as_str(f.get("img"))),
    }
}

fn map_network(payload: &Value) -> NetworkRecord {
    let asn = payload.get("connection").and_then(|c| as_num(c.get("asn")));
    NetworkRecord {
        asn: asn.map(|v| v as i64),
        organization: payload.get("connection").and_then(|c| as_str(c.get("org"))),
        isp: payload.get("connection").and_then(|c| as_str(c.get("isp"))),
        domain: payload.get("connection").and_then(|c| as_str(c.get("domain"))),
    }
}

fn parse_error(message: &str, details: Value) -> ApiError {
    ApiError::Upstream {
        code: "PARSE_ERROR".to_string(),
        message: message.to_string(),
        details: Some(details),
    }
}

/// Turns one provider document into the service's own domain record.
///
/// Which error a `success: false` becomes depends on the message: a bad
/// address is a client error, a reserved range is a 404.
pub fn parse_ipwho_payload(payload: &Value) -> Result<GeolocationRecord, ApiError> {
    if !payload.is_object() {
        return Err(parse_error(
            "the geolocation provider returned a non-object payload",
            json!({}),
        ));
    }

    if payload.get("success").and_then(Value::as_bool) != Some(true) {
        let message = as_str(payload.get("message"))
            .unwrap_or_else(|| "the provider reported an unspecified failure".to_string());
        let reported = as_str(payload.get("ip"));
        if message.to_lowercase().contains("invalid ip") {
            return Err(ApiError::validation_details(
                "INVALID_ADDRESS",
                "the address is not a valid IPv4 or IPv6 address",
                json!({
                    "address": reported.unwrap_or_default(),
                    "reason": "provider rejected the address as invalid",
                }),
            ));
        }
        if message.to_lowercase().contains("reserved") {
            return Err(ApiError::not_found_details(
                "NOT_FOUND",
                "the geolocation provider holds no data for this reserved address",
                json!({ "providerMessage": message, "address": reported }),
            ));
        }
        return Err(parse_error(
            "the geolocation provider reported a failed lookup",
            json!({ "providerMessage": message }),
        ));
    }

    let reported = as_str(payload.get("ip")).ok_or_else(|| {
        parse_error(
            "the geolocation provider returned a success envelope without an ip field",
            json!({}),
        )
    })?;

    let parsed: ParsedIpAddress = parse_ip_address(&reported).ok_or_else(|| {
        parse_error(
            "the geolocation provider echoed back an address that is not a valid IP",
            json!({ "reported": reported }),
        )
    })?;

    let network = map_network(payload);
    if network.asn.is_none() && payload.get("connection").is_none() {
        return Err(parse_error(
            "the geolocation provider returned no connection block for a routable address",
            json!({}),
        ));
    }

    Ok(GeolocationRecord {
        address: parsed.address,
        ip_version: parsed.version,
        scope: parsed.scope.to_string(),
        is_public: parsed.is_public,
        location: map_location(payload),
        network,
    })
}

/* -------------------------------------------------------------- client */

/// `encodeURIComponent` keeps a colon-separated IPv6 address legal in the path.
fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*'
            | b'\'' | b'(' | b')' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The upstream base URL, honouring the `UPSTREAM_IPWHOIS` override.
pub fn upstream_base(state: &AppState) -> String {
    state.settings.upstream("IPWHOIS", UPSTREAM_IPWHOIS)
}

async fn fetch_record(state: &AppState, address: &str) -> Result<Value, ApiError> {
    let base = upstream_base(state).trim_end_matches('/').to_string();
    let url = format!("{base}/{}", encode_uri_component(address));
    let timed = httpx::get_json_with_headers(
        state,
        &url,
        &[],
        &[
            ("accept", "application/json"),
            ("user-agent", "rapidaApi-ip-geolocation/1.0"),
        ],
        CODE_PREFIX,
    )
    .await?;
    let record = parse_ipwho_payload(&timed.body)?;
    serde_json::to_value(&record)
        .map_err(|e| ApiError::internal(format!("record serialisation failed: {e}")))
}

/// Resolves the canonical address to the cached normalised record.
///
/// Cache key: the canonical address is the only input that changes the answer,
/// so the same address under any spelling shares one entry. Concurrent callers
/// share one upstream call (single-flight via `cached_json`).
pub async fn resolve_record(
    state: &AppState,
    canonical: &str,
) -> Result<(Arc<Value>, bool, u64), ApiError> {
    let key = format!("geo:{canonical}");
    let owned = canonical.to_string();
    let cached = httpx::cached_json(
        state,
        cache_names::IP_GEOLOCATION,
        &key,
        move || async move { fetch_record(state, &owned).await },
    )
    .await?;
    Ok((cached.body, cached.cached, cached.duration_ms))
}

/* ------------------------------------------------------- field catalogue */

/// The `fields` catalogue, in response order: every name is a top-level key of
/// the normalised record, so an allowlist guarantees a caller can never reach
/// an internal field. `fields` is deliberately not forwarded to the provider:
/// this service always fetches the full free-tier document and projects it
/// here, which keeps the cache key meaningful.
pub const FIELD_NAMES: &[&str] = &[
    "continent",
    "continentCode",
    "country",
    "countryCode",
    "region",
    "regionCode",
    "city",
    "postal",
    "latitude",
    "longitude",
    "timezone",
    "timezoneAbbreviation",
    "timezoneOffset",
    "isDst",
    "callingCode",
    "capital",
    "borders",
    "isEu",
    "flag",
    "asn",
    "organization",
    "isp",
    "domain",
];

/// Returned when the caller does not ask for anything in particular: the
/// identity of the address plus the fields most consumers need.
pub const DEFAULT_FIELDS: &[&str] = &[
    "country",
    "countryCode",
    "region",
    "city",
    "latitude",
    "longitude",
    "timezone",
    "postal",
    "asn",
    "organization",
    "isp",
];

/// Upper bound on a single `fields=` list (`@ArrayMaxSize(24)` upstream).
pub const MAX_FIELDS: usize = 24;

fn is_field_name(value: &str) -> bool {
    FIELD_NAMES.contains(&value)
}

/// Names must be alphanumeric so a junk value is rejected before the allowlist
/// check produces a precise error naming the offending fields.
fn is_well_formed_name(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
        && value.len() <= 32
        && value.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Resolves a `fields` list into a validated, de-duplicated, stably ordered
/// selection. An unknown name is a client error, not a silently ignored typo.
pub fn resolve_field_selection(requested: Option<&str>) -> Result<Vec<String>, ApiError> {
    let Some(raw) = requested else {
        return Ok(DEFAULT_FIELDS.iter().map(|s| s.to_string()).collect());
    };
    // Accepts `?fields=country,city`, `?fields=country|city` and repeated
    // separators, so hand-typed requests keep working.
    let items: Vec<String> = raw
        .split([',', '|'])
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect();
    if items.is_empty() {
        return Ok(DEFAULT_FIELDS.iter().map(|s| s.to_string()).collect());
    }

    for item in &items {
        if !is_well_formed_name(item) {
            return Err(ApiError::validation(
                "INVALID_REQUEST",
                "each field name must be alphanumeric, start with a letter and be at most 32 characters",
            ));
        }
    }
    if items.len() > MAX_FIELDS {
        return Err(ApiError::validation(
            "INVALID_REQUEST",
            format!("fields must contain no more than {MAX_FIELDS} elements"),
        ));
    }

    let unknown: Vec<&String> = items.iter().filter(|name| !is_field_name(name)).collect();
    if !unknown.is_empty() {
        return Err(ApiError::validation_details(
            "INVALID_REQUEST",
            format!(
                "Unknown field{}: {}",
                if unknown.len() > 1 { "s" } else { "" },
                unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ),
            json!({ "unknown": unknown, "allowed": FIELD_NAMES }),
        ));
    }

    // Catalogue order (not caller order) keeps responses stable and cacheable.
    let unique: std::collections::HashSet<&String> = items.iter().collect();
    Ok(FIELD_NAMES
        .iter()
        .filter(|name| unique.contains(&&name.to_string()))
        .map(|name| name.to_string())
        .collect())
}

/// Projects a normalised record down to the selected fields.
pub fn project_fields(record: &Value, selection: &[String]) -> Value {
    let location = record.get("location").cloned().unwrap_or(Value::Null);
    let network = record.get("network").cloned().unwrap_or(Value::Null);
    let mut out = Map::new();
    for name in selection {
        let value: Value = match name.as_str() {
            "continent" => location.get("continent").cloned().unwrap_or(Value::Null),
            "continentCode" => location.get("continentCode").cloned().unwrap_or(Value::Null),
            "country" => location.get("country").cloned().unwrap_or(Value::Null),
            "countryCode" => location.get("countryCode").cloned().unwrap_or(Value::Null),
            "region" => location.get("region").cloned().unwrap_or(Value::Null),
            "regionCode" => location.get("regionCode").cloned().unwrap_or(Value::Null),
            "city" => location.get("city").cloned().unwrap_or(Value::Null),
            "postal" => location.get("postal").cloned().unwrap_or(Value::Null),
            "latitude" => location.get("latitude").cloned().unwrap_or(Value::Null),
            "longitude" => location.get("longitude").cloned().unwrap_or(Value::Null),
            "timezone" => location.get("timezoneId").cloned().unwrap_or(Value::Null),
            "timezoneAbbreviation" => location.get("timezoneAbbreviation").cloned().unwrap_or(Value::Null),
            "timezoneOffset" => location.get("timezoneOffset").cloned().unwrap_or(Value::Null),
            "isDst" => location.get("isDst").cloned().unwrap_or(Value::Null),
            "callingCode" => location.get("callingCode").cloned().unwrap_or(Value::Null),
            "capital" => location.get("capital").cloned().unwrap_or(Value::Null),
            "borders" => location.get("borders").cloned().unwrap_or(Value::Array(Vec::new())),
            "isEu" => location.get("isEu").cloned().unwrap_or(Value::Null),
            "flag" => json!({
                "emoji": location.get("flagEmoji").cloned().unwrap_or(Value::Null),
                "image": location.get("flagImage").cloned().unwrap_or(Value::Null),
            }),
            "asn" => network.get("asn").cloned().unwrap_or(Value::Null),
            "organization" => network.get("organization").cloned().unwrap_or(Value::Null),
            "isp" => network.get("isp").cloned().unwrap_or(Value::Null),
            "domain" => network.get("domain").cloned().unwrap_or(Value::Null),
            _ => Value::Null,
        };
        out.insert(name.clone(), value);
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_payload() -> Value {
        serde_json::json!({
            "ip": "8.8.8.8",
            "success": true,
            "type": "IPv4",
            "continent": "North America",
            "continent_code": "NA",
            "country": "United States",
            "country_code": "us",
            "region": "California",
            "region_code": "CA",
            "city": "San Jose",
            "latitude": 37.3393939,
            "longitude": -121.8949553,
            "is_eu": false,
            "postal": "95025",
            "calling_code": "1",
            "capital": "Washington D.C.",
            "borders": "CA,MX",
            "flag": { "img": "https://cdn.ipwhois.io/flags/us.svg", "emoji": "🇺🇸" },
            "connection": { "asn": 15169, "org": "Google LLC", "isp": "Google LLC", "domain": "google.com" },
            "timezone": { "id": "America/Los_Angeles", "abbr": "PDT", "is_dst": true, "utc": "-07:00" }
        })
    }

    #[test]
    fn parses_a_success_payload_into_the_domain_record() {
        let record = parse_ipwho_payload(&sample_payload()).expect("valid payload");
        assert_eq!(record.address, "8.8.8.8");
        assert_eq!(record.ip_version, 4);
        assert_eq!(record.scope, "public");
        assert!(record.is_public);
        assert_eq!(record.location.country.as_deref(), Some("United States"));
        assert_eq!(record.location.country_code.as_deref(), Some("US"));
        assert_eq!(record.location.borders, vec!["CA", "MX"]);
        assert_eq!(record.location.timezone_offset.as_deref(), Some("-07:00"));
        assert_eq!(record.location.flag_emoji.as_deref(), Some("🇺🇸"));
        assert_eq!(record.network.asn, Some(15169));
        assert_eq!(record.network.organization.as_deref(), Some("Google LLC"));
    }

    #[test]
    fn maps_success_false_invalid_ip_to_a_400() {
        let payload = serde_json::json!({ "ip": "999.1.1.1", "success": false, "message": "Invalid IP address" });
        let err = parse_ipwho_payload(&payload).unwrap_err();
        assert!(matches!(err, ApiError::Validation { ref code, .. } if code == "INVALID_ADDRESS"));
        assert_eq!(err.to_string(), "the address is not a valid IPv4 or IPv6 address");
    }

    #[test]
    fn maps_success_false_reserved_range_to_a_404() {
        let payload = serde_json::json!({ "ip": "192.168.1.1", "success": false, "message": "Reserved range" });
        let err = parse_ipwho_payload(&payload).unwrap_err();
        assert!(matches!(err, ApiError::NotFound { ref code, .. } if code == "NOT_FOUND"));
        assert_eq!(
            err.to_string(),
            "the geolocation provider holds no data for this reserved address"
        );
    }

    #[test]
    fn maps_other_success_false_to_parse_error() {
        let payload = serde_json::json!({ "ip": "8.8.8.8", "success": false, "message": "Maintenance" });
        let err = parse_ipwho_payload(&payload).unwrap_err();
        assert!(matches!(err, ApiError::Upstream { ref code, .. } if code == "PARSE_ERROR"));
    }

    #[test]
    fn encodes_ipv6_for_the_upstream_path() {
        assert_eq!(encode_uri_component("8.8.8.8"), "8.8.8.8");
        assert_eq!(encode_uri_component("2001:db8::1"), "2001%3Adb8%3A%3A1");
    }

    #[test]
    fn default_selection_lists_the_common_fields() {
        let selection = resolve_field_selection(None).expect("default");
        assert_eq!(selection, DEFAULT_FIELDS);
        assert_eq!(
            resolve_field_selection(Some("")).unwrap(),
            DEFAULT_FIELDS,
            "an empty list falls back to the default"
        );
    }

    #[test]
    fn selection_dedupes_and_reorders_into_catalogue_order() {
        let selection = resolve_field_selection(Some("isp|country,asn,isp,country")).unwrap();
        assert_eq!(selection, vec!["country", "asn", "isp"]);
    }

    #[test]
    fn unknown_field_is_a_client_error_naming_the_field() {
        let err = resolve_field_selection(Some("country,bogus")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Unknown field: bogus"
        );
        let err = resolve_field_selection(Some("bogus,junk")).unwrap_err();
        assert_eq!(err.to_string(), "Unknown fields: bogus, junk");
    }

    #[test]
    fn projects_selected_fields_from_a_cached_record() {
        let record = serde_json::to_value(parse_ipwho_payload(&sample_payload()).unwrap()).unwrap();
        let selection = resolve_field_selection(Some("city,flag,asn")).unwrap();
        let projected = project_fields(&record, &selection);
        assert_eq!(projected["city"], json!("San Jose"));
        assert_eq!(projected["flag"], json!({ "emoji": "🇺🇸", "image": "https://cdn.ipwhois.io/flags/us.svg" }));
        assert_eq!(projected["asn"], json!(15169));
        assert!(projected.get("country").is_none(), "unselected fields are omitted");
    }
}
