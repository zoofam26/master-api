//! countries service — every route answered from an in-memory snapshot of the
//! mledoze/countries dataset (embedded at build time, ~1.4 MB) joined with
//! GeoNames `countryInfo.txt` (fetched lazily, TTL-cached, single-flight).
//!
//! Ported 1:1 from `zoofam26/countries-api` (NestJS).

mod model;
mod routes;
mod snapshot;

pub use model::{field_catalogue_json, Country};
pub use snapshot::{get_snapshot, Snapshot};

/// The raw embedded mledoze dataset as JSON — reused by other services
/// (e.g. books) so the binary carries exactly one copy of it.
pub fn dataset_raw_public() -> serde_json::Value {
    serde_json::from_str(include_str!("../data_countries.json"))
        .expect("embedded countries dataset must be valid JSON")
}

use actix_web::web;
use utoipa::openapi::OpenApi;

pub const SERVICE_PREFIX: &str = "/countries";
pub const UPSTREAM_COUNTRIES: &str = "https://raw.githubusercontent.com/mledoze/countries/master/countries.json";
pub const UPSTREAM_GEONAMES: &str = "https://download.geonames.org/export/dump/countryInfo.txt";
pub const CODE_PREFIX: &str = "COUNTRIES";

/// Register this service's routes under its prefix on the router.
pub fn router(cfg: &mut web::ServiceConfig) {
    routes::configure(cfg);
}

/// The service's own OpenAPI document (served at `/countries/openapi.json`).
pub fn openapi() -> OpenApi {
    routes::openapi_doc()
}

/// Machine-readable service index, merged into the master `GET /` catalogue.
pub fn index() -> serde_json::Value {
    serde_json::json!({
        "name": "countries-api",
        "prefix": SERVICE_PREFIX,
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Country reference data (250 countries) with search, filters, sorting, pagination and neighbour resolution. Served from an embedded mledoze/countries snapshot joined with GeoNames countryInfo.",
        "dataSource": {
            "provider": "mledoze/countries + GeoNames",
            "upstream": [UPSTREAM_COUNTRIES, UPSTREAM_GEONAMES],
            "requiresApiKey": false,
            "note": "The mledoze dataset is embedded in the binary; GeoNames countryInfo is refreshed at most once per TTL window.",
        },
        "endpoints": [
            { "method": "GET", "path": "/countries/fields", "description": "Static metadata about the dataset and the fields parameter" },
            { "method": "GET", "path": "/countries/regions", "description": "Reference aggregates: regions, subregions and continents" },
            { "method": "GET", "path": "/countries/countries", "description": "Search, filter, sort and paginate the whole dataset" },
            { "method": "GET", "path": "/countries/countries/{code}", "description": "One country by alpha-2/alpha-3/numeric/IOC code or name" },
            { "method": "GET", "path": "/countries/countries/{code}/neighbours", "description": "Resolved border countries" },
            { "method": "GET", "path": "/countries/countries/{code}/currencies", "description": "Currencies in circulation for a country" },
            { "method": "GET", "path": "/countries/countries/{code}/languages", "description": "Official languages of a country" },
            { "method": "GET", "path": "/countries/health", "description": "Liveness plus a real snapshot load probe" },
            { "method": "GET", "path": "/countries/health/ready", "description": "Readiness probe" },
            { "method": "GET", "path": "/countries/openapi.json", "description": "OpenAPI 3 document for this service" },
        ],
    })
}
