//! The static registry of transit agencies this service knows about.
//!
//! Port of `config/agency-registry.ts`: deliberately *configuration*, not a
//! proxy. Every entry was fetched and verified by hand (status, content
//! length, zip magic bytes, licence), and nothing here is discovered at
//! runtime, so `GET /transit/agencies` is answerable with zero network
//! traffic.
//!
//! A deployment can point any entry at a different archive with
//! `TRANSIT__AGENCY_FEEDS='{"id":"https://…/feed.zip",…}'` without touching
//! the code; the descriptive fields stay as registered.

use once_cell::sync::OnceCell;
use serde_json::{json, Value};

use master_common::error::ApiError;

use crate::errors;
use crate::CODE_PREFIX;

/// A single entry of the registry.
#[derive(Debug, Clone)]
pub struct AgencyEntry {
    /// Stable, URL-safe slug; also the `:agencyId` path segment everywhere.
    pub id: String,
    /// Display name, used as the primary sort key of `/transit/agencies`.
    pub name: String,
    /// City or conurbation the network serves.
    pub city: String,
    /// Administrative region, state or federal subject.
    pub region: String,
    /// ISO 3166-1 alpha-2 country code.
    pub country: String,
    /// Human readable country name.
    pub country_name: String,
    /// IANA timezone the feed's schedule is expressed in.
    pub timezone: String,
    /// Keyless, directly downloadable GTFS Schedule zip.
    pub feed_url: String,
    /// Publisher of the feed, as stated by the agency.
    pub publisher: String,
    pub publisher_url: String,
    /// Short licence identifier, e.g. `CC-BY-4.0`.
    pub licence: String,
    /// Where the licence terms are published.
    pub licence_url: String,
    /// Attribution string a redistributor must display.
    pub attribution: String,
    /// `stop_times.txt` was present in the feed when this entry was verified,
    /// so `/transit/departures` is a candidate.
    pub publishes_stop_times: bool,
    /// Verified compressed size in bytes, for operator reference only.
    pub measured_feed_bytes: i64,
}

fn entry(
    id: &str,
    name: &str,
    city: &str,
    region: &str,
    country: &str,
    country_name: &str,
    timezone: &str,
    feed_url: &str,
    publisher: &str,
    publisher_url: &str,
    licence: &str,
    licence_url: &str,
    attribution: &str,
    measured_feed_bytes: i64,
) -> AgencyEntry {
    AgencyEntry {
        id: id.to_string(),
        name: name.to_string(),
        city: city.to_string(),
        region: region.to_string(),
        country: country.to_string(),
        country_name: country_name.to_string(),
        timezone: timezone.to_string(),
        feed_url: feed_url.to_string(),
        publisher: publisher.to_string(),
        publisher_url: publisher_url.to_string(),
        licence: licence.to_string(),
        licence_url: licence_url.to_string(),
        attribution: attribution.to_string(),
        publishes_stop_times: true,
        measured_feed_bytes,
    }
}

/// The verified registry, in registration order (as the original ships it).
pub fn default_agencies() -> Vec<AgencyEntry> {
    vec![
        entry(
            "mta-nyct",
            "MTA New York City Transit Subway",
            "New York",
            "New York",
            "US",
            "United States",
            "America/New_York",
            "https://rrgtfsfeeds.s3.amazonaws.com/gtfs_subway.zip",
            "Metropolitan Transportation Authority",
            "https://www.mta.info/developers",
            "MTA Developers Terms and Conditions",
            "https://www.mta.info/developers/terms-and-conditions",
            "MTA New York City Transit GTFS — Metropolitan Transportation Authority",
            5_615_228,
        ),
        entry(
            "sncf-reseau",
            "SNCF Réseau (TGV, Intercités, TER)",
            "France",
            "Île-de-France and nationwide",
            "FR",
            "France",
            "Europe/Paris",
            "https://eu.ftp.opendatasoft.com/sncf/plandata/Export_OpenData_SNCF_GTFS_NewTripId.zip",
            "SNCF Réseau",
            "https://www.sncf.com/fr/groupe/open-data",
            "Licence Ouverte 2.0 (Open Licence)",
            "https://www.groupe-sncf.com/fr/groupe/open-data",
            "SNCF Réseau GTFS — © SNCF Réseau, Licence Ouverte 2.0",
            5_238_365,
        ),
        entry(
            "sound-transit",
            "Sound Transit (Link light rail, ST Express)",
            "Seattle",
            "Washington",
            "US",
            "United States",
            "America/Los_Angeles",
            "https://gtfs.sound.obaweb.org/prod/40_gtfs.zip",
            "Sound Transit",
            "https://www.soundtransit.org",
            "Sound Transit Transit Data Terms of Use",
            "https://www.soundtransit.org/help-contacts/business-information/open-transit-data-otd/transit-data-terms-use",
            "Sound Transit GTFS — Sound Transit Open Transit Data",
            2_253_659,
        ),
        entry(
            "bart",
            "Bay Area Rapid Transit (BART)",
            "San Francisco Bay Area",
            "California",
            "US",
            "United States",
            "America/Los_Angeles",
            "https://www.bart.gov/dev/schedules/google_transit.zip",
            "Bay Area Rapid Transit District",
            "https://www.bart.gov/developers",
            "BART website terms and copyright notice",
            "https://www.bart.gov/siteinfo/copyright",
            "BART GTFS — Bay Area Rapid Transit District",
            892_312,
        ),
        entry(
            "seattle-monorail",
            "Seattle Center Monorail",
            "Seattle",
            "Washington",
            "US",
            "United States",
            "America/Los_Angeles",
            "https://gtfs.sound.obaweb.org/prod/96_gtfs.zip",
            "Seattle Monorail Services",
            "https://www.seattlemonorail.com",
            "Sound Transit Transit Data Terms of Use",
            "https://www.soundtransit.org/help-contacts/business-information/open-transit-data-otd/transit-data-terms-use",
            "Seattle Center Monorail GTFS — Sound Transit Open Transit Data",
            26_424,
        ),
    ]
}

/// The agency whose feed is probed by `/health`: the smallest archive in the
/// registry (26 KB), so a liveness probe over it costs a fraction of a second
/// and is still a genuine round trip to a real GTFS host.
pub const PROBE_AGENCY_ID: &str = "seattle-monorail";

/// Applies environment overrides on top of the built-in registry.
///
/// `TRANSIT__AGENCY_FEEDS` accepts a JSON object of `id -> feedUrl`; unknown
/// ids are ignored so a typo degrades to the default rather than to a broken
/// registry, and the effective feed URL is reported on every agency response.
pub fn resolve_agencies(raw: Option<&str>, base: &[AgencyEntry]) -> Vec<AgencyEntry> {
    let Some(raw) = raw else { return base.to_vec() };
    let Ok(parsed) = serde_json::from_str::<Value>(raw) else { return base.to_vec() };
    let Some(object) = parsed.as_object() else { return base.to_vec() };
    let overrides: Vec<(&String, &str)> = object
        .iter()
        .filter_map(|(key, value)| value.as_str().map(|url| (key, url)))
        .collect();
    if overrides.is_empty() {
        return base.to_vec();
    }
    base.iter()
        .map(|agency| match overrides.iter().find(|(key, _)| *key == &agency.id) {
            Some((_, url)) => {
                let mut overridden = agency.clone();
                overridden.feed_url = url.to_string();
                overridden
            }
            None => agency.clone(),
        })
        .collect()
}

static REGISTRY: OnceCell<Vec<AgencyEntry>> = OnceCell::new();

/// The effective registry (defaults + `TRANSIT__AGENCY_FEEDS` overrides),
/// resolved at most once per process.
pub fn registry() -> &'static [AgencyEntry] {
    REGISTRY.get_or_init(|| {
        let raw = std::env::var("TRANSIT__AGENCY_FEEDS").ok();
        resolve_agencies(raw.as_deref(), &default_agencies())
    })
}

/// One registry entry by id, or a 404 that names the whole registry.
pub fn require_agency(id: &str) -> Result<&'static AgencyEntry, ApiError> {
    let agencies = registry();
    agencies
        .iter()
        .find(|agency| agency.id == id)
        .ok_or_else(|| {
            errors::not_found(
                format!("No agency with id \"{id}\" in the registry"),
                json!({
                    "agencyId": id,
                    "registrySize": agencies.len(),
                    "registry": agencies.iter().map(|agency| agency.id.clone()).collect::<Vec<_>>(),
                }),
            )
        })
}

/// The registry entry, as served (`toAgencyRef` in dto/response.types.ts).
pub fn agency_ref_json(agency: &AgencyEntry) -> Value {
    json!({
        "id": agency.id,
        "name": agency.name,
        "city": agency.city,
        "region": agency.region,
        "country": agency.country,
        "countryName": agency.country_name,
        "timezone": agency.timezone,
        "feedUrl": agency.feed_url,
        "publisher": agency.publisher,
        "publisherUrl": agency.publisher_url,
        "licence": agency.licence,
        "licenceUrl": agency.licence_url,
        "attribution": agency.attribution,
        "publishesStopTimes": agency.publishes_stop_times,
        "measuredFeedBytes": agency.measured_feed_bytes,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn registry_entries_are_complete_and_unique() {
        let agencies = default_agencies();
        assert_eq!(agencies.len(), 5);
        let mut ids: Vec<String> = agencies.iter().map(|a| a.id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 5, "agency ids are unique");
        for agency in &agencies {
            assert!(
                agency
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "id {:?} must be a url-safe slug",
                agency.id
            );
            assert!(agency.feed_url.starts_with("https://"), "{agency:?}");
            assert_eq!(agency.country.len(), 2, "{agency:?}");
            assert!(!agency.name.is_empty() && !agency.timezone.is_empty());
            assert!(
                agency.publishes_stop_times,
                "every registered feed published stop_times.txt when verified"
            );
            assert!(agency.measured_feed_bytes > 0);
            assert!(agency.licence_url.starts_with("https://"));
        }
        assert!(
            agencies.iter().any(|a| a.id == PROBE_AGENCY_ID),
            "the health probe agency exists"
        );
        assert!(agencies.iter().any(|a| a.feed_url.contains("bart.gov")));
        assert!(agencies.iter().any(|a| a.feed_url.contains("sound.obaweb.org")));
        assert!(agencies
            .iter()
            .any(|a| a.feed_url.contains("rrgtfsfeeds.s3.amazonaws.com")));
    }

    #[test]
    fn resolve_agencies_applies_overrides_only_to_known_ids() {
        let base = default_agencies();
        let raw = r#"{"bart": "https://mirror.example.org/bart.zip", "nope": "https://evil.example.org/x.zip"}"#;
        let resolved = resolve_agencies(Some(raw), &base);
        let bart = resolved.iter().find(|a| a.id == "bart").expect("bart present");
        assert_eq!(bart.feed_url, "https://mirror.example.org/bart.zip");
        assert!(
            resolved.iter().find(|a| a.id == "nope").is_none(),
            "unknown ids are ignored, not added"
        );
        assert_eq!(resolved.len(), base.len(), "descriptive fields stay as registered");
        let mta = resolved.iter().find(|a| a.id == "mta-nyct").unwrap();
        assert_eq!(
            mta.feed_url,
            base.iter().find(|a| a.id == "mta-nyct").unwrap().feed_url
        );
    }

    #[test]
    fn resolve_agencies_degrades_to_defaults_on_garbage() {
        let base = default_agencies();
        assert_eq!(resolve_agencies(None, &base).len(), base.len());
        assert_eq!(resolve_agencies(Some("not json"), &base).len(), base.len());
        assert_eq!(resolve_agencies(Some("[1,2,3]"), &base).len(), base.len());
        assert_eq!(resolve_agencies(Some(r#"{"bart": 42}"#), &base).len(), base.len());
        assert_eq!(
            resolve_agencies(Some(r#"{"bart": "https://m.example.org/b.zip"}"#), &base).len(),
            base.len()
        );
    }

    #[test]
    fn agency_ref_json_carries_every_documented_field() {
        let agency = &default_agencies()[3];
        let value = agency_ref_json(agency);
        for field in [
            "id",
            "name",
            "city",
            "region",
            "country",
            "countryName",
            "timezone",
            "feedUrl",
            "publisher",
            "publisherUrl",
            "licence",
            "licenceUrl",
            "attribution",
            "publishesStopTimes",
            "measuredFeedBytes",
        ] {
            assert!(value.get(field).is_some(), "missing {field}");
        }
        assert_eq!(value.get("id").and_then(Value::as_str), Some("bart"));
        assert_eq!(value.get("country").and_then(Value::as_str), Some("US"));
    }

    #[test]
    fn require_agency_reports_the_registry_on_unknown_ids() {
        let error = require_agency("nope").expect_err("unknown id");
        let body = error.to_error_body();
        assert_eq!(body["error"]["code"], json!(format!("{CODE_PREFIX}_NOT_FOUND")));
        let details = body["error"]["details"].clone();
        assert_eq!(details["agencyId"], json!("nope"));
        assert_eq!(details["registrySize"], json!(5));
        assert_eq!(details["registry"].as_array().map(Vec::len), Some(5));
    }
}
