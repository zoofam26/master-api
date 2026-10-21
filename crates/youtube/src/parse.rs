//! InnerTube payload parsing: renderer extraction into this service's flat
//! item shapes. Ported from `youtube-api/src/youtube/parsers/*`.

use serde_json::{json, Value};

// ------------------------------------------------------------- primitives

pub fn read_text(v: Option<&Value>) -> Option<String> {
    let Some(v) = v else { return None };
    if let Some(s) = v.as_str() {
        return Some(s.to_string()).filter(|s| !s.is_empty());
    }
    if let Some(simple) = v.get("simpleText").and_then(|x| x.as_str()) {
        return Some(simple.to_string()).filter(|s| !s.is_empty());
    }
    if let Some(runs) = v.get("runs").and_then(|x| x.as_array()) {
        let joined: String = runs.iter().filter_map(|r| r.get("text").and_then(|t| t.as_str())).collect();
        return Some(joined).filter(|s| !s.is_empty());
    }
    None
}

/// `{"simpleText": "1,234,567 views"}` / "1.2M views" → 1234567 (approx for K/M/B).
pub fn parse_count(input: Option<&Value>) -> Option<i64> {
    let text = read_text(input)?;
    let cleaned = text.replace(',', "").replace('\u{a0}', " ");
    let parts: Vec<&str> = cleaned.split_whitespace().collect();
    let num_part = parts.first()?;
    // Suffix may be glued to the number ("1.2M") or separate ("1.2 M").
    let (digits, glued_multiplier) = {
        let t = num_part.to_lowercase();
        if let Some(stripped) = t.strip_suffix('k') {
            (stripped.to_string(), 1_000i64)
        } else if let Some(stripped) = t.strip_suffix('m') {
            (stripped.to_string(), 1_000_000)
        } else if let Some(stripped) = t.strip_suffix('b') {
            (stripped.to_string(), 1_000_000_000)
        } else {
            (t.to_string(), 1)
        }
    };
    let separate_multiplier = match parts.get(1).map(|s| s.to_lowercase()).as_deref() {
        Some(s) if s.starts_with('k') => 1_000i64,
        Some(s) if s.starts_with('m') => 1_000_000,
        Some(s) if s.starts_with('b') => 1_000_000_000,
        _ => 1,
    };
    let multiplier = if glued_multiplier != 1 { glued_multiplier } else { separate_multiplier };
    if let Ok(n) = digits.parse::<i64>() {
        return Some(n * multiplier);
    }
    let float: f64 = digits.parse().ok()?;
    Some((float * multiplier as f64) as i64)
}

/// `PT4M13S` / `4:13` → seconds.
pub fn parse_duration_to_seconds(input: Option<&str>) -> Option<i64> {
    let text = input?;
    if let Some(rest) = text.strip_prefix("PT") {
        let mut total = 0i64;
        let mut num = String::new();
        for c in rest.chars() {
            if c.is_ascii_digit() {
                num.push(c);
            } else {
                let n: i64 = num.parse().unwrap_or(0);
                num.clear();
                match c {
                    'H' => total += n * 3600,
                    'M' => total += n * 60,
                    'S' => total += n,
                    _ => {}
                }
            }
        }
        return Some(total);
    }
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() >= 2 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) {
        let mut total = 0i64;
        for p in parts {
            total = total * 60 + p.parse::<i64>().unwrap_or(0);
        }
        return Some(total);
    }
    None
}

pub fn format_duration(total_seconds: Option<i64>) -> Option<String> {
    let s = total_seconds?;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    Some(if h > 0 { format!("{h}:{m:02}:{sec:02}") } else { format!("{m}:{sec:02}") })
}

pub fn read_thumbnails(v: Option<&Value>) -> Vec<Value> {
    v.and_then(|x| x.get("thumbnails"))
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    let url = t.get("url").and_then(|u| u.as_str())?;
                    Some(json!({
                        "url": if url.starts_with("//") { format!("https:{url}") } else { url.to_string() },
                        "width": t.get("width").cloned().unwrap_or(Value::Null),
                        "height": t.get("height").cloned().unwrap_or(Value::Null),
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn best_thumbnail(v: Option<&Value>) -> Option<Value> {
    let thumbs = read_thumbnails(v);
    thumbs
        .iter()
        .max_by_key(|t| t.get("width").and_then(|w| w.as_i64()).unwrap_or(0))
        .cloned()
}

/// Navigation endpoint extraction (browseId + canonical base url).
fn read_navigation(v: Option<&Value>) -> (Option<String>, Option<String>) {
    let endpoint = v
        .and_then(|x| x.get("runs"))
        .and_then(|x| x.as_array())
        .and_then(|a| a.first())
        .and_then(|r| r.get("navigationEndpoint"));
    let browse_id = endpoint
        .and_then(|e| e.get("browseEndpoint"))
        .and_then(|b| b.get("browseId"))
        .and_then(|x| x.as_str())
        .map(String::from);
    let canonical = endpoint
        .and_then(|e| e.get("commandMetadata"))
        .and_then(|e| e.get("webCommandMetadata"))
        .and_then(|b| b.get("url"))
        .and_then(|x| x.as_str())
        .map(String::from);
    (browse_id, canonical)
}

/// "3 days ago" → best-effort ISO instant (approximate by design; the raw
/// relative text is always preserved alongside).
pub fn relative_text_to_iso(text: Option<&Value>) -> Option<String> {
    let t = read_text(text)?.to_lowercase();
    let now = chrono::Utc::now();
    let (n, unit) = {
        let parts: Vec<&str> = t.split_whitespace().collect();
        if parts.len() < 3 || !parts[2].starts_with("ago") {
            return None;
        }
        (parts[0].parse::<i64>().ok()?, parts[1].to_string())
    };
    let delta = match unit.as_str() {
        s if s.starts_with("second") => chrono::Duration::seconds(n),
        s if s.starts_with("minute") => chrono::Duration::minutes(n),
        s if s.starts_with("hour") => chrono::Duration::hours(n),
        s if s.starts_with("day") => chrono::Duration::days(n),
        s if s.starts_with("week") => chrono::Duration::weeks(n),
        s if s.starts_with("month") => chrono::Duration::days(30 * n),
        s if s.starts_with("year") => chrono::Duration::days(365 * n),
        _ => return None,
    };
    Some((now - delta).to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

// ------------------------------------------------------------- renderers

pub const VIDEO_RENDERER_KEYS: &[&str] = &[
    "videoRenderer",
    "compactVideoRenderer",
    "gridVideoRenderer",
    "reelItemRenderer",
    "playlistPanelVideoRenderer",
];

#[allow(dead_code)]
pub fn is_video_renderer_key(key: &str) -> bool {
    VIDEO_RENDERER_KEYS.contains(&key)
}

fn collect_badges(node: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["badges", "ownerBadges"] {
        if let Some(arr) = node.get(key).and_then(|v| v.as_array()) {
            for b in arr {
                if let Some(style) = b.pointer("/metadataBadgeRenderer/style").and_then(|s| s.as_str()) {
                    out.push(style.to_string());
                }
            }
        }
    }
    out
}

#[allow(dead_code)]
fn extract_from_metadata(node: &Value, ends_with: &str) -> Option<String> {
    let rows = node.pointer("/ownerText")?;
    let _ = ends_with;
    let _ = rows;
    None
}

/// Parses any of YouTube's video renderer shapes into one flat structure.
pub fn parse_video_renderer(node: &Value) -> Option<Value> {
    let video_id = node.get("videoId").and_then(|v| v.as_str())?.to_string();
    let owner_text = read_text(node.get("ownerText")).or_else(|| read_text(node.get("shortBylineText")));
    let (owner_browse_id, owner_canonical) = read_navigation(node.get("ownerText").or_else(|| node.get("shortBylineText")));
    let length_text = read_text(node.get("lengthText"));
    let duration_seconds = parse_duration_to_seconds(length_text.as_deref());
    let badges = collect_badges(node);
    let view_count_text = read_text(node.get("viewCountText")).or_else(|| read_text(node.get("shortViewCountText")));
    let is_live = node.get("liveNow").and_then(|v| v.as_bool()).unwrap_or(false)
        || badges.iter().any(|b| b == "LIVE")
        || node.pointer("/thumbnailOverlays").map(|_| false).unwrap_or(false) && badges.iter().any(|b| b.contains("LIVE"));

    let channel_url = owner_canonical
        .map(|c| format!("https://www.youtube.com{c}"))
        .or_else(|| owner_browse_id.as_ref().map(|b| format!("https://www.youtube.com/channel/{b}")));

    let mut out = json!({
        "videoId": video_id,
        "title": read_text(node.get("title")).or_else(|| read_text(node.get("headline"))),
        "channel": {
            "channelId": owner_browse_id,
            "title": owner_text,
            "url": channel_url,
            "verified": badges.iter().any(|b| b.to_lowercase().contains("verified")),
            "thumbnail": best_thumbnail(node.pointer("/channelThumbnailSupportedRenderers/channelThumbnailWithLinkRenderer/thumbnail")),
        },
        "publishedAtText": read_text(node.get("publishedTimeText")),
        "duration": length_text.clone().or_else(|| format_duration(duration_seconds)),
        "durationSeconds": duration_seconds,
        "viewCount": parse_count(node.get("viewCountText")).or_else(|| parse_count(node.get("shortViewCountText"))),
        "viewCountText": view_count_text,
        "isLive": is_live,
        "isShort": false,
        "badges": badges,
        "thumbnails": collect_video_thumbnails(node),
        "url": format!("https://www.youtube.com/watch?v={video_id}"),
    });
    if let Some(published) = relative_text_to_iso(node.get("publishedTimeText")) {
        out["publishedAt"] = json!(published);
    }
    if let Some(desc) = read_text(node.pointer("/detailedMetadataSnippets/0/snippetText")) {
        out["description"] = json!(desc);
    }
    if let Some(vc) = parse_count(node.get("viewCountText")).or_else(|| parse_count(node.get("shortViewCountText"))) {
        out["viewCount"] = json!(vc);
    }
    out["isLive"] = json!(node.get("liveNow").and_then(|v| v.as_bool()).unwrap_or(false)
        || out["isLive"].as_bool().unwrap_or(false));
    Some(out)
}

fn collect_video_thumbnails(node: &Value) -> Vec<Value> {
    let t = read_thumbnails(node.get("thumbnail"));
    if !t.is_empty() {
        return t;
    }
    best_thumbnail(node.pointer("/thumbnail/thumbnails")).map(|b| vec![b]).unwrap_or_default()
}

/// `lockupViewModel` — YouTube's newer universal card.
pub fn parse_lockup(node: &Value) -> Option<(String, String, Value)> {
    let content_id = node.get("contentId").and_then(|v| v.as_str())?;
    let content_type = node.get("contentType").and_then(|v| v.as_str())?;
    let meta = node.pointer("/metadata/lockupMetadataViewModel/title");
    let title = read_text(meta);
    let image = best_thumbnail(node.pointer("/contentImage/thumbnailViewModel/image"));
    match content_type {
        t if t.starts_with("LOCKUP_CONTENT_TYPE_PLAYLIST") => Some((
            "playlist".into(),
            content_id.to_string(),
            json!({
                "playlistId": content_id,
                "title": title,
                "thumbnails": image.map(|b| vec![b]).unwrap_or_default(),
            }),
        )),
        t if t.starts_with("LOCKUP_CONTENT_TYPE_VIDEO") => Some((
            "video".into(),
            content_id.to_string(),
            json!({
                "videoId": content_id,
                "title": title,
                "thumbnails": image.map(|b| vec![b]).unwrap_or_default(),
                "url": format!("https://www.youtube.com/watch?v={content_id}"),
            }),
        )),
        _ => None,
    }
}

#[allow(dead_code)]
pub fn is_channel_renderer_key(key: &str) -> bool {
    key == "channelRenderer"
}

pub fn parse_channel_renderer(node: &Value) -> Option<Value> {
    let channel_id = node.get("channelId").and_then(|v| v.as_str())?.to_string();
    let (_, canonical) = read_navigation(node.get("navigationEndpoint"));
    Some(json!({
        "channel": {
            "channelId": channel_id,
            "title": read_text(node.get("title")),
            "handle": read_text(node.get("displayName")),
            "descriptionSnippet": read_text(node.get("descriptionSnippet")),
            "subscriberCountText": read_text(node.get("subscriberCountText")),
            "videoCountText": read_text(node.get("videoCountText")),
            "url": canonical.map(|c| format!("https://www.youtube.com{c}"))
                .unwrap_or_else(|| format!("https://www.youtube.com/channel/{channel_id}")),
            "thumbnail": best_thumbnail(node.get("thumbnail")),
        }
    }))
}

#[allow(dead_code)]
pub fn is_playlist_renderer_key(key: &str) -> bool {
    key == "playlistRenderer"
}

pub fn parse_playlist_renderer(node: &Value) -> Option<Value> {
    let playlist_id = node.get("playlistId").and_then(|v| v.as_str())?.to_string();
    Some(json!({
        "playlistId": playlist_id,
        "title": read_text(node.get("title")),
        "videoCountText": read_text(node.get("videoCountText")),
        "publishedAtText": read_text(node.get("publishedTimeText")),
        "thumbnails": read_thumbnails(node.get("thumbnails")).into_iter().take(4).collect::<Vec<_>>(),
        "url": format!("https://www.youtube.com/playlist?list={playlist_id}"),
    }))
}

// ------------------------------------------------------------- traversal

/// Depth-first walk collecting the first N of a renderer key.
pub fn collect_by_key(root: &Value, keys: &[&str], max: usize) -> Vec<(String, Value)> {
    let mut found = Vec::new();
    walk(root, keys, max, &mut found, 0);
    found
}

fn walk(node: &Value, keys: &[&str], max: usize, found: &mut Vec<(String, Value)>, depth: usize) {
    if found.len() >= max || depth > 40 {
        return;
    }
    match node {
        Value::Object(map) => {
            for (k, v) in map {
                if keys.contains(&k.as_str()) {
                    found.push((k.clone(), v.clone()));
                }
                walk(v, keys, max, found, depth + 1);
                if found.len() >= max {
                    return;
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                walk(item, keys, max, found, depth + 1);
                if found.len() >= max {
                    return;
                }
            }
        }
        _ => {}
    }
}

/// First continuation token found anywhere in the payload.
pub fn read_continuation(root: &Value) -> Option<String> {
    let mut token = None;
    walk_for_continuation(root, &mut token, 0);
    token
}

fn walk_for_continuation(node: &Value, token: &mut Option<String>, depth: usize) {
    if token.is_some() || depth > 40 {
        return;
    }
    match node {
        Value::Object(map) => {
            if let Some(next) = map.get("continuationEndpoint") {
                if let Some(t) = next.pointer("/continuationCommand/token").and_then(|x| x.as_str()) {
                    *token = Some(t.to_string());
                    return;
                }
            }
            if let Some(next) = map.get("nextContinuationData") {
                if let Some(t) = next.get("continuation").and_then(|x| x.as_str()) {
                    *token = Some(t.to_string());
                    return;
                }
            }
            for v in map.values() {
                walk_for_continuation(v, token, depth + 1);
                if token.is_some() {
                    return;
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                walk_for_continuation(item, token, depth + 1);
                if token.is_some() {
                    return;
                }
            }
        }
        _ => {}
    }
}

/// Encodes search filters into the base64 protobuf `params` blob.
/// Wire format: 1: sort (varint), 2: nested {1: uploadDate, 2: type, 3: duration}.
pub fn build_search_params(
    sort: Option<&str>,
    upload_date: Option<&str>,
    type_filter: Option<&str>,
    duration: Option<&str>,
) -> Option<String> {
    let varint = |value: u32| -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut remaining = value;
        loop {
            let mut byte = (remaining & 0x7f) as u8;
            remaining >>= 7;
            if remaining != 0 {
                byte |= 0x80;
            }
            bytes.push(byte);
            if remaining == 0 {
                break;
            }
        }
        bytes
    };
    const SORT: &[(&str, u32)] = &[("relevance", 0), ("rating", 1), ("upload_date", 2), ("view_count", 3)];
    const UPLOAD: &[(&str, u32)] = &[("today", 1), ("this_week", 2), ("this_month", 3), ("this_year", 4)];
    const TYPE: &[(&str, u32)] = &[("all", 0), ("video", 1), ("channel", 2), ("playlist", 3)];
    const DURATION: &[(&str, u32)] = &[("short", 1), ("medium", 2), ("long", 3)];

    let mut parts: Vec<u8> = Vec::new();
    let mut nested: Vec<u8> = Vec::new();
    if let Some(s) = sort {
        if let Some((_, code)) = SORT.iter().find(|(k, _)| *k == s) {
            if *code != 0 {
                parts.push(0x08);
                parts.extend(varint(*code));
            }
        }
    }
    if let Some(u) = upload_date {
        if let Some((_, code)) = UPLOAD.iter().find(|(k, _)| *k == u) {
            nested.push(0x08);
            nested.extend(varint(*code));
        }
    }
    if let Some(t) = type_filter {
        if let Some((_, code)) = TYPE.iter().find(|(k, _)| *k == t) {
            if *code != 0 {
                nested.push(0x10);
                nested.extend(varint(*code));
            }
        }
    }
    if let Some(d) = duration {
        if let Some((_, code)) = DURATION.iter().find(|(k, _)| *k == d) {
            nested.push(0x18);
            nested.extend(varint(*code));
        }
    }
    if !nested.is_empty() {
        parts.push(0x12);
        parts.push(nested.len() as u8);
        parts.extend(nested);
    }
    if parts.is_empty() {
        return None;
    }
    use base64::Engine as _;
    Some(base64::engine::general_purpose::STANDARD.encode(parts))
}

/// Search result extraction: walks the payload, de-duplicating by id.
pub fn parse_search_results(data: &Value, query: &str, type_filter: Option<&str>) -> Value {
    let mut contents: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let renderers = collect_by_key(data, VIDEO_RENDERER_KEYS, 200);
    for (_, node) in renderers {
        if let Some(video) = parse_video_renderer(&node) {
            let id = video.get("videoId").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if seen.insert(id) {
                contents.push(json!({ "type": "video", "id": video.get("videoId"), "video": video }));
            }
        }
    }
    // channels + playlists + lockups (only when not filtering to videos)
    let type_wants = |t: &str| type_filter.map(|f| f == "all" || f == t).unwrap_or(true);
    if !type_filter.map(|f| f == "video").unwrap_or(false) {
        for (key, node) in collect_by_key(data, &["channelRenderer"], 50) {
            let _ = key;
            if let Some(ch) = parse_channel_renderer(&node) {
                let id = ch.pointer("/channel/channelId").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if seen.insert(id) {
                    contents.push(json!({ "type": "channel", "id": ch.pointer("/channel/channelId"), "channel": ch }));
                }
            }
        }
        for (key, node) in collect_by_key(data, &["playlistRenderer"], 50) {
            let _ = key;
            if let Some(pl) = parse_playlist_renderer(&node) {
                let id = pl.get("playlistId").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if seen.insert(id) {
                    contents.push(json!({ "type": "playlist", "id": pl.get("playlistId"), "playlist": pl }));
                }
            }
        }
        // lockupViewModel is not a key but a shape: find objects with contentId+contentType.
        let mut lockups: Vec<Value> = Vec::new();
        find_lockups(data, &mut lockups, 0);
        for node in lockups {
            if let Some((t, id, item)) = parse_lockup(&node) {
                if type_wants(&t) && seen.insert(id.clone()) {
                    match t.as_str() {
                        "video" => contents.push(json!({ "type": "video", "id": id, "video": item })),
                        "playlist" => contents.push(json!({ "type": "playlist", "id": id, "playlist": item })),
                        _ => {}
                    }
                }
            }
        }
    }

    let estimated = read_text(data.get("estimatedResults")).and_then(|s| s.replace(',', "").parse::<i64>().ok());
    json!({
        "query": query,
        "contents": contents,
        "pageInfo": {
            "results": contents.len(),
            "nextPageToken": read_continuation(data),
            "totalResults": estimated,
        },
    })
}

fn find_lockups(node: &Value, out: &mut Vec<Value>, depth: usize) {
    if out.len() >= 100 || depth > 40 {
        return;
    }
    match node {
        Value::Object(map) => {
            if map.contains_key("contentId") && map.contains_key("contentType") {
                out.push(node.clone());
            }
            for v in map.values() {
                find_lockups(v, out, depth + 1);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                find_lockups(item, out, depth + 1);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn counts_and_durations() {
        assert_eq!(parse_count(Some(&json!({"simpleText": "1,234,567 views"}))), Some(1_234_567));
        assert_eq!(parse_count(Some(&json!({"simpleText": "1.2M views"}))), Some(1_200_000));
        assert_eq!(parse_count(Some(&json!({"simpleText": "988 views"}))), Some(988));
        assert_eq!(parse_duration_to_seconds(Some("PT4M13S")), Some(253));
        assert_eq!(parse_duration_to_seconds(Some("4:13")), Some(253));
        assert_eq!(parse_duration_to_seconds(Some("1:02:03")), Some(3723));
        assert_eq!(format_duration(Some(253)), Some("4:13".into()));
    }

    #[test]
    fn builds_search_params() {
        // sort=view_count alone → field 1 = 3
        let p = build_search_params(Some("view_count"), None, None, None).unwrap();
        assert_eq!(p, "CAM=");
        // type=video alone → nested field 2 = 1
        let p2 = build_search_params(None, None, Some("video"), None).unwrap();
        assert_eq!(p2, "EgIQAQ==");
        // video + this_week + medium: nested {1:2, 2:1, 3:2}
        let p3 = build_search_params(None, Some("this_week"), Some("video"), Some("medium")).unwrap();
        let decoded = base64::engine::general_purpose::STANDARD.decode(&p3).unwrap();
        assert!(decoded.contains(&0x12));
    }

    #[test]
    fn parses_video_renderer() {
        let node = json!({
            "videoId": "dQw4w9WgXcQ",
            "title": { "runs": [{ "text": "Never Gonna Give You Up" }] },
            "ownerText": { "runs": [{ "text": "Rick Astley", "navigationEndpoint": { "browseEndpoint": { "browseId": "UCuAXFkgsw1L7xaCfnd5JJOw" } } }] },
            "lengthText": { "simpleText": "3:33" },
            "viewCountText": { "simpleText": "1,000,000,000 views" }
        });
        let v = parse_video_renderer(&node).unwrap();
        assert_eq!(v["videoId"], "dQw4w9WgXcQ");
        assert_eq!(v["title"], "Never Gonna Give You Up");
        assert_eq!(v["channel"]["channelId"], "UCuAXFkgsw1L7xaCfnd5JJOw");
        assert_eq!(v["durationSeconds"], 213);
        assert_eq!(v["viewCount"], 1_000_000_000i64);
        assert_eq!(v["url"], "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    }

    #[test]
    fn parses_search_results_with_dedup() {
        let data = json!({
            "contents": {
                "twoColumnSearchResultsRenderer": {
                    "primaryContents": {
                        "sectionListRenderer": { "contents": [
                            { "itemSectionRenderer": { "contents": [
                                { "videoRenderer": { "videoId": "a", "title": { "simpleText": "A" } } },
                                { "videoRenderer": { "videoId": "a", "title": { "simpleText": "A dup" } } },
                                { "videoRenderer": { "videoId": "b", "title": { "simpleText": "B" } } }
                            ]}}
                        ]}}
                }
            }
        });
        let out = parse_search_results(&data, "test", None);
        assert_eq!(out["pageInfo"]["results"], 2);
        assert_eq!(out["contents"][0]["video"]["title"], "A");
    }
}
