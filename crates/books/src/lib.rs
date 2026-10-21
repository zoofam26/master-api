//! books service — Open Library-backed book search, works, authors, ISBN
//! lookups, subject browsing and country reference.
//!
//! Ported 1:1 from `zoofam26/book-api` (NestJS). The upstream client honours
//! Open Library's published usage guidelines: requests are serialised with a
//! minimum spacing, cached aggressively, and retried only on transient
//! failures.

mod isbn;
mod olparse;
mod routes;

use actix_web::web;
use utoipa::openapi::OpenApi;

pub const SERVICE_PREFIX: &str = "/books";
pub const CODE_PREFIX: &str = "BOOKS";
pub const UPSTREAM_OPENLIBRARY: &str = "https://openlibrary.org";
pub const UPSTREAM_COVERS: &str = "https://covers.openlibrary.org";
pub const UPSTREAM_NAME: &str = "openlibrary";

/// Cache TTLs mirror the original service's defaults (ms → s).
pub const TTL_SEARCH_SECS: u64 = 60;
pub const TTL_WORK_SECS: u64 = 3_600;
pub const TTL_AUTHOR_SECS: u64 = 3_600;
pub const TTL_SUBJECT_SECS: u64 = 21_600;
pub const TTL_ISBN_SECS: u64 = 86_400;
pub const TTL_COUNTRIES_SECS: u64 = 86_400;

pub fn router(cfg: &mut web::ServiceConfig) {
    routes::configure(cfg);
}

pub fn openapi() -> OpenApi {
    routes::openapi_doc()
}

pub fn index() -> serde_json::Value {
    serde_json::json!({
        "name": "book-api",
        "prefix": SERVICE_PREFIX,
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Book search, work/author records, ISBN resolution and subject browsing over Open Library, plus a country reference listing.",
        "dataSource": {
            "provider": "Open Library (Internet Archive)",
            "upstream": [UPSTREAM_OPENLIBRARY, UPSTREAM_COVERS],
            "requiresApiKey": false,
            "note": "Requests are serialised with a 1.1s minimum spacing and cached per Open Library's usage guidelines.",
        },
        "endpoints": [
            { "method": "GET", "path": "/books/books/search", "description": "Search works (q/title/author/subject required)" },
            { "method": "GET", "path": "/books/books/isbn/{isbn}", "description": "ISBN-10/13 lookup with normalisation and check-digit validation" },
            { "method": "GET", "path": "/books/books/{key}", "description": "One work, e.g. OL45804W" },
            { "method": "GET", "path": "/books/books/{key}/authors", "description": "Authors credited on a work, with resolved names" },
            { "method": "GET", "path": "/books/authors/{authorKey}", "description": "One author, e.g. OL26320A" },
            { "method": "GET", "path": "/books/subjects", "description": "Browse the works under one subject" },
            { "method": "GET", "path": "/books/countries", "description": "Country reference listing" },
            { "method": "GET", "path": "/books/health", "description": "Liveness plus a real Open Library probe" },
            { "method": "GET", "path": "/books/health/ready", "description": "Readiness probe" },
            { "method": "GET", "path": "/books/openapi.json", "description": "OpenAPI 3 document for this service" },
        ],
    })
}
