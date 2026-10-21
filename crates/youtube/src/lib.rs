//! youtube service — the YouTube InnerTube engine (the same JSON API that
//! youtube.com itself uses), with client-profile fallbacks and homepage
//! credential scraping. No API key or Google Cloud project required.
//!
//! Ported from `zoofam26/youtube-api` (NestJS).

mod innertube;
mod parse;
mod routes;
pub mod url;

use actix_web::web;
use utoipa::openapi::OpenApi;

pub const SERVICE_PREFIX: &str = "/youtube";
pub const CODE_PREFIX: &str = "YOUTUBE";
pub const UPSTREAM_INNERTUBE: &str = "https://www.youtube.com/youtubei/v1";
pub const YOUTUBE_HOMEPAGE: &str = "https://www.youtube.com/";

pub const SEARCH_TYPES: &[&str] = &["all", "video", "channel", "playlist"];
pub const SEARCH_SORTS: &[&str] = &["relevance", "rating", "upload_date", "view_count"];
pub const SEARCH_DURATIONS: &[&str] = &["short", "medium", "long"];
pub const SEARCH_UPLOAD_DATES: &[&str] = &["today", "this_week", "this_month", "this_year"];
pub const COMMENT_ORDERS: &[&str] = &["top", "newest"];

pub fn router(cfg: &mut web::ServiceConfig) {
    routes::configure(cfg);
}

pub fn openapi() -> OpenApi {
    routes::openapi_doc()
}

pub fn index() -> serde_json::Value {
    serde_json::json!({
        "name": "youtube-api",
        "prefix": SERVICE_PREFIX,
        "version": env!("CARGO_PKG_VERSION"),
        "description": "YouTube search, trending, video/channel/playlist details, transcripts, comments and related videos over YouTube's InnerTube API — no API key, no quota.",
        "dataSource": {
            "provider": "YouTube InnerTube (public web client credentials scraped from the homepage)",
            "upstream": [UPSTREAM_INNERTUBE, YOUTUBE_HOMEPAGE],
            "requiresApiKey": false,
            "note": "Client profiles (WEB/ANDROID/IOS) with per-endpoint fallback chains, mirroring the original service.",
        },
        "endpoints": [
            { "method": "GET", "path": "/youtube/search", "description": "Search videos/channels/playlists with type/sort/duration/upload-date filters" },
            { "method": "POST", "path": "/youtube/search", "description": "Body-driven search, for clients that cannot send long query strings" },
            { "method": "GET", "path": "/youtube/trending", "description": "Trending videos" },
            { "method": "GET", "path": "/youtube/resolve", "description": "Resolve a YouTube URL to its canonical target" },
            { "method": "GET", "path": "/youtube/videos", "description": "Batch video lookup (id=a&id=b)" },
            { "method": "GET", "path": "/youtube/videos/{id}", "description": "Video details" },
            { "method": "GET", "path": "/youtube/videos/{id}/formats", "description": "Stream formats" },
            { "method": "GET", "path": "/youtube/videos/{id}/transcript", "description": "Caption track as transcript lines" },
            { "method": "GET", "path": "/youtube/videos/{id}/comments", "description": "Comments with paging and ordering" },
            { "method": "GET", "path": "/youtube/videos/{id}/related", "description": "Related videos" },
            { "method": "GET", "path": "/youtube/channels", "description": "Batch channel lookup (id=a&id=b)" },
            { "method": "GET", "path": "/youtube/channels/{id}", "description": "Channel details (id or @handle)" },
            { "method": "GET", "path": "/youtube/channels/{id}/videos", "description": "Channel videos" },
            { "method": "GET", "path": "/youtube/channels/{id}/shorts", "description": "Channel shorts" },
            { "method": "GET", "path": "/youtube/channels/{id}/live", "description": "Channel live streams" },
            { "method": "GET", "path": "/youtube/channels/{id}/posts", "description": "Channel posts" },
            { "method": "GET", "path": "/youtube/channels/{id}/playlists", "description": "Channel playlists" },
            { "method": "GET", "path": "/youtube/channels/{id}/tabs", "description": "Arbitrary channel tab" },
            { "method": "GET", "path": "/youtube/playlists/{id}", "description": "Playlist details" },
            { "method": "GET", "path": "/youtube/playlists/{id}/videos", "description": "Playlist videos" },
            { "method": "GET", "path": "/youtube/v1/videos", "description": "Data-API-v3-style batch: id=a&id=b" },
            { "method": "GET", "path": "/youtube/v1/channels", "description": "Data-API-v3-style batch: id=a&id=b" },
            { "method": "GET", "path": "/youtube/video", "description": "Compat alias: ?id=" },
            { "method": "GET", "path": "/youtube/channel", "description": "Compat alias: ?id=" },
            { "method": "GET", "path": "/youtube/channel/videos", "description": "Compat alias: ?id=" },
            { "method": "GET", "path": "/youtube/comments", "description": "Compat alias: ?id=" },
            { "method": "GET", "path": "/youtube/playlist", "description": "Compat alias: ?id=" },
            { "method": "GET", "path": "/youtube/health", "description": "Liveness plus a real InnerTube probe" },
            { "method": "GET", "path": "/youtube/health/ready", "description": "Readiness probe" },
            { "method": "GET", "path": "/youtube/openapi.json", "description": "OpenAPI 3 document for this service" },
        ],
    })
}
