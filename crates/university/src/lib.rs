//! university service — searchable directory of world universities and their
//! domains, backed by the Hipo `university-domains-list` dataset.
//!
//! Ported 1:1 from `zoofam26/university-api` (NestJS), including the
//! deterministic id assignment (`<slug>-<cc>`, content-hashed on collision)
//! and the domain resolver with suffix candidates.

mod routes;
mod snapshot;

pub use snapshot::{get_snapshot, UniversityRecord, UniversitySnapshot};

use actix_web::web;
use utoipa::openapi::OpenApi;

pub const SERVICE_PREFIX: &str = "/university";
pub const CODE_PREFIX: &str = "UNIVERSITY";
pub const UPSTREAM_HIPO: &str =
    "https://raw.githubusercontent.com/Hipo/university-domains-list/master/world_universities_and_domains.json";

/// Sort keys accepted by `?sort=`.
pub const SORT_KEYS: &[&str] = &["name", "name_desc", "country", "country_desc", "domain"];

/// Default 24h snapshot TTL unless `CACHE_TTL_UNIVERSITY` overrides it.
fn snapshot_ttl(state: &master_common::state::AppState) -> std::time::Duration {
    if state.settings.cache_ttl_overrides.contains_key("UNIVERSITY") {
        state.settings.ttl_for("UNIVERSITY")
    } else {
        std::time::Duration::from_secs(24 * 3600)
    }
}

pub fn router(cfg: &mut web::ServiceConfig) {
    routes::configure(cfg);
}

pub fn openapi() -> OpenApi {
    routes::openapi_doc()
}

pub fn index() -> serde_json::Value {
    serde_json::json!({
        "name": "university-api",
        "prefix": SERVICE_PREFIX,
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Searchable directory of world universities with stable ids, multi-country grouping and academic domain resolution, backed by the Hipo university-domains-list dataset.",
        "dataSource": {
            "provider": "Hipo university-domains-list",
            "upstream": [UPSTREAM_HIPO],
            "requiresApiKey": false,
            "note": "The ~2 MB dataset is downloaded at most once per TTL window and fully indexed in memory.",
        },
        "endpoints": [
            { "method": "GET", "path": "/university/universities", "description": "Search and paginate universities (name substring and/or country filter)" },
            { "method": "GET", "path": "/university/countries", "description": "Countries present in the dataset, with university counts" },
            { "method": "GET", "path": "/university/universities/domain/{domain}", "description": "Resolve an academic domain (or URL) to its institution" },
            { "method": "GET", "path": "/university/universities/{id}", "description": "One university by stable id" },
            { "method": "GET", "path": "/university/universities/{id}/countries", "description": "Which countries an institution appears in" },
            { "method": "GET", "path": "/university/health", "description": "Liveness plus a real dataset probe" },
            { "method": "GET", "path": "/university/health/ready", "description": "Readiness probe" },
            { "method": "GET", "path": "/university/openapi.json", "description": "OpenAPI 3 document for this service" },
        ],
    })
}
