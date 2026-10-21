//! Local URL resolution for YouTube links (watch/shorts/channel/handle/
//! playlist), used by `/youtube/resolve` — the upstream resolve_url endpoint
//! 404s watch URLs for anonymous clients.

use serde_json::{json, Value};

pub enum ResolvedUrl {
    Video { id: String },
    Channel { id: String },
    Handle { handle: String },
    Playlist { id: String },
}

const VIDEO_ID_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_";

fn looks_like_video_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 20
        && s.chars().all(|c| VIDEO_ID_CHARS.contains(c))
        && s.chars().any(|c| c.is_ascii_alphabetic())
}

/// Parses a YouTube URL (or bare id) into its target.
pub fn parse_youtube_url(input: &str) -> Option<ResolvedUrl> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Bare ids.
    if looks_like_video_id(trimmed) && !trimmed.starts_with('@') && !trimmed.starts_with("UC") {
        // A bare 11-char id is assumed to be a video.
        return Some(ResolvedUrl::Video { id: trimmed.to_string() });
    }
    if trimmed.starts_with("UC") && looks_like_channel_id(trimmed) {
        return Some(HandlelessChannel(trimmed.to_string()).into_resolved());
    }

    // Normalise scheme/host.
    let mut path = trimmed.to_string();
    if let Some(pos) = path.find("://") {
        path = path[pos + 3..].to_string();
    }
    if path.starts_with("www.") {
        path = path[4..].to_string();
    } else if path.starts_with("m.") || path.starts_with("music.") {
        path = path.split_once('.').map(|(_, rest)| rest.to_string()).unwrap_or(path);
    } else if path.starts_with("youtube.") || path.starts_with("youtu.be") {
        // host without www
    }
    if !path.starts_with("youtube") && !path.starts_with("youtu.be") {
        // Could be a bare path like "watch?v=..." or "@handle"
        if path.starts_with("watch?v=") {
            let id = path.trim_start_matches("watch?v=").split('&').next().unwrap_or("");
            if looks_like_video_id(id) {
                return Some(ResolvedUrl::Video { id: id.to_string() });
            }
            return None;
        }
        if path.starts_with('@') {
            let handle = path.split(|c| c == '/' || c == '?' || c == '#').next().unwrap_or("");
            if handle.len() > 1 {
                return Some(ResolvedUrl::Handle { handle: handle.to_string() });
            }
        }
        return None;
    }

    // youtube.com/... or youtu.be/...
    let (host, rest) = if path.starts_with("youtu.be") {
        ("youtu.be", path.trim_start_matches("youtu.be").trim_start_matches('/').to_string())
    } else {
        let after = path.trim_start_matches("youtube.com").trim_start_matches('/').to_string();
        ("youtube.com", after)
    };

    if host == "youtu.be" {
        let id = rest.split(|c| c == '/' || c == '?' || c == '#').next().unwrap_or("");
        if looks_like_video_id(id) {
            return Some(ResolvedUrl::Video { id: id.to_string() });
        }
        return None;
    }

    let segments: Vec<&str> = rest.split(|c| c == '/' || c == '?' || c == '#' || c == '&')
        .filter(|s| !s.is_empty())
        .collect();

    // watch?v={id} in query form.
    if let Some(v) = trimmed.split("v=").nth(1) {
        let id = v.split('&').next().unwrap_or("");
        if looks_like_video_id(id) {
            return Some(ResolvedUrl::Video { id: id.to_string() });
        }
    }

    match segments.first().copied() {
        Some("watch") => {
            let id = segments.iter().find(|s| s.starts_with("v=")).map(|s| s[2..].to_string());
            match id {
                Some(id) if looks_like_video_id(&id) => Some(ResolvedUrl::Video { id }),
                _ => None,
            }
        }
        Some("shorts") | Some("live") | Some("embed") | Some("v") => {
            let id = segments.get(1).copied().unwrap_or("");
            if looks_like_video_id(id) {
                Some(ResolvedUrl::Video { id: id.to_string() })
            } else {
                None
            }
        }
        Some("channel") => {
            let id = segments.get(1).copied().unwrap_or("");
            if looks_like_channel_id(id) {
                Some(ResolvedUrl::Channel { id: id.to_string() })
            } else {
                None
            }
        }
        Some("playlist") => {
            let list = segments.iter().find(|s| s.starts_with("list=")).map(|s| s[5..].to_string());
            match list {
                Some(id) if !id.is_empty() => Some(ResolvedUrl::Playlist { id }),
                _ => None,
            }
        }
        Some(first) if first.starts_with('@') => {
            Some(ResolvedUrl::Handle { handle: first.to_string() })
        }
        Some("c") | Some("user") => {
            let handle = segments.get(1).copied().unwrap_or("");
            if !handle.is_empty() {
                Some(ResolvedUrl::Handle { handle: format!("@{handle}") })
            } else {
                None
            }
        }
        _ => None,
    }
}

fn looks_like_channel_id(s: &str) -> bool {
    s.len() == 24 && s.starts_with("UC") && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl HandlelessChannel {
    fn into_resolved(self) -> ResolvedUrl {
        ResolvedUrl::Channel { id: self.0 }
    }
}

struct HandlelessChannel(String);

/// Serialises a resolved target for tests.
pub fn describe(r: &ResolvedUrl) -> Value {
    match r {
        ResolvedUrl::Video { id } => json!({ "type": "video", "id": id }),
        ResolvedUrl::Channel { id } => json!({ "type": "channel", "id": id }),
        ResolvedUrl::Handle { handle } => json!({ "type": "handle", "id": handle }),
        ResolvedUrl::Playlist { id } => json!({ "type": "playlist", "id": id }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_watch_urls() {
        assert!(matches!(
            parse_youtube_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            Some(ResolvedUrl::Video { id }) if id == "dQw4w9WgXcQ"
        ));
        assert!(matches!(
            parse_youtube_url("youtube.com/watch?v=9bZkp7q19f0&t=1s"),
            Some(ResolvedUrl::Video { id }) if id == "9bZkp7q19f0"
        ));
        assert!(matches!(
            parse_youtube_url("https://youtu.be/dQw4w9WgXcQ"),
            Some(ResolvedUrl::Video { id }) if id == "dQw4w9WgXcQ"
        ));
        assert!(matches!(
            parse_youtube_url("https://www.youtube.com/shorts/abcdefghijk"),
            Some(ResolvedUrl::Video { id }) if id == "abcdefghijk"
        ));
    }

    #[test]
    fn parses_channels_and_playlists() {
        assert!(matches!(
            parse_youtube_url("https://www.youtube.com/channel/UCuAXFkgsw1L7xaCfnd5JJOw"),
            Some(ResolvedUrl::Channel { id }) if id == "UCuAXFkgsw1L7xaCfnd5JJOw"
        ));
        assert!(matches!(
            parse_youtube_url("https://www.youtube.com/@rickastley"),
            Some(ResolvedUrl::Handle { handle }) if handle == "@rickastley"
        ));
        assert!(matches!(
            parse_youtube_url("https://www.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI"),
            Some(ResolvedUrl::Playlist { id }) if id == "PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI"
        ));
        assert!(matches!(
            parse_youtube_url("watch?v=dQw4w9WgXcQ"),
            Some(ResolvedUrl::Video { id }) if id == "dQw4w9WgXcQ"
        ));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_youtube_url("").is_none());
        assert!(parse_youtube_url("https://example.com/foo").is_none());
        assert!(parse_youtube_url("https://www.youtube.com/watch?v=###").is_none());
    }
}
