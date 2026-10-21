//! transit service — GTFS-based transit directory and departures over
//! keyless agency feeds (downloaded lazily, parsed in memory, TTL-cached).
//!
//! Ported from `zoofam26/transit-api` (NestJS).

mod gtfs;
mod routes;

use actix_web::web;
use utoipa::openapi::OpenApi;

pub const SERVICE_PREFIX: &str = "/transit";
pub const CODE_PREFIX: &str = "TRANSIT";

pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 100;
pub const DEFAULT_DEPARTURE_COUNT: usize = 10;
pub const MAX_DEPARTURE_COUNT: usize = 50;
pub const DEFAULT_NEARBY_RADIUS_KM: f64 = 1.0;
pub const MAX_NEARBY_RADIUS_KM: f64 = 25.0;

/// The agency registry — every entry is a keyless, directly downloadable
/// GTFS Schedule zip (same registry as the original service).
pub struct AgencyEntry {
    pub id: &'static str,
    pub name: &'static str,
    pub city: &'static str,
    pub region: &'static str,
    pub country: &'static str,
    pub country_name: &'static str,
    pub timezone: &'static str,
    pub feed_url: &'static str,
    pub publisher: &'static str,
    pub publisher_url: &'static str,
    pub licence: &'static str,
    pub licence_url: &'static str,
    pub attribution: &'static str,
    pub publishes_stop_times: bool,
    pub measured_feed_bytes: u64,
}

pub const AGENCIES: &[AgencyEntry] = &[
    AgencyEntry {
        id: "mta-nyct",
        name: "MTA New York City Transit Subway",
        city: "New York",
        region: "New York",
        country: "US",
        country_name: "United States",
        timezone: "America/New_York",
        feed_url: "https://rrgtfsfeeds.s3.amazonaws.com/gtfs_subway.zip",
        publisher: "Metropolitan Transportation Authority",
        publisher_url: "https://www.mta.info/developers",
        licence: "MTA Developers Terms and Conditions",
        licence_url: "https://www.mta.info/developers/terms-and-conditions",
        attribution: "MTA New York City Transit GTFS — Metropolitan Transportation Authority",
        publishes_stop_times: true,
        measured_feed_bytes: 5_615_228,
    },
    AgencyEntry {
        id: "sncf-reseau",
        name: "SNCF Réseau (TGV, Intercités, TER)",
        city: "France",
        region: "Île-de-France and nationwide",
        country: "FR",
        country_name: "France",
        timezone: "Europe/Paris",
        feed_url: "https://eu.ftp.opendatasoft.com/sncf/plandata/Export_OpenData_SNCF_GTFS_NewTripId.zip",
        publisher: "SNCF Réseau",
        publisher_url: "https://www.sncf.com/fr/groupe/open-data",
        licence: "Licence Ouverte 2.0 (Open Licence)",
        licence_url: "https://www.groupe-sncf.com/fr/groupe/open-data",
        attribution: "SNCF Réseau GTFS — © SNCF Réseau, Licence Ouverte 2.0",
        publishes_stop_times: true,
        measured_feed_bytes: 5_238_365,
    },
    AgencyEntry {
        id: "sound-transit",
        name: "Sound Transit (Link light rail, ST Express)",
        city: "Seattle",
        region: "Washington",
        country: "US",
        country_name: "United States",
        timezone: "America/Los_Angeles",
        feed_url: "https://gtfs.sound.obaweb.org/prod/40_gtfs.zip",
        publisher: "Sound Transit",
        publisher_url: "https://www.soundtransit.org",
        licence: "Sound Transit Transit Data Terms of Use",
        licence_url: "https://www.soundtransit.org/help-contacts/business-information/open-transit-data-otd/transit-data-terms-use",
        attribution: "Sound Transit GTFS — Sound Transit Open Transit Data",
        publishes_stop_times: true,
        measured_feed_bytes: 2_253_659,
    },
    AgencyEntry {
        id: "bart",
        name: "Bay Area Rapid Transit (BART)",
        city: "San Francisco Bay Area",
        region: "California",
        country: "US",
        country_name: "United States",
        timezone: "America/Los_Angeles",
        feed_url: "https://www.bart.gov/dev/schedules/google_transit.zip",
        publisher: "Bay Area Rapid Transit District",
        publisher_url: "https://www.bart.gov/developers",
        licence: "BART website terms and copyright notice",
        licence_url: "https://www.bart.gov/siteinfo/copyright",
        attribution: "BART GTFS — Bay Area Rapid Transit District",
        publishes_stop_times: true,
        measured_feed_bytes: 892_312,
    },
    AgencyEntry {
        id: "seattle-monorail",
        name: "Seattle Center Monorail",
        city: "Seattle",
        region: "Washington",
        country: "US",
        country_name: "United States",
        timezone: "America/Los_Angeles",
        feed_url: "https://gtfs.sound.obaweb.org/prod/96_gtfs.zip",
        publisher: "Seattle Monorail Services",
        publisher_url: "https://www.seattlemonorail.com",
        licence: "Sound Transit Transit Data Terms of Use",
        licence_url: "https://www.soundtransit.org/help-contacts/business-information/open-transit-data-otd/transit-data-terms-use",
        attribution: "Seattle Center Monorail GTFS — Sound Transit Open Transit Data",
        publishes_stop_times: true,
        measured_feed_bytes: 26_424,
    },
];

pub fn find_agency(id: &str) -> Option<&'static AgencyEntry> {
    AGENCIES.iter().find(|a| a.id == id)
}

pub fn router(cfg: &mut web::ServiceConfig) {
    routes::configure(cfg);
}

pub fn openapi() -> OpenApi {
    routes::openapi_doc()
}

pub fn index() -> serde_json::Value {
    serde_json::json!({
        "name": "transit-api",
        "prefix": SERVICE_PREFIX,
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Transit agencies, routes, stops and scheduled departures over keyless GTFS Schedule feeds (MTA Subway, SNCF Réseau, Sound Transit, BART, Seattle Monorail).",
        "dataSource": {
            "provider": "GTFS Schedule publishers (per agency)",
            "upstream": AGENCIES.iter().map(|a| a.feed_url).collect::<Vec<_>>(),
            "requiresApiKey": false,
            "note": "A feed (26 KB to 6 MB zip) is downloaded at most once per TTL window, on the first request that needs it; concurrent callers share one download.",
        },
        "endpoints": [
            { "method": "GET", "path": "/transit/agencies", "description": "The agency registry, with search/country filter and paging" },
            { "method": "GET", "path": "/transit/agencies/{agencyId}", "description": "One agency, plus its feed summary when cached" },
            { "method": "GET", "path": "/transit/agencies/{agencyId}/routes", "description": "Routes of a feed, with search and route_type filter" },
            { "method": "GET", "path": "/transit/routes/{agencyId}/{routeId}", "description": "One route" },
            { "method": "GET", "path": "/transit/stops/{agencyId}/{stopId}", "description": "One stop" },
            { "method": "GET", "path": "/transit/stops/nearby", "description": "Stops near a point, over cached feeds" },
            { "method": "GET", "path": "/transit/departures/{agencyId}/{stopId}", "description": "Scheduled departures from a stop" },
            { "method": "GET", "path": "/transit/transport-modes", "description": "The GTFS route_type catalogue" },
            { "method": "GET", "path": "/transit/health", "description": "Liveness plus registry health" },
            { "method": "GET", "path": "/transit/health/ready", "description": "Readiness probe" },
            { "method": "GET", "path": "/transit/openapi.json", "description": "OpenAPI 3 document for this service" },
        ],
    })
}

/// GTFS route_type catalogue (standard + extended codes).
pub const ROUTE_TYPES: &[(i64, &str, &str, bool)] = &[
    (0, "tram", "Tram, streetcar or light rail", false),
    (1, "subway", "Subway or metro", false),
    (2, "rail", "Rail (heavy rail, interurban, commuter)", false),
    (3, "bus", "Bus", false),
    (4, "ferry", "Ferry or water transport", false),
    (5, "cable_tram", "Cable tram", false),
    (6, "aerial_lift", "Aerial lift or funicular", false),
    (7, "funicular", "Funicular (specification correction; treated as aerial lift)", false),
    (11, "trolleybus", "Trolleybus", false),
    (12, "monorail", "Monorail", false),
    (400, "railway_service", "Railway service (extended, legacy)", true),
    (401, "subway_service", "Subway service (extended, legacy)", true),
    (402, "rail_service", "Rail service (extended, legacy)", true),
    (403, "bus_service", "Bus service (extended, legacy)", true),
    (404, "ferry_service", "Ferry service (extended, legacy)", true),
    (405, "cable_tram_service", "Cable tram service (extended, legacy)", true),
];

pub fn route_type_descriptor(code: i64) -> Option<&'static (i64, &'static str, &'static str, bool)> {
    ROUTE_TYPES.iter().find(|(c, _, _, _)| *c == code)
}
