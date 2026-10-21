//! Route handlers for the youtube service.

use std::sync::Arc;

use actix_web::web::{self, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use utoipa::{IntoParams, OpenApi};

use master_common::envelope::{meta, meta_extend};
use master_common::error::ApiError;
use master_common::state::{cache_names, AppState};

use crate::innertube::{self, EP_BROWSE, EP_GUIDE, EP_NEXT, EP_PLAYER, EP_RESOLVE, EP_SEARCH};
use crate::url;
use crate::parse::{
    build_search_params, collect_by_key, parse_search_results,
    parse_video_renderer, read_continuation, read_text,
};
use crate::{CODE_PREFIX, SERVICE_PREFIX, UPSTREAM_INNERTUBE};

const UPSTREAM_NAME: &str = "youtube-innertube";

fn meta_json(cached: bool, duration_ms: u64, extra: Value) -> Value {
    let base = meta(UPSTREAM_INNERTUBE, UPSTREAM_NAME, cached, duration_ms);
    meta_extend(base, extra)
}

fn envelope(data: Value, meta: Value) -> Value {
    json!({ "success": true, "data": data, "meta": meta })
}

// -------------------------------------------------------------- query DTOs

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LocaleQuery {
    /// Content language (e.g. `en`).
    pub hl: Option<String>,
    /// Content region (e.g. `US`).
    pub gl: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PaginationQuery {
    pub hl: Option<String>,
    pub gl: Option<String>,
    /// Page size.
    #[serde(rename = "maxResults")]
    pub max_results: Option<usize>,
    /// Alias for maxResults.
    pub limit: Option<usize>,
    /// Continuation token from a previous page.
    #[serde(rename = "pageToken")]
    pub page_token: Option<String>,
    /// Alias for pageToken.
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SearchQuery {
    pub hl: Option<String>,
    pub gl: Option<String>,
    #[serde(rename = "maxResults")]
    pub max_results: Option<usize>,
    pub limit: Option<usize>,
    #[serde(rename = "pageToken")]
    pub page_token: Option<String>,
    pub cursor: Option<String>,
    /// Free-text query (or `query`).
    pub q: Option<String>,
    /// Alias for `q`.
    pub query: Option<String>,
    /// all | video | channel | playlist.
    #[serde(rename = "type")]
    pub search_type: Option<String>,
    /// relevance | rating | upload_date | view_count.
    pub sort: Option<String>,
    /// short | medium | long.
    pub duration: Option<String>,
    /// today | this_week | this_month | this_year.
    #[serde(rename = "uploadDate")]
    pub upload_date: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[allow(dead_code)]
pub struct VideoQuery {
    pub hl: Option<String>,
    pub gl: Option<String>,
    /// `details`, `formats`, `microformat` — defaults to all.
    pub part: Option<String>,
    /// Include stream formats (default true).
    #[serde(rename = "includeFormats")]
    pub include_formats: Option<bool>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[allow(dead_code)]
pub struct VideoListQuery {
    pub hl: Option<String>,
    pub gl: Option<String>,
    /// Repeated `id` parameters, Data API v3 style.
    pub id: Option<CommaList>,
    pub part: Option<String>,
    pub max_results: Option<usize>,
    #[serde(rename = "includeFormats")]
    pub include_formats: Option<bool>,
}

/// Accepts repeated `id=` values or one comma-separated list.
#[derive(Debug, Clone, utoipa::ToSchema)]
pub struct CommaList(pub Vec<String>);

impl<'de> Deserialize<'de> for CommaList {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = CommaList;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a comma-separated string of ids")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(CommaList(v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()))
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[allow(dead_code)]
pub struct CommentsQuery {
    pub hl: Option<String>,
    pub gl: Option<String>,
    #[serde(rename = "maxResults")]
    pub max_results: Option<usize>,
    pub limit: Option<usize>,
    #[serde(rename = "pageToken")]
    pub page_token: Option<String>,
    pub cursor: Option<String>,
    /// top (default) or newest.
    pub order: Option<String>,
    #[serde(rename = "includeReplies")]
    pub include_replies: Option<bool>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TranscriptQuery {
    pub hl: Option<String>,
    pub gl: Option<String>,
    /// Preferred caption language code (e.g. `en`).
    pub lang: Option<String>,
    /// Prefer manually-created captions over ASR.
    #[allow(dead_code)]
    pub lang_only: Option<bool>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ChannelTabQuery {
    pub hl: Option<String>,
    pub gl: Option<String>,
    #[serde(rename = "maxResults")]
    pub max_results: Option<usize>,
    pub limit: Option<usize>,
    #[serde(rename = "pageToken")]
    pub page_token: Option<String>,
    /// videos (default) | shorts | live | streams | posts | playlists | community.
    pub tab: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[allow(dead_code)]
pub struct ResolveQuery {
    /// A YouTube URL: watch, shorts, channel, handle or playlist.
    pub url: Option<String>,
    pub hl: Option<String>,
    pub gl: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SearchBody {
    pub q: Option<String>,
    pub query: Option<String>,
    #[serde(rename = "type")]
    pub search_type: Option<String>,
    pub sort: Option<String>,
    pub duration: Option<String>,
    #[serde(rename = "uploadDate")]
    pub upload_date: Option<String>,
    #[serde(rename = "maxResults")]
    pub max_results: Option<usize>,
    pub limit: Option<usize>,
    #[serde(rename = "pageToken")]
    pub page_token: Option<String>,
    pub hl: Option<String>,
    pub gl: Option<String>,
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    let scope = web::scope(SERVICE_PREFIX)
        .route("/search", web::post().to(search_post))
        .route("/search", web::get().to(search))
        .route("/trending", web::get().to(trending))
        .route("/resolve", web::get().to(resolve))
        .route("/v1/videos", web::get().to(videos_batch))
        .route("/v1/channels", web::get().to(channels_batch))
        .route("/videos/{id}/formats", web::get().to(video_formats))
        .route("/videos/{id}/transcript", web::get().to(transcript))
        .route("/videos/{id}/comments", web::get().to(comments))
        .route("/videos/{id}/related", web::get().to(related))
        .route("/videos/{id}", web::get().to(video_detail))
        .route("/channels/{id}/videos", web::get().to(channel_videos))
        .route("/channels/{id}/shorts", web::get().to(channel_shorts))
        .route("/channels/{id}/live", web::get().to(channel_live))
        .route("/channels/{id}/posts", web::get().to(channel_posts))
        .route("/channels/{id}/playlists", web::get().to(channel_playlists))
        .route("/channels/{id}/tabs", web::get().to(channel_tabs))
        .route("/channels/{id}", web::get().to(channel_detail))
        .route("/playlists/{id}/videos", web::get().to(playlist_videos))
        .route("/playlists/{id}", web::get().to(playlist_detail))
        // Compat aliases
        .route("/video", web::get().to(compat_video))
        .route("/channel/videos", web::get().to(compat_channel_videos))
        .route("/channel", web::get().to(compat_channel))
        .route("/comments", web::get().to(compat_comments))
        .route("/playlist", web::get().to(compat_playlist))
        .route("/videos", web::get().to(videos_batch))
        .route("/channels", web::get().to(channels_batch))
        .route("/health/ready", web::get().to(health_ready))
        .route("/health", web::get().to(health));
    cfg.service(scope);
}

// ------------------------------------------------------------------ search

fn page(query_page_token: Option<String>, query_cursor: Option<String>) -> Option<String> {
    query_page_token.or(query_cursor)
}

fn page_size(max_results: Option<usize>, limit: Option<usize>, default: usize, max: usize) -> usize {
    max_results.or(limit).unwrap_or(default).clamp(1, max)
}

async fn do_search(
    state: &AppState,
    term: &str,
    search_type: Option<&str>,
    sort: Option<&str>,
    duration: Option<&str>,
    upload_date: Option<&str>,
    continuation: Option<String>,
    limit: usize,
    hl: &str,
    gl: &str,
) -> Result<Value, ApiError> {
    for (name, value, allowed) in [
        ("type", search_type, crate::SEARCH_TYPES),
        ("sort", sort, crate::SEARCH_SORTS),
        ("duration", duration, crate::SEARCH_DURATIONS),
        ("uploadDate", upload_date, crate::SEARCH_UPLOAD_DATES),
    ] {
        if let Some(v) = value {
            if !allowed.contains(&v) {
                return Err(ApiError::validation_details(
                    format!("{CODE_PREFIX}_INVALID_REQUEST"),
                    format!("Unknown {name} \"{v}\""),
                    json!({ name: v, "allowed": allowed }),
                ));
            }
        }
    }
    let params = build_search_params(sort, upload_date, search_type, duration);

    let cache = state.json_cache(cache_names::YOUTUBE);
    let cache_key = format!(
        "search:{term}:{:?}:{:?}:{:?}:{:?}:{:?}:{limit}:{hl}:{gl}",
        search_type, sort, duration, upload_date, continuation
    );
    if let Some(v) = cache.get(&cache_key).await {
        return Ok(trim_search_page(v.as_ref().clone(), limit));
    }

    let body = match &continuation {
        Some(token) => json!({ "continuation": token, "params": params }),
        None => json!({ "query": term, "params": params }),
    };
    let started = std::time::Instant::now();
    let raw = innertube::post(state, EP_SEARCH, body, hl, gl, None).await?;
    let mut parsed = parse_search_results(&raw, term, search_type);
    // Apply the requested page size to videos when the caller limited it.
    let trimmed = trim_search_page(parsed.clone(), limit);
    parsed["pageInfo"]["requestedLimit"] = json!(limit);
    cache.insert(cache_key, Arc::new(parsed.clone())).await;
    let _ = started;
    Ok(trimmed)
}

fn trim_search_page(mut page: Value, limit: usize) -> Value {
    let truncated_len = {
        let contents = page.get_mut("contents").and_then(|c| c.as_array_mut());
        match contents {
            Some(arr) if arr.len() > limit => {
                arr.truncate(limit);
                Some(arr.len())
            }
            _ => None,
        }
    };
    if let Some(len) = truncated_len {
        if let Some(pi) = page.get_mut("pageInfo").and_then(|p| p.as_object_mut()) {
            pi.insert("results".into(), json!(len));
        }
    }
    page
}

/// `GET /youtube/search`
#[utoipa::path(get, path = "/youtube/search", context_path = "/youtube", params(SearchQuery),
    responses((status = 200, description = "Search results")))]
async fn search(
    state: web::Data<AppState>,
    query: web::Query<SearchQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let term = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| query.query.as_deref().map(str::trim).filter(|s| !s.is_empty()));
    let Some(term) = term else {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "`q` is required, e.g. /youtube/search?q=lofi hip hop".to_string(),
            json!({ "accepted": ["q", "query"] }),
        ));
    };
    let limit = page_size(query.max_results, query.limit, 20, 60);
    let data = do_search(
        &state,
        term,
        query.search_type.as_deref(),
        query.sort.as_deref(),
        query.duration.as_deref(),
        query.upload_date.as_deref(),
        page(query.page_token.clone(), query.cursor.clone()),
        limit,
        query.hl.as_deref().unwrap_or("en"),
        query.gl.as_deref().unwrap_or("US"),
    )
    .await?;
    let meta = meta_json(false, started.elapsed().as_millis() as u64, json!({}));
    Ok(Json(envelope(data, meta)))
}

/// `POST /youtube/search` — body-driven search.
#[utoipa::path(post, path = "/youtube/search", context_path = "/youtube", request_body = SearchBody,
    responses((status = 200, description = "Search results")))]
async fn search_post(state: web::Data<AppState>, body: web::Json<SearchBody>) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let term = body
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| body.query.as_deref().map(str::trim).filter(|s| !s.is_empty()));
    let Some(term) = term else {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "`q` is required in the request body".to_string(),
            json!({ "accepted": ["q", "query"] }),
        ));
    };
    let limit = page_size(body.max_results, body.limit, 20, 60);
    let data = do_search(
        &state,
        term,
        body.search_type.as_deref(),
        body.sort.as_deref(),
        body.duration.as_deref(),
        body.upload_date.as_deref(),
        body.page_token.clone(),
        limit,
        body.hl.as_deref().unwrap_or("en"),
        body.gl.as_deref().unwrap_or("US"),
    )
    .await?;
    let meta = meta_json(false, started.elapsed().as_millis() as u64, json!({}));
    Ok(Json(envelope(data, meta)))
}

// ---------------------------------------------------------------- trending

/// `GET /youtube/trending`
#[utoipa::path(get, path = "/youtube/trending", context_path = "/youtube", params(PaginationQuery),
    responses((status = 200, description = "Trending videos")))]
async fn trending(state: web::Data<AppState>, query: web::Query<PaginationQuery>) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let limit = page_size(query.max_results, query.limit, 30, 50);
    let hl = query.hl.as_deref().unwrap_or("en");
    let gl = query.gl.as_deref().unwrap_or("US");

    let cache = state.json_cache(cache_names::YOUTUBE);
    let cache_key = format!("trending:{limit}:{hl}:{gl}");
    if let Some(v) = cache.get(&cache_key).await {
        let meta = meta_json(true, started.elapsed().as_millis() as u64, json!({}));
        return Ok(Json(envelope(v.as_ref().clone(), meta)));
    }

    // YouTube's FEtrending browse endpoint now rejects anonymous requests
    // (HTTP 400 verified), so trending is approximated the honest way: the
    // most-viewed videos uploaded today across a rotating set of broad seeds.
    const SEEDS: &[&str] = &["official music video", "news", "trailer", "highlights", "live"];
    let mut videos: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for seed in SEEDS {
        if videos.len() >= limit {
            break;
        }
        let body = json!({
            "query": seed,
            "params": crate::parse::build_search_params(Some("view_count"), Some("today"), None, None),
        });
        if let Ok(raw) = innertube::post(&state, EP_SEARCH, body, hl, gl, None).await {
            let parsed = crate::parse::parse_search_results(&raw, seed, Some("video"));
            if let Some(contents) = parsed.get("contents").and_then(|c| c.as_array()) {
                for item in contents {
                    if videos.len() >= limit {
                        break;
                    }
                    let video = item.get("video").cloned().unwrap_or(Value::Null);
                    let id = video.get("videoId").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    if !id.is_empty() && seen.insert(id) {
                        videos.push(json!({ "type": "video", "id": video.get("videoId"), "video": video }));
                    }
                }
            }
        }
    }
    let data = json!({
        "videos": videos,
        "count": videos.len(),
        "note": "Approximated via the most-viewed videos uploaded today across broad seed queries; YouTube's trending feed now requires a signed-in session.",
    });
    cache.insert(cache_key, Arc::new(data.clone())).await;
    let meta = meta_json(false, started.elapsed().as_millis() as u64, json!({}));
    Ok(Json(envelope(data, meta)))
}

// ----------------------------------------------------------------- resolve

/// `GET /youtube/resolve?url=`
#[utoipa::path(get, path = "/youtube/resolve", context_path = "/youtube", params(ResolveQuery),
    responses((status = 200, description = "Resolved target")))]
async fn resolve(state: web::Data<AppState>, query: web::Query<ResolveQuery>) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let Some(url) = query.url.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "`url` is required, e.g. /youtube/resolve?url=https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string(),
            json!({ "parameter": "url" }),
        ));
    };
    // YouTube's resolve_url now 404s for watch URLs on anonymous clients, so
    // resolution is done locally from the URL shape, with a live verification
    // call where it is cheap.
    let parsed = url::parse_youtube_url(url);
    let target = match parsed {
        Some(url::ResolvedUrl::Video { id }) => {
            // Verify via the cheapest real call (player response).
            player_response(&state, &id, "en", "US").await?;
            json!({ "type": "video", "videoId": id, "url": format!("https://www.youtube.com/watch?v={id}") })
        }
        Some(url::ResolvedUrl::Channel { id }) => {
            json!({ "type": "channel", "channelId": id, "url": format!("https://www.youtube.com/channel/{id}") })
        }
        Some(url::ResolvedUrl::Handle { handle }) => {
            json!({ "type": "channel", "handle": handle, "url": format!("https://www.youtube.com/{handle}") })
        }
        Some(url::ResolvedUrl::Playlist { id }) => {
            json!({ "type": "playlist", "playlistId": id, "url": format!("https://www.youtube.com/playlist?list={id}") })
        }
        None => {
            return Err(ApiError::not_found_details(
                format!("{CODE_PREFIX}_NOT_FOUND"),
                format!("Could not resolve \"{url}\" to a known YouTube target"),
                json!({ "url": url, "accepted": ["watch URLs", "shorts", "channels", "@handles", "playlists"] }),
            ));
        }
    };
    let meta = meta_json(false, started.elapsed().as_millis() as u64, json!({ "requested": url }));
    Ok(Json(envelope(target, meta)))
}

// ------------------------------------------------------------------ videos

fn extract_player_details(raw: &Value) -> Value {
    let details = raw.get("videoDetails").cloned().unwrap_or(json!({}));
    let micro = raw.get("microformat").cloned().unwrap_or(json!({}));
    let thumbs: Vec<Value> = details
        .get("thumbnail")
        .and_then(|t| t.get("thumbnails"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .map(|t| json!({ "url": t.get("url"), "width": t.get("width"), "height": t.get("height") }))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "videoId": details.get("videoId").cloned().unwrap_or(Value::Null),
        "title": details.get("title").cloned().unwrap_or(Value::Null),
        "description": details.get("shortDescription").cloned().unwrap_or(Value::Null),
        "channelId": details.get("channelId").cloned().unwrap_or(Value::Null),
        "author": details.get("author").cloned().unwrap_or(Value::Null),
        "lengthSeconds": details.get("lengthSeconds").and_then(|v| v.as_str()).and_then(|s| s.parse::<i64>().ok()),
        "viewCount": details.get("viewCount").and_then(|v| v.as_str()).and_then(|s| s.parse::<i64>().ok()),
        "keywords": details.get("keywords").cloned().unwrap_or(json!([])),
        "isLive": details.get("isLiveContent").cloned().unwrap_or(Value::Null),
        "thumbnails": thumbs,
        "publishDate": micro.pointer("/playerMicroformatRenderer/publishDate").cloned().unwrap_or(Value::Null),
        "uploadDate": micro.pointer("/playerMicroformatRenderer/uploadDate").cloned().unwrap_or(Value::Null),
        "category": micro.pointer("/playerMicroformatRenderer/category").cloned().unwrap_or(Value::Null),
        "viewCountMicroformat": micro.pointer("/playerMicroformatRenderer/viewCount").cloned().unwrap_or(Value::Null),
        "familySafe": micro.pointer("/playerMicroformatRenderer/isFamilySafe").cloned().unwrap_or(Value::Null),
    })
}

fn extract_formats(raw: &Value) -> Vec<Value> {
    let pick = |list: Option<&Vec<Value>>, kind: &str| -> Vec<Value> {
        list.map(|a| {
            a.iter()
                .map(|f| {
                    json!({
                        "kind": kind,
                        "itag": f.get("itag").cloned().unwrap_or(Value::Null),
                        "mimeType": f.get("mimeType").cloned().unwrap_or(Value::Null),
                        "qualityLabel": f.get("qualityLabel").cloned().unwrap_or(Value::Null),
                        "bitrate": f.get("bitrate").cloned().unwrap_or(Value::Null),
                        "contentLength": f.get("contentLength").cloned().unwrap_or(Value::Null),
                        "approxDurationMs": f.get("approxDurationMs").cloned().unwrap_or(Value::Null),
                        "url": f.get("url").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
    };
    let mut out = pick(raw.pointer("/streamingData/formats").and_then(|v| v.as_array()), "muxed");
    out.extend(pick(raw.pointer("/streamingData/adaptiveFormats").and_then(|v| v.as_array()), "adaptive"));
    out
}

async fn player_response(state: &AppState, id: &str, hl: &str, gl: &str) -> Result<Value, ApiError> {
    validate_video_id(id)?;
    let cache = state.json_cache(cache_names::YOUTUBE);
    let key = format!("player:{id}:{hl}:{gl}");
    if let Some(v) = cache.get(&key).await {
        return Ok(v.as_ref().clone());
    }
    let raw = innertube::post(state, EP_PLAYER, json!({ "videoId": id }), hl, gl, None).await?;
    let status = raw.pointer("/playabilityStatus/status").and_then(|v| v.as_str()).unwrap_or("OK");
    if status != "OK" && status != "LIVE_STREAM_OFFLINE" {
        return Err(ApiError::not_found_details(
            format!("{CODE_PREFIX}_UNPLAYABLE"),
            format!("YouTube reports the video as {status}"),
            json!({ "videoId": id, "status": status, "reason": raw.pointer("/playabilityStatus/reason") }),
        ));
    }
    cache.insert(key, Arc::new(raw.clone())).await;
    Ok(raw)
}

fn validate_video_id(id: &str) -> Result<(), ApiError> {
    if !id.is_empty() && id.len() <= 20 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        Ok(())
    } else {
        Err(ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            format!("\"{id}\" is not a YouTube video id"),
            json!({ "videoId": id }),
        ))
    }
}

/// `GET /youtube/videos/{id}`
#[utoipa::path(get, path = "/youtube/videos/{id}", context_path = "/youtube", params(VideoQuery),
    responses((status = 200, description = "Video details"), (status = 404, description = "Unknown video")))]
async fn video_detail(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<VideoQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let id = path.into_inner();
    let raw = player_response(&state, &id, query.hl.as_deref().unwrap_or("en"), query.gl.as_deref().unwrap_or("US")).await?;
    let include_formats = query.include_formats.unwrap_or(true);
    let mut data = json!({ "video": extract_player_details(&raw) });
    if include_formats {
        data["formats"] = json!(extract_formats(&raw));
    }
    let meta = meta_json(false, started.elapsed().as_millis() as u64, json!({}));
    Ok(Json(envelope(data, meta)))
}

/// `GET /youtube/videos/{id}/formats`
#[utoipa::path(get, path = "/youtube/videos/{id}/formats", context_path = "/youtube", params(VideoQuery),
    responses((status = 200, description = "Stream formats")))]
async fn video_formats(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<VideoQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let id = path.into_inner();
    let raw = player_response(&state, &id, query.hl.as_deref().unwrap_or("en"), query.gl.as_deref().unwrap_or("US")).await?;
    let formats = extract_formats(&raw);
    let count = formats.len();
    let meta = meta_json(false, started.elapsed().as_millis() as u64, json!({}));
    Ok(Json(envelope(json!({ "videoId": id, "count": count, "formats": formats }), meta)))
}

/// `GET /youtube/videos` — batch lookup.
#[utoipa::path(get, path = "/youtube/videos", context_path = "/youtube", params(VideoListQuery),
    responses((status = 200, description = "Batch video lookup")))]
async fn videos_batch(
    state: web::Data<AppState>,
    query: web::Query<VideoListQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let ids = query.id.clone().map(|c| c.0).ok_or_else(|| {
        ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "`id` is required, e.g. /youtube/videos?id=a&id=b".to_string(),
            json!({ "parameter": "id" }),
        )
    })?;
    if ids.len() > 50 {
        return Err(ApiError::validation(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "at most 50 ids per request",
        ));
    }
    let hl = query.hl.as_deref().unwrap_or("en");
    let gl = query.gl.as_deref().unwrap_or("US");
    let mut videos = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    for id in &ids {
        match player_response(&state, id, hl, gl).await {
            Ok(raw) => videos.push(extract_player_details(&raw)),
            Err(e) => errors.push(json!({ "videoId": id, "error": e.to_string() })),
        }
    }
    let meta = meta_json(false, started.elapsed().as_millis() as u64, json!({}));
    Ok(Json(envelope(json!({ "videos": videos, "errors": errors, "count": videos.len() }), meta)))
}

/// Caption formats to try, in order of preference. YouTube's timedtext
/// endpoint periodically drops support for a format or requires additional
/// tokens for the JSON variant, so the route falls back through the well-known
/// representations: `json3` → `srv3` XML → `srv1` XML (the bare URL default).
const CAPTION_FORMATS: [&str; 3] = ["json3", "srv3", "srv1"];

/// Fetch a caption track and normalise it into `{startMs, durationMs, text}`
/// lines, trying every [`CAPTION_FORMATS`] representation before giving up.
async fn fetch_caption_lines(state: &AppState, track_url: &str) -> Result<Vec<Value>, ApiError> {
    let candidates: Vec<String> = if track_url.contains("fmt=") {
        vec![track_url.to_string()]
    } else {
        CAPTION_FORMATS
            .iter()
            .map(|fmt| format!("{track_url}&fmt={fmt}"))
            .chain(std::iter::once(track_url.to_string()))
            .collect()
    };

    let mut last_body = String::new();
    for url in &candidates {
        let body = state
            .http
            .get(url)
            .header("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
            .send()
            .await
            .map_err(|e| ApiError::upstream(format!("{CODE_PREFIX}_UNREACHABLE"), format!("Could not fetch the caption track: {e}")))?;
        let text = body.text().await.unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }

        // 1) JSON (json3).
        if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
            if let Some(lines) = json3_lines(&parsed) {
                if !lines.is_empty() {
                    return Ok(lines);
                }
            }
        }
        // 2) XML (srv3 / srv1).
        if text.contains("<transcript") || text.contains("<timedtext") {
            let lines = xml_caption_lines(&text);
            if !lines.is_empty() {
                return Ok(lines);
            }
        }
        last_body = text;
    }

    let snippet: String = last_body.chars().take(120).collect();
    Err(ApiError::upstream(
        format!("{CODE_PREFIX}_PARSE_ERROR"),
        format!("Caption track was not parseable (tried {} formats). Upstream body: {snippet}", CAPTION_FORMATS.len()),
    ))
}

/// Extract lines from a `json3` payload.
fn json3_lines(parsed: &Value) -> Option<Vec<Value>> {
    let events = parsed.get("events")?.as_array()?;
    Some(
        events
            .iter()
            .filter_map(|e| {
                let start = e.get("tStartMs").and_then(|v| v.as_i64())?;
                let dur = e.get("dDurationMs").and_then(|v| v.as_i64()).unwrap_or(0);
                let text: String = e
                    .get("segs")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|s| s.get("utf8").and_then(|t| t.as_str())).collect::<Vec<_>>().join(""))
                    .unwrap_or_default();
                if text.trim().is_empty() {
                    return None;
                }
                Some(json!({ "startMs": start, "durationMs": dur, "text": text }))
            })
            .collect(),
    )
}

/// Decode the handful of entities timedtext XML actually uses.
fn xml_unescape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let entity_end = tail.find(';').map(|i| i + 1).unwrap_or(0);
        let entity = &tail[..entity_end];
        let decoded = match entity {
            "&amp;" => Some('&'),
            "&lt;" => Some('<'),
            "&gt;" => Some('>'),
            "&quot;" => Some('"'),
            "&apos;" => Some('\''),
            "&nbsp;" => Some('\u{a0}'),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[entity_end..];
            }
            None => {
                // Numeric (&#39; decimal, &#x27; hex) or unknown: keep raw when unresolvable.
                let numeric = if let Some(hex) = entity.strip_prefix("&#x").and_then(|s| s.strip_suffix(';')) {
                    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                } else if let Some(dec) = entity.strip_prefix("&#").and_then(|s| s.strip_suffix(';')) {
                    dec.parse::<u32>().ok().and_then(char::from_u32)
                } else {
                    None
                };
                match numeric {
                    Some(c) => {
                        out.push(c);
                        rest = &tail[entity_end..];
                    }
                    None => {
                        out.push('&');
                        rest = &tail[1..];
                    }
                }
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parse `srv1` (`<transcript><text start dur>…`) and `srv3`
/// (`<timedtext><body><p t d><s>…`) XML shapes with a tolerant scanner.
fn xml_caption_lines(xml: &str) -> Vec<Value> {
    let mut lines = Vec::new();
    let srv1 = regex::Regex::new(r#"(?s)<text[^>]*start="([\d.]+)"[^>]*dur="([\d.]+)"[^>]*>(.*?)</text>"#).expect("srv1 regex");
    if srv1.is_match(xml) {
        for cap in srv1.captures_iter(xml) {
            let text = xml_unescape(&cap[3]);
            if text.trim().is_empty() {
                continue;
            }
            let start = cap[1].parse::<f64>().unwrap_or(0.0);
            let dur = cap[2].parse::<f64>().unwrap_or(0.0);
            lines.push(json!({
                "startMs": (start * 1000.0) as i64,
                "durationMs": (dur * 1000.0) as i64,
                "text": text,
            }));
        }
        return lines;
    }

    let para = regex::Regex::new(r#"(?s)<p[^>]*t="(\d+)"[^>]*d="(\d+)"[^>]*>(.*?)</p>"#).expect("srv3 regex");
    let seg = regex::Regex::new(r"(?s)<s[^>]*>(.*?)</s>").expect("srv3 seg regex");
    for cap in para.captures_iter(xml) {
        let inner = &cap[3];
        let text = if seg.is_match(inner) {
            seg.captures_iter(inner)
                .filter_map(|s| s.get(1).map(|m| m.as_str().to_string()))
                .collect::<Vec<_>>()
                .join("")
        } else {
            inner.to_string()
        };
        let text = xml_unescape(&text);
        if text.trim().is_empty() {
            continue;
        }
        lines.push(json!({
            "startMs": cap[1].parse::<i64>().unwrap_or(0),
            "durationMs": cap[2].parse::<i64>().unwrap_or(0),
            "text": text,
        }));
    }
    lines
}

/// `GET /youtube/videos/{id}/transcript`
#[utoipa::path(get, path = "/youtube/videos/{id}/transcript", context_path = "/youtube", params(TranscriptQuery),
    responses((status = 200, description = "Transcript lines"), (status = 404, description = "No captions")))]
async fn transcript(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<TranscriptQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let id = path.into_inner();
    let hl = query.hl.as_deref().unwrap_or("en");
    let raw = player_response(&state, &id, hl, query.gl.as_deref().unwrap_or("US")).await?;
    let tracks = raw
        .pointer("/captions/playerCaptionsTracklistRenderer/captionTracks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if tracks.is_empty() {
        return Err(ApiError::not_found_details(
            format!("{CODE_PREFIX}_NOT_FOUND"),
            format!("No caption tracks available for {id}"),
            json!({ "videoId": id }),
        ));
    }
    let pick = |lang: Option<&str>| -> Value {
        match lang {
            Some(l) => tracks
                .iter()
                .find(|t| t.get("languageCode").and_then(|x| x.as_str()).map(|c| c.starts_with(l)).unwrap_or(false))
                .cloned()
                .unwrap_or_else(|| tracks[0].clone()),
            None => tracks
                .iter()
                .find(|t| t.get("kind").and_then(|x| x.as_str()) != Some("asr"))
                .cloned()
                .unwrap_or_else(|| tracks[0].clone()),
        }
    };
    let track = pick(query.lang.as_deref());
    let track_url = track
        .get("baseUrl")
        .and_then(|v| v.as_str())
        .map(|u| if u.starts_with("//") { format!("https:{u}") } else { u.to_string() })
        .ok_or_else(|| ApiError::upstream(format!("{CODE_PREFIX}_UPSTREAM"), "Caption track has no baseUrl"))?;

    let lines = fetch_caption_lines(&state, &track_url).await?;

    let data = json!({
        "videoId": id,
        "language": track.get("languageCode").cloned().unwrap_or(Value::Null),
        "captionTrackName": track.get("name").and_then(|n| read_text(Some(n))),
        "generated": track.get("kind").and_then(|v| v.as_str()) == Some("asr"),
        "availableLanguages": tracks.iter().filter_map(|t| t.get("languageCode").cloned()).collect::<Vec<_>>(),
        "lines": lines,
    });
    let meta = meta_json(false, started.elapsed().as_millis() as u64, json!({}));
    Ok(Json(envelope(data, meta)))
}

/// `GET /youtube/videos/{id}/comments`
#[utoipa::path(get, path = "/youtube/videos/{id}/comments", context_path = "/youtube", params(CommentsQuery),
    responses((status = 200, description = "Comments")))]
async fn comments(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<CommentsQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let id = path.into_inner();
    let hl = query.hl.as_deref().unwrap_or("en");
    let gl = query.gl.as_deref().unwrap_or("US");
    let limit = page_size(query.max_results, query.limit, 20, 50);
    let continuation = page(query.page_token.clone(), query.cursor.clone());
    let (data, was_cached) = comments_handler(&state, &id, continuation, limit, hl, gl).await?;
    let meta = meta_json(was_cached, started.elapsed().as_millis() as u64, json!({}));
    Ok(Json(envelope(data, meta)))
}

async fn comments_handler(
    state: &AppState,
    id: &str,
    continuation: Option<String>,
    limit: usize,
    hl: &str,
    gl: &str,
) -> Result<(Value, bool), ApiError> {
    validate_video_id(id)?;

    let cache = state.json_cache(cache_names::YOUTUBE);
    let cache_key = format!("comments:{id}:{:?}:{limit}:{hl}:{gl}", continuation);
    if let Some(v) = cache.get(&cache_key).await {
        return Ok((v.as_ref().clone(), true));
    }

    let raw = match &continuation {
        Some(token) => innertube::post(&state, EP_NEXT, json!({ "continuation": token }), hl, gl, None).await?,
        None => {
            let initial = innertube::post(&state, EP_NEXT, json!({ "videoId": id }), hl, gl, None).await?;
            // The comments continuation lives under the item section of the
            // comments engagement panel.
            let token = find_comments_continuation(&initial).ok_or_else(|| {
                ApiError::not_found_details(
                    format!("{CODE_PREFIX}_NOT_FOUND"),
                    format!("No comments section available for {id}"),
                    json!({ "videoId": id }),
                )
            })?;
            innertube::post(&state, EP_NEXT, json!({ "continuation": token }), hl, gl, None).await?
        }
    };

    let comments: Vec<Value> = collect_by_key(&raw, &["commentEntityPayload"], limit)
        .into_iter()
        .filter_map(|(_, node)| {
            Some(json!({
                "id": node.get("properties").and_then(|p| p.get("commentId")).cloned().unwrap_or(Value::Null),
                "author": node.pointer("/author/displayName").cloned().unwrap_or(Value::Null),
                "authorIsChannelOwner": node.pointer("/author/isCreator").cloned().unwrap_or(Value::Null),
                "text": node.pointer("/content/content").cloned().unwrap_or(Value::Null),
                "publishedTimeText": node.pointer("/publishedTimeText/publishedTimeText").and_then(|v| v.as_str()).map(|s| json!(s)).unwrap_or(Value::Null),
                "likeCount": node.pointer("/toolbar/likeCountNotliked").and_then(|v| v.as_str()).and_then(|s| s.replace(',', "").parse::<i64>().ok()),
                "replyCount": node.pointer("/toolbar/replyCount").and_then(|v| v.as_str()).and_then(|s| s.parse::<i64>().ok()),
            }))
        })
        .collect();
    let next_token = read_continuation(&raw);
    let data = json!({
        "videoId": id,
        "comments": comments,
        "pageInfo": { "returned": comments.len(), "nextPageToken": next_token },
    });
    cache.insert(cache_key, Arc::new(data.clone())).await;
    Ok((data, false))
}

fn find_comments_continuation(root: &Value) -> Option<String> {
    // The comments continuation token is the one under an itemSectionRenderer
    // with sectionIdentifier "comment-item-section".
    let mut found = None;
    walk_for_comment_token(root, &mut found, 0);
    found
}

fn walk_for_comment_token(node: &Value, token: &mut Option<String>, depth: usize) {
    if token.is_some() || depth > 40 {
        return;
    }
    match node {
        Value::Object(map) => {
            if map.get("sectionIdentifier").and_then(|v| v.as_str()) == Some("comment-item-section") {
                if let Some(t) = read_continuation(node) {
                    *token = Some(t);
                    return;
                }
            }
            for v in map.values() {
                walk_for_comment_token(v, token, depth + 1);
                if token.is_some() {
                    return;
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                walk_for_comment_token(item, token, depth + 1);
                if token.is_some() {
                    return;
                }
            }
        }
        _ => {}
    }
}

/// `GET /youtube/videos/{id}/related`
#[utoipa::path(get, path = "/youtube/videos/{id}/related", context_path = "/youtube", params(PaginationQuery),
    responses((status = 200, description = "Related videos")))]
async fn related(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<PaginationQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let id = path.into_inner();
    validate_video_id(&id)?;
    let hl = query.hl.as_deref().unwrap_or("en");
    let gl = query.gl.as_deref().unwrap_or("US");
    let limit = page_size(query.max_results, query.limit, 20, 50);

    let cache = state.json_cache(cache_names::YOUTUBE);
    let cache_key = format!("related:{id}:{limit}:{hl}:{gl}");
    if let Some(v) = cache.get(&cache_key).await {
        let meta = meta_json(true, started.elapsed().as_millis() as u64, json!({}));
        return Ok(Json(envelope(v.as_ref().clone(), meta)));
    }

    let raw = innertube::post(&state, EP_NEXT, json!({ "videoId": id }), hl, gl, None).await?;
    let mut videos: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (_, node) in collect_by_key(&raw, crate::parse::VIDEO_RENDERER_KEYS, 80) {
        if let Some(v) = parse_video_renderer(&node) {
            let vid = v.get("videoId").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if vid != id && seen.insert(vid) {
                videos.push(json!({ "type": "video", "id": v.get("videoId"), "video": v }));
            }
        }
        if videos.len() >= limit {
            break;
        }
    }
    let data = json!({ "videoId": id, "videos": videos, "count": videos.len() });
    cache.insert(cache_key, Arc::new(data.clone())).await;
    let meta = meta_json(false, started.elapsed().as_millis() as u64, json!({}));
    Ok(Json(envelope(data, meta)))
}

// ---------------------------------------------------------------- channels

async fn resolve_channel_browse_id(state: &AppState, id: &str, hl: &str, gl: &str) -> Result<String, ApiError> {
    if id.starts_with("UC") {
        return Ok(id.to_string());
    }
    // @handle (or custom path): resolve via resolve_url.
    let handle = if id.starts_with('@') { format!("https://www.youtube.com/{id}") } else { format!("https://www.youtube.com/@{id}") };
    let raw = innertube::post(state, EP_RESOLVE, json!({ "url": handle }), hl, gl, None).await?;
    raw.pointer("/endpoint/browseEndpoint/browseId")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| {
            ApiError::not_found_details(
                format!("{CODE_PREFIX}_NOT_FOUND"),
                format!("Could not resolve channel \"{id}\""),
                json!({ "channel": id }),
            )
        })
}

fn extract_channel_details(raw: &Value) -> Value {
    let header = raw
        .pointer("/header/pageHeaderRenderer")
        .or_else(|| raw.pointer("/header/c4TabbedHeaderRenderer"))
        .cloned()
        .unwrap_or(json!({}));
    let meta = raw.pointer("/metadata/channelMetadataRenderer").cloned().unwrap_or(json!({}));
    json!({
        "channelId": meta.get("externalId").cloned().unwrap_or(Value::Null),
        "title": meta.get("title").cloned().unwrap_or_else(|| read_text(header.get("title")).map(Value::String).unwrap_or(Value::Null)),
        "description": meta.get("description").cloned().unwrap_or(Value::Null),
        "handle": meta.pointer("/vanityChannelUrl").and_then(|v| v.as_str()).map(|u| u.rsplit('/').next().unwrap_or("").to_string()),
        "vanityUrl": meta.pointer("/vanityChannelUrl").cloned().unwrap_or(Value::Null),
        "url": meta.pointer("/channelUrl").cloned().unwrap_or(Value::Null),
        "rssUrl": meta.pointer("/rssUrl").cloned().unwrap_or(Value::Null),
        "keywords": meta.get("keywords").cloned().unwrap_or(Value::Null),
        "country": meta.get("country").cloned().unwrap_or(Value::Null),
        "subscriberCountText": read_text(header.pointer("/content/pageHeaderViewModel/metadata/contentMetadataViewModel/metadataRows/0/metadataParts/0/text/content")),
        "videosCountText": read_text(header.pointer("/content/pageHeaderViewModel/metadata/contentMetadataViewModel/metadataRows/0/metadataParts/1/text/content")),
        "avatar": innertube_fallback_thumb(&header, "/image/decoratedAvatarViewModel/avatar/avatarViewModel/image"),
        "banner": innertube_fallback_thumb(&raw, "/header/pageHeaderRenderer/banner/imageBannerViewModel/image"),
    })
}

fn innertube_fallback_thumb(root: &Value, pointer: &str) -> Value {
    let best = |node: &Value| -> Value {
        collect_by_key(node, &["imageViewModel"], 1)
            .first()
            .and_then(|(_, img)| img.pointer("/image/sources/0"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let _ = pointer;
    root.pointer(pointer).map(|n| best(n)).unwrap_or(Value::Null)
}

/// `GET /youtube/channels/{id}`
#[utoipa::path(get, path = "/youtube/channels/{id}", context_path = "/youtube", params(VideoQuery),
    responses((status = 200, description = "Channel details"), (status = 404, description = "Unknown channel")))]
async fn channel_detail(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<VideoQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let raw_id = path.into_inner();
    let hl = query.hl.as_deref().unwrap_or("en");
    let gl = query.gl.as_deref().unwrap_or("US");
    let browse_id = resolve_channel_browse_id(&state, &raw_id, hl, gl).await?;
    let cache = state.json_cache(cache_names::YOUTUBE);
    let key = format!("channel:{browse_id}:{hl}:{gl}");
    let (data, cached): (Value, bool) = if let Some(v) = cache.get(&key).await {
        (v.as_ref().clone(), true)
    } else {
        let raw = innertube::post(&state, EP_BROWSE, json!({ "browseId": browse_id }), hl, gl, None).await?;
        let details = extract_channel_details(&raw);
        cache.insert(key, Arc::new(details.clone())).await;
        (details, false)
    };
    let meta = meta_json(cached, started.elapsed().as_millis() as u64, json!({ "requested": raw_id }));
    Ok(Json(envelope(data, meta)))
}

/// Channel tab pages share one walker.
async fn channel_tab(
    state: &AppState,
    raw_id: &str,
    tab: &str,
    continuation: Option<String>,
    limit: usize,
    hl: &str,
    gl: &str,
) -> Result<(String, Vec<Value>, Option<String>), ApiError> {
    let tab_key = tab.to_lowercase();
    let body = match &continuation {
        Some(token) => json!({ "continuation": token }),
        None => {
            let browse_id = resolve_channel_browse_id(state, raw_id, hl, gl).await?;
            let mut body = json!({ "browseId": browse_id });
            if tab_key != "home" {
                if let Some(params) = tab_params(tab_key.as_str()) {
                    body["params"] = json!(params);
                }
            }
            body
        }
    };
    let raw = innertube::post(state, EP_BROWSE, body, hl, gl, None).await?;
    let mut videos: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let keys: Vec<&str> = if tab_key == "shorts" {
        vec!["reelItemRenderer", "shortsLockupViewModel"]
    } else if tab_key == "playlists" || tab_key == "posts" {
        vec!["gridPlaylistRenderer", "lockupViewModel", "postRenderer", "backstagePostRenderer"]
    } else if tab_key == "live" || tab_key == "streams" {
        crate::parse::VIDEO_RENDERER_KEYS.to_vec()
    } else {
        crate::parse::VIDEO_RENDERER_KEYS.to_vec()
    };
    for (_, node) in collect_by_key(&raw, &keys, limit * 3) {
        if let Some(v) = parse_video_renderer(&node) {
            let vid = v.get("videoId").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if seen.insert(vid) {
                videos.push(json!({ "type": "video", "id": v.get("videoId"), "video": v }));
            }
        } else if tab_key == "playlists" {
            if let Some((t, id, item)) = crate::parse::parse_lockup(&node) {
                if seen.insert(id.clone()) {
                    videos.push(json!({ "type": t, "id": id, "playlist": item }));
                }
            } else if let Some(pl) = crate::parse::parse_playlist_renderer(&node) {
                let pid = pl.get("playlistId").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if seen.insert(pid.clone()) {
                    videos.push(json!({ "type": "playlist", "id": pid, "playlist": pl }));
                }
            }
        }
        if videos.len() >= limit {
            break;
        }
    }
    let next = read_continuation(&raw);
    Ok((tab_key, videos, next))
}

fn tab_params(tab: &str) -> Option<&'static str> {
    // Known tab params (YouTube rotates them; the home response carries fresh
    // ones which the original also reads — these are the stable defaults).
    match tab {
        "videos" => Some("EgZ2aWRlb3PyBGQMAAQ="),
        "shorts" => Some("EgZzaG9ydHPyBgUKA5oB"),
        "live" | "streams" => Some("EgdzdHJlYW1z8gYECgJ6AA%3D%3D"),
        "playlists" => Some("EglwbGF5bGlzdHPyBgQKAkIA"),
        _ => None,
    }
}

#[utoipa::path(get, path = "/youtube/channels/{id}/videos", context_path = "/youtube", params(ChannelTabQuery),
    responses((status = 200, description = "Channel videos")))]
async fn channel_videos(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<ChannelTabQuery>,
) -> Result<Json<Value>, ApiError> {
    channel_tab_handler(state, path, query, "videos").await
}

#[utoipa::path(get, path = "/youtube/channels/{id}/shorts", context_path = "/youtube", params(ChannelTabQuery),
    responses((status = 200, description = "Channel shorts")))]
async fn channel_shorts(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<ChannelTabQuery>,
) -> Result<Json<Value>, ApiError> {
    channel_tab_handler(state, path, query, "shorts").await
}

#[utoipa::path(get, path = "/youtube/channels/{id}/live", context_path = "/youtube", params(ChannelTabQuery),
    responses((status = 200, description = "Channel live streams")))]
async fn channel_live(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<ChannelTabQuery>,
) -> Result<Json<Value>, ApiError> {
    channel_tab_handler(state, path, query, "live").await
}

#[utoipa::path(get, path = "/youtube/channels/{id}/posts", context_path = "/youtube", params(ChannelTabQuery),
    responses((status = 200, description = "Channel posts")))]
async fn channel_posts(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<ChannelTabQuery>,
) -> Result<Json<Value>, ApiError> {
    channel_tab_handler(state, path, query, "posts").await
}

#[utoipa::path(get, path = "/youtube/channels/{id}/playlists", context_path = "/youtube", params(ChannelTabQuery),
    responses((status = 200, description = "Channel playlists")))]
async fn channel_playlists(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<ChannelTabQuery>,
) -> Result<Json<Value>, ApiError> {
    channel_tab_handler(state, path, query, "playlists").await
}

/// `GET /youtube/channels/{id}/tabs?tab=`
#[utoipa::path(get, path = "/youtube/channels/{id}/tabs", context_path = "/youtube", params(ChannelTabQuery),
    responses((status = 200, description = "Arbitrary channel tab")))]
async fn channel_tabs(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<ChannelTabQuery>,
) -> Result<Json<Value>, ApiError> {
    let tab = query.tab.as_deref().unwrap_or("videos").to_string();
    channel_tab_handler(state, path, query, &tab).await
}

async fn channel_tab_handler(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<ChannelTabQuery>,
    tab: &str,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let raw_id = path.into_inner();
    let hl = query.hl.as_deref().unwrap_or("en");
    let gl = query.gl.as_deref().unwrap_or("US");
    let limit = page_size(query.max_results, query.limit, 30, 60);
    let continuation = page(query.page_token.clone(), None);
    let (tab, items, next) = channel_tab(&state, &raw_id, tab, continuation, limit, hl, gl).await?;
    let meta = meta_json(false, started.elapsed().as_millis() as u64, json!({ "tab": tab, "requested": raw_id }));
    Ok(Json(envelope(json!({ "tab": tab, "items": items, "count": items.len(), "pageInfo": { "nextPageToken": next } }), meta)))
}

/// `GET /youtube/channels` — batch lookup.
#[utoipa::path(get, path = "/youtube/channels", context_path = "/youtube", params(VideoListQuery),
    responses((status = 200, description = "Batch channel lookup")))]
async fn channels_batch(
    state: web::Data<AppState>,
    query: web::Query<VideoListQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let ids = query.id.clone().map(|c| c.0).ok_or_else(|| {
        ApiError::validation_details(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "`id` is required, e.g. /youtube/channels?id=UC...&id=UC...".to_string(),
            json!({ "parameter": "id" }),
        )
    })?;
    if ids.len() > 50 {
        return Err(ApiError::validation(
            format!("{CODE_PREFIX}_INVALID_REQUEST"),
            "at most 50 ids per request",
        ));
    }
    let hl = query.hl.as_deref().unwrap_or("en");
    let gl = query.gl.as_deref().unwrap_or("US");
    let mut channels = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    for id in &ids {
        match resolve_channel_browse_id(&state, id, hl, gl).await {
            Ok(browse_id) => {
                let raw = innertube::post(&state, EP_BROWSE, json!({ "browseId": browse_id }), hl, gl, None).await?;
                channels.push(extract_channel_details(&raw));
            }
            Err(e) => errors.push(json!({ "channelId": id, "error": e.to_string() })),
        }
    }
    let meta = meta_json(false, started.elapsed().as_millis() as u64, json!({}));
    Ok(Json(envelope(json!({ "channels": channels, "errors": errors, "count": channels.len() }), meta)))
}

// --------------------------------------------------------------- playlists

/// `GET /youtube/playlists/{id}`
#[utoipa::path(get, path = "/youtube/playlists/{id}", context_path = "/youtube", params(LocaleQuery),
    responses((status = 200, description = "Playlist details"), (status = 404, description = "Unknown playlist")))]
async fn playlist_detail(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<LocaleQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let id = path.into_inner();
    let data = playlist_handler(&state, &id, query.hl.as_deref().unwrap_or("en"), query.gl.as_deref().unwrap_or("US")).await?;
    let meta = meta_json(false, started.elapsed().as_millis() as u64, json!({}));
    Ok(Json(envelope(data, meta)))
}

async fn playlist_handler(state: &AppState, id: &str, hl: &str, gl: &str) -> Result<Value, ApiError> {
    let raw = innertube::post(state, EP_BROWSE, json!({ "browseId": format!("VL{id}") }), hl, gl, None).await?;
    let header = raw.pointer("/header/playlistHeaderRenderer").cloned().unwrap_or(json!({}));
    let meta = raw.pointer("/metadata/playlistMetadataRenderer").cloned().unwrap_or(json!({}));
    if meta.get("title").is_none() && header.is_null() {
        return Err(ApiError::not_found_details(
            format!("{CODE_PREFIX}_NOT_FOUND"),
            format!("Playlist \"{id}\" was not found"),
            json!({ "playlistId": id }),
        ));
    }
    let data = json!({
        "playlistId": id,
        "title": meta.get("title").cloned().unwrap_or_else(|| read_text(header.get("title")).map(Value::String).unwrap_or(Value::Null)),
        "description": meta.get("description").cloned().unwrap_or(Value::Null),
        "ownerText": read_text(header.get("ownerText")),
        "videoCountText": read_text(header.get("numVideosText")),
        "viewCountText": read_text(header.get("viewCountText")),
        "isUnlisted": header.get("isUnlisted").cloned().unwrap_or(Value::Null),
        "thumbnails": collect_by_key(&raw, &["playlistCustomThumbnailRenderer"], 1)
            .first()
            .and_then(|(_, n)| crate::parse::best_thumbnail(n.get("thumbnail")))
            .map(|b| vec![b])
            .unwrap_or_default(),
        "url": format!("https://www.youtube.com/playlist?list={id}"),
    });
    Ok(data)
}

/// `GET /youtube/playlists/{id}/videos`
#[utoipa::path(get, path = "/youtube/playlists/{id}/videos", context_path = "/youtube", params(PaginationQuery),
    responses((status = 200, description = "Playlist videos")))]
async fn playlist_videos(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<PaginationQuery>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let id = path.into_inner();
    let hl = query.hl.as_deref().unwrap_or("en");
    let gl = query.gl.as_deref().unwrap_or("US");
    let limit = page_size(query.max_results, query.limit, 50, 100);
    let continuation = page(query.page_token.clone(), query.cursor.clone());

    let cache = state.json_cache(cache_names::YOUTUBE);
    let cache_key = format!("playlist_videos:{id}:{:?}:{limit}:{hl}:{gl}", continuation);
    if let Some(v) = cache.get(&cache_key).await {
        let meta = meta_json(true, started.elapsed().as_millis() as u64, json!({}));
        return Ok(Json(envelope(v.as_ref().clone(), meta)));
    }

    let body = match &continuation {
        Some(token) => json!({ "continuation": token }),
        None => json!({ "browseId": format!("VL{id}") }),
    };
    let raw = innertube::post(&state, EP_BROWSE, body, hl, gl, None).await?;
    let mut videos: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (_, node) in collect_by_key(&raw, crate::parse::VIDEO_RENDERER_KEYS, limit * 2) {
        if let Some(v) = parse_video_renderer(&node) {
            let vid = v.get("videoId").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if seen.insert(vid) {
                videos.push(json!({ "type": "video", "id": v.get("videoId"), "video": v }));
            }
        }
        if videos.len() >= limit {
            break;
        }
    }
    let next = read_continuation(&raw);
    let data = json!({
        "playlistId": id,
        "videos": videos,
        "count": videos.len(),
        "pageInfo": { "nextPageToken": next },
    });
    cache.insert(cache_key, Arc::new(data.clone())).await;
    let meta = meta_json(false, started.elapsed().as_millis() as u64, json!({}));
    Ok(Json(envelope(data, meta)))
}

// ----------------------------------------------------------- compat aliases

fn require_query_id(value: Option<&String>, endpoint: &str) -> Result<String, ApiError> {
    value
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| {
            ApiError::validation_details(
                format!("{CODE_PREFIX}_INVALID_REQUEST"),
                format!("`id` is required, e.g. /youtube/{endpoint}?id=..."),
                json!({ "parameter": "id" }),
            )
        })
}

#[utoipa::path(get, path = "/youtube/video", context_path = "/youtube", params(VideoQuery),
    responses((status = 200, description = "Video details (compat alias)")))]
async fn compat_video(
    state: web::Data<AppState>,
    query: web::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let id = require_query_id(query.get("id"), "video")?;
    let raw = player_response(&state, &id, query.get("hl").map(|s| s.as_str()).unwrap_or("en"), query.get("gl").map(|s| s.as_str()).unwrap_or("US")).await?;
    let data = json!({ "video": extract_player_details(&raw), "formats": extract_formats(&raw) });
    let meta = meta_json(false, started.elapsed().as_millis() as u64, json!({}));
    Ok(Json(envelope(data, meta)))
}

#[utoipa::path(get, path = "/youtube/channel", context_path = "/youtube",
    responses((status = 200, description = "Channel details (compat alias)")))]
async fn compat_channel(
    state: web::Data<AppState>,
    query: web::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let started = std::time::Instant::now();
    let raw_id = require_query_id(query.get("id"), "channel")?;
    let hl = query.get("hl").map(|s| s.as_str()).unwrap_or("en");
    let gl = query.get("gl").map(|s| s.as_str()).unwrap_or("US");
    let browse_id = resolve_channel_browse_id(&state, &raw_id, hl, gl).await?;
    let cache = state.json_cache(cache_names::YOUTUBE);
    let key = format!("channel:{browse_id}:{hl}:{gl}");
    let (data, cached): (Value, bool) = if let Some(v) = cache.get(&key).await {
        (v.as_ref().clone(), true)
    } else {
        let raw = innertube::post(&state, EP_BROWSE, json!({ "browseId": browse_id }), hl, gl, None).await?;
        let details = extract_channel_details(&raw);
        cache.insert(key, Arc::new(details.clone())).await;
        (details, false)
    };
    let meta = meta_json(cached, started.elapsed().as_millis() as u64, json!({ "requested": raw_id }));
    Ok(Json(envelope(data, meta)))
}

#[utoipa::path(get, path = "/youtube/channel/videos", context_path = "/youtube",
    responses((status = 200, description = "Channel videos (compat alias)")))]
async fn compat_channel_videos(
    state: web::Data<AppState>,
    query: web::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let raw_id = require_query_id(query.get("id"), "channel/videos")?;
    let hl = query.get("hl").map(|s| s.as_str()).unwrap_or("en");
    let gl = query.get("gl").map(|s| s.as_str()).unwrap_or("US");
    let limit = query.get("maxResults").or_else(|| query.get("limit")).and_then(|v| v.parse::<usize>().ok()).unwrap_or(30).clamp(1, 60);
    let (tab, items, next) = channel_tab(&state, &raw_id, "videos", None, limit, hl, gl).await?;
    let started = std::time::Instant::now();
    let meta = meta_json(false, started.elapsed().as_millis() as u64, json!({ "tab": tab, "requested": raw_id }));
    Ok(Json(envelope(json!({ "tab": tab, "items": items, "count": items.len(), "pageInfo": { "nextPageToken": next } }), meta)))
}

#[utoipa::path(get, path = "/youtube/comments", context_path = "/youtube",
    responses((status = 200, description = "Comments (compat alias)")))]
async fn compat_comments(
    state: web::Data<AppState>,
    query: web::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let id = require_query_id(query.get("id"), "comments")?;
    let started = std::time::Instant::now();
    let (data, _) = comments_handler(&state, &id, None, 20, "en", "US").await?;
    let meta = meta_json(false, started.elapsed().as_millis() as u64, json!({}));
    Ok(Json(envelope(data, meta)))
}

#[utoipa::path(get, path = "/youtube/playlist", context_path = "/youtube",
    responses((status = 200, description = "Playlist details (compat alias)")))]
async fn compat_playlist(
    state: web::Data<AppState>,
    query: web::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let id = require_query_id(query.get("id"), "playlist")?;
    let started = std::time::Instant::now();
    let data = playlist_handler(&state, &id, "en", "US").await?;
    let meta = meta_json(false, started.elapsed().as_millis() as u64, json!({}));
    Ok(Json(envelope(data, meta)))
}

// ------------------------------------------------------------------ health

async fn health_report(state: &AppState) -> Value {
    let probe_key = "youtube_probe";
    let probe_cache = state.json_cache(cache_names::HEALTH_PROBES);
    let cached = probe_cache.get(probe_key).await;
    let (probe, was_cached) = match cached {
        Some(v) => (v.as_ref().clone(), true),
        None => {
            let report: Value = match innertube::post(state, EP_GUIDE, json!({}), "en", "US", None).await {
                Ok(raw) => {
                    let items = raw
                        .pointer("/items")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    if items > 0 {
                        json!({ "reachable": true, "guideItems": items })
                    } else {
                        json!({ "reachable": false, "error": "Guide payload had no items" })
                    }
                }
                Err(e) => json!({ "reachable": false, "error": e.to_string() }),
            };
            probe_cache.insert(probe_key.to_string(), Arc::new(report.clone())).await;
            (report, false)
        }
    };
    let reachable = probe.get("reachable").and_then(|v| v.as_bool()).unwrap_or(false);
    json!({
        "status": if reachable { "ok" } else { "degraded" },
        "uptimeSeconds": state.uptime_seconds(),
        "upstream": { "name": UPSTREAM_NAME, "endpoint": UPSTREAM_INNERTUBE, "probe": probe, "cached": was_cached },
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
}

/// `GET /youtube/health`
#[utoipa::path(get, path = "/youtube/health", context_path = "/youtube",
    responses((status = 200, description = "Liveness plus InnerTube probe")))]
async fn health(state: web::Data<AppState>) -> Json<Value> {
    Json(health_report(&state).await)
}

/// `GET /youtube/health/ready`
#[utoipa::path(get, path = "/youtube/health/ready", context_path = "/youtube",
    responses((status = 200, description = "InnerTube reachable"), (status = 503, description = "Upstream down")))]
async fn health_ready(state: web::Data<AppState>) -> Result<Json<Value>, ApiError> {
    let report = health_report(&state).await;
    if report.get("upstream").and_then(|u| u.get("probe")).and_then(|p| p.get("reachable")).and_then(|v| v.as_bool()) != Some(true) {
        return Err(ApiError::unavailable(
            format!("{CODE_PREFIX}_UPSTREAM_UNAVAILABLE"),
            "The InnerTube upstream is unreachable".to_string(),
        ));
    }
    Ok(Json(json!({
        "status": "ready",
        "upstream": UPSTREAM_NAME,
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })))
}

// ---------------------------------------------------------------- OpenAPI

#[derive(OpenApi)]
#[openapi(
    info(title = "youtube-api", version = "1.0.0", description = "YouTube InnerTube API: search, trending, videos, channels, playlists, transcripts and comments — no API key."),
    paths(search, search_post, trending, resolve, video_detail, video_formats, videos_batch, transcript, comments, related, channel_detail, channel_videos, channel_shorts, channel_live, channel_posts, channel_playlists, channel_tabs, channels_batch, playlist_detail, playlist_videos, compat_video, compat_channel, compat_channel_videos, compat_comments, compat_playlist, health, health_ready)
)]
pub struct YoutubeApi;

pub fn openapi_doc() -> utoipa::openapi::OpenApi {
    let mut doc = YoutubeApi::openapi();
    doc.info.title = "youtube-api".into();
    doc.servers = Some(vec![utoipa::openapi::Server::new(SERVICE_PREFIX)]);
    doc
}

#[cfg(test)]
mod transcript_tests {
    use super::*;

    #[test]
    fn srv1_xml_parses_with_entities() {
        let xml = r#"<transcript><text start="1.5" dur="2.25">hello &amp; &#39;world&#39;</text><text start="4" dur="1"></text><text start="5.25" dur="1.5">second line</text></transcript>"#;
        let lines = xml_caption_lines(xml);
        assert_eq!(lines.len(), 2, "empty texts are dropped: {lines:?}");
        assert_eq!(lines[0]["startMs"], 1500);
        assert_eq!(lines[0]["durationMs"], 2250);
        assert_eq!(lines[0]["text"], "hello & 'world'");
        assert_eq!(lines[1]["startMs"], 5250);
    }

    #[test]
    fn srv3_xml_parses_segments() {
        let xml = r#"<timedtext format="3"><body><p t="0" d="4000"><s>never</s><s> gonna</s></p><p t="4000" d="2000">give you up</p></body></timedtext>"#;
        let lines = xml_caption_lines(xml);
        assert_eq!(lines.len(), 2, "srv3 segments join: {lines:?}");
        assert_eq!(lines[0]["text"], "never gonna");
        assert_eq!(lines[0]["startMs"], 0);
        assert_eq!(lines[1]["text"], "give you up");
        assert_eq!(lines[1]["startMs"], 4000);
    }

    #[test]
    fn json3_lines_extract_events() {
        let parsed: Value = serde_json::from_str(
            r#"{"events":[{"tStartMs":100,"dDurationMs":900,"segs":[{"utf8":"hi "},{"utf8":"there"}]},{"tStartMs":2000,"segs":[{"utf8":"  "}]}]}"#,
        )
        .unwrap();
        let lines = json3_lines(&parsed).unwrap();
        assert_eq!(lines.len(), 1, "whitespace-only events are dropped: {lines:?}");
        assert_eq!(lines[0]["text"], "hi there");
        assert_eq!(lines[0]["durationMs"], 900);
    }

    #[test]
    fn entity_unescape_handles_hex_decimal_and_raw() {
        assert_eq!(xml_unescape("a&amp;b&#x27;c&#39;d&unknown;e"), "a&b'c'd&unknown;e");
        assert_eq!(xml_unescape("&quot;q&quot; &gt; &lt;"), "\"q\" > <");
    }
}
