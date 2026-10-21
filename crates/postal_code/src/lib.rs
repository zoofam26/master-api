//! postal-code service — postal-code lookup, validation and distance over the
//! GeoNames postal datasets (lazily downloaded per country, parsed in memory)
//! with reference data from GeoNames `countryInfo.txt`.
//!
//! Ported from `zoofam26/postal-code-api` (NestJS).

mod routes;

use actix_web::web;
use utoipa::openapi::OpenApi;

pub const SERVICE_PREFIX: &str = "/postal-code";
pub const CODE_PREFIX: &str = "POSTAL";
pub const UPSTREAM_GEONAMES: &str = "https://download.geonames.org";
pub const ZIP_BASE: &str = "https://download.geonames.org/export/zip";
pub const COUNTRY_INFO_URL: &str = "https://download.geonames.org/export/dump/countryInfo.txt";
pub const DATA_SOURCE_PROVIDER: &str = "GeoNames";
pub const DATA_SOURCE_LICENCE: &str = "CC BY 4.0 (https://creativecommons.org/licenses/by/4.0/)";

/// Countries whose free GeoNames dataset is truncated (the full one ships as
/// `<CC>_full.csv.zip`); the note is surfaced in lookups, as upstream does.
pub const TRUNCATED_DATASET_NOTES: &[(&str, &str)] = &[
    ("CA", "The CA dataset contains only the first three characters of each postal code (Forward Sortation Area). Full codes are published separately as CA_full.csv.zip."),
    ("GB", "The GB dataset contains only the outward code (e.g. SW1A), not the full postcode (SW1A 1AA). Full postcodes are published separately as GB_full.csv.zip."),
    ("NL", "The NL dataset contains only the first four characters of each postal code. Full codes are published separately as NL_full.csv.zip."),
];

pub fn router(cfg: &mut web::ServiceConfig) {
    routes::configure(cfg);
}

pub fn openapi() -> OpenApi {
    routes::openapi_doc()
}

pub fn index() -> serde_json::Value {
    serde_json::json!({
        "name": "postal-code-api",
        "prefix": SERVICE_PREFIX,
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Postal-code lookup, validation, place search and great-circle distance over the GeoNames postal datasets, with per-country reference data (formats, patterns, currency) from countryInfo.txt.",
        "dataSource": {
            "provider": DATA_SOURCE_PROVIDER,
            "upstream": [ZIP_BASE, COUNTRY_INFO_URL],
            "requiresApiKey": false,
            "note": "A country's dataset is downloaded at most once per TTL window and indexed in memory; the reference table is refreshed separately.",
        },
        "endpoints": [
            { "method": "GET", "path": "/postal-code/search", "description": "Search places by postal code or name, optionally within a radius" },
            { "method": "GET", "path": "/postal-code/validate/{country}/{postalCode}", "description": "Validate a code against its country's documented pattern" },
            { "method": "GET", "path": "/postal-code/distance/{country}/{postalCode}/{country2}/{postalCode2}", "description": "Great-circle distance between two postal codes" },
            { "method": "GET", "path": "/postal-code/countries", "description": "Country reference list with filters" },
            { "method": "GET", "path": "/postal-code/countries/{code}", "description": "One country with its postal format rules" },
            { "method": "GET", "path": "/postal-code/{country}/{postalCode}", "description": "All places for one postal code" },
            { "method": "GET", "path": "/postal-code/health", "description": "Liveness plus a real upstream probe" },
            { "method": "GET", "path": "/postal-code/health/ready", "description": "Readiness probe" },
            { "method": "GET", "path": "/postal-code/openapi.json", "description": "OpenAPI 3 document for this service" },
        ],
    })
}

/// Uppercased, punctuation-free matching form of a postal code.
pub fn normalise_postal_key(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Trimmed, whitespace-collapsed, uppercased display form.
pub fn normalise_postal_display(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.trim().to_uppercase()
}

/// Great-circle distance between two WGS84 points, in kilometres (haversine).
pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const EARTH_RADIUS_KM: f64 = 6_372.8;
    let d_lat = (lat2 - lat1).to_radians();
    let d_lon = (lon2 - lon1).to_radians();
    let a = (d_lat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (d_lon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().asin();
    EARTH_RADIUS_KM * c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_keys() {
        assert_eq!(normalise_postal_key(" 90210 "), "90210");
        assert_eq!(normalise_postal_key("sw1a 1aa"), "SW1A1AA");
        assert_eq!(normalise_postal_key("100-0001"), "1000001");
    }

    #[test]
    fn normalises_display() {
        assert_eq!(normalise_postal_display(" sw1a  1aa "), "SW1A 1AA");
        assert_eq!(normalise_postal_display("90210"), "90210");
    }

    #[test]
    fn haversine_sane() {
        // Dar es Salaam -> Zanzibar is roughly 75-90 km.
        let d = haversine_km(-6.79, 39.21, -6.16, 39.19);
        assert!(d > 60.0 && d < 100.0, "got {d}");
        // Zero distance for identical points.
        assert!(haversine_km(0.0, 0.0, 0.0, 0.0) < 1e-9);
    }
}
