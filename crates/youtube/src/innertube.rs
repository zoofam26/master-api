//! InnerTube transport: homepage credential scraping, client profiles with
//! per-endpoint fallback chains, resilient POSTs. Ported from
//! `youtube-api/src/innertube/*`.

use std::sync::Arc;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};

use crate::CODE_PREFIX;
use master_common::error::ApiError;
use master_common::httpx;
use master_common::state::{cache_names, AppState};

pub const INNERTUBE_BASE: &str = "https://www.youtube.com/youtubei/v1";
pub const YOUTUBE_HOMEPAGE: &str = "https://www.youtube.com/";

/// Well-known InnerTube endpoints we consume.
pub const EP_PLAYER: &str = "player";
pub const EP_SEARCH: &str = "search";
pub const EP_BROWSE: &str = "browse";
pub const EP_NEXT: &str = "next";
pub const EP_GUIDE: &str = "guide";
pub const EP_RESOLVE: &str = "resolve_url";

const DESKTOP_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// A client identity; YouTube serves materially different payloads per client.
#[derive(Clone, Copy)]
pub struct ClientProfile {
    pub name: &'static str,
    pub client_id: u32,
    pub version: &'static str,
    pub user_agent: &'static str,
}

pub const WEB: ClientProfile = ClientProfile { name: "WEB", client_id: 1, version: "2.20241211.01.00", user_agent: DESKTOP_UA };
pub const ANDROID: ClientProfile = ClientProfile {
    name: "ANDROID",
    client_id: 3,
    version: "20.10.38",
    user_agent: "com.google.android.youtube/20.10.38 (Linux; U; Android 11) gzip",
};
pub const IOS: ClientProfile = ClientProfile {
    name: "IOS",
    client_id: 5,
    version: "20.10.4",
    user_agent: "com.google.ios.youtube/20.10.4 (iPhone16,2; U; CPU iOS 18_3_1 like Mac OS X)",
};

/// Ordered fallback chain per endpoint, best profile first.
pub fn fallbacks_for(endpoint: &str) -> Vec<ClientProfile> {
    match endpoint {
        EP_PLAYER => vec![ANDROID, IOS, WEB],
        EP_SEARCH | EP_BROWSE | EP_NEXT | EP_GUIDE | EP_RESOLVE => vec![WEB, ANDROID, IOS],
        _ => vec![WEB, ANDROID, IOS],
    }
}

pub struct Credentials {
    pub api_key: String,
    pub web_client_version: String,
    pub visitor_data: Option<String>,
}

static SCRAPE_KEY: Lazy<Regex> = Lazy::new(|| Regex::new("\"INNERTUBE_API_KEY\":\"([^\"]+)\"").unwrap());
static SCRAPE_VERSION: Lazy<Regex> = Lazy::new(|| Regex::new("\"INNERTUBE_CLIENT_VERSION\":\"([^\"]+)\"").unwrap());
static SCRAPE_VISITOR: Lazy<Regex> = Lazy::new(|| Regex::new("\"visitorData\":\"([^\"]+)\"").unwrap());

/// The public web key ships to every youtube.com visitor; it is the fallback
/// when the homepage scrape is unavailable.
const FALLBACK_WEB_KEY: &str = "AIzaSyAO_FJ2SlqU8Q4STEHLGCilw_Y9_11qcW8";

/// Scrapes (or loads cached) InnerTube credentials; cached ~1h.
pub async fn get_credentials(state: &AppState) -> Result<Credentials, ApiError> {
    let cache = state.json_cache(cache_names::YOUTUBE);
    if let Some(v) = cache.get("innertube_credentials").await {
        return Ok(Credentials {
            api_key: v.get("apiKey").and_then(|x| x.as_str()).unwrap_or(FALLBACK_WEB_KEY).to_string(),
            web_client_version: v.get("webVersion").and_then(|x| x.as_str()).unwrap_or(WEB.version).to_string(),
            visitor_data: v.get("visitorData").and_then(|x| x.as_str()).map(String::from),
        });
    }
    let started = std::time::Instant::now();
    let response = state
        .http
        .get(YOUTUBE_HOMEPAGE)
        .header("user-agent", DESKTOP_UA)
        .header("accept", "text/html,application/xhtml+xml")
        .send()
        .await
        .map_err(|e| ApiError::upstream(format!("{CODE_PREFIX}_UNREACHABLE"), format!("Could not fetch the YouTube homepage: {e}")))?;
    let html = response
        .text()
        .await
        .map_err(|e| ApiError::upstream(format!("{CODE_PREFIX}_UPSTREAM"), format!("Could not read the YouTube homepage: {e}")))?;
    let _ = started;

    let api_key = SCRAPE_KEY.captures(&html).and_then(|c| c.get(1)).map(|m| m.as_str().to_string());
    let web_version = SCRAPE_VERSION.captures(&html).and_then(|c| c.get(1)).map(|m| m.as_str().to_string());
    let visitor = SCRAPE_VISITOR.captures(&html).and_then(|c| c.get(1)).map(|m| m.as_str().to_string());

    let creds = Credentials {
        api_key: api_key.clone().unwrap_or_else(|| FALLBACK_WEB_KEY.to_string()),
        web_client_version: web_version.clone().unwrap_or_else(|| WEB.version.to_string()),
        visitor_data: visitor.clone(),
    };
    let stored = json!({
        "apiKey": creds.api_key,
        "webVersion": creds.web_client_version,
        "visitorData": visitor,
    });
    cache.insert("innertube_credentials".to_string(), Arc::new(stored)).await;
    if api_key.is_some() {
        tracing::info!("InnerTube credentials scraped from the YouTube homepage");
    } else {
        tracing::warn!("Homepage scrape incomplete; using the well-known public web key");
    }
    Ok(creds)
}

fn context(profile: &ClientProfile, version: &str, hl: &str, gl: &str, visitor: Option<&String>) -> Value {
    let mut client = json!({
        "clientName": profile.name,
        "clientVersion": if profile.name == "WEB" { version } else { profile.version },
        "hl": hl,
        "gl": gl,
    });
    if let Some(v) = visitor {
        client["visitorData"] = json!(v);
    }
    if profile.name == "ANDROID" {
        client["androidSdkVersion"] = json!(30);
        client["osName"] = json!("Android");
        client["osVersion"] = json!("11");
    }
    if profile.name == "IOS" {
        client["deviceMake"] = json!("Apple");
        client["deviceModel"] = json!("iPhone16,2");
        client["osName"] = json!("iPhone");
        client["osVersion"] = json!("18.3.1.22D72");
    }
    json!({ "client": client })
}

/// POSTs to an InnerTube endpoint, walking the client fallback chain.
pub async fn post(
    state: &AppState,
    endpoint: &str,
    body: Value,
    hl: &str,
    gl: &str,
    forced_profile: Option<ClientProfile>,
) -> Result<Value, ApiError> {
    let creds = get_credentials(state).await?;
    let chain = match forced_profile {
        Some(p) => vec![p],
        None => fallbacks_for(endpoint),
    };
    let mut last_err: Option<ApiError> = None;

    for profile in chain {
        let effective_version = if profile.name == "WEB" { creds.web_client_version.clone() } else { profile.version.to_string() };
        let payload = merge(
            json!({
                "context": context(&profile, &creds.web_client_version, hl, gl, creds.visitor_data.as_ref()),
                "racyCheckOk": true,
                "contentCheckOk": true,
            }),
            body.clone(),
        );

        let url = format!("{INNERTUBE_BASE}/{endpoint}?key={}&prettyPrint=false", creds.api_key);
        let headers = [
            ("content-type", "application/json".to_string()),
            ("user-agent", effective_version_user_agent(&profile)),
            ("x-youtube-client-name", profile.client_id.to_string()),
            ("x-youtube-client-version", effective_version.clone()),
            ("x-goog-api-format-version", "2".to_string()),
            ("origin", "https://www.youtube.com".to_string()),
            ("accept", "application/json".to_string()),
        ];
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();

        match httpx::post_json_owned(state, &url, &payload, &headers, CODE_PREFIX).await {
            Ok(t) => return Ok(t.body),
            Err(e) => {
                // Client fallbacks are only meaningful for payload-level
                // failures (empty renderers); transport errors surface.
                if matches!(e, ApiError::Upstream { .. }) || matches!(e, ApiError::Timeout { .. }) {
                    last_err = Some(e);
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| ApiError::upstream(format!("{CODE_PREFIX}_UPSTREAM"), "InnerTube request failed")))
}

fn effective_version_user_agent(profile: &ClientProfile) -> String {
    profile.user_agent.to_string()
}

/// Shallow merge: `b` wins over `a` at the top level.
pub fn merge(mut a: Value, b: Value) -> Value {
    let mut b = b;
    if let (Some(am), Some(bm)) = (a.as_object_mut(), b.as_object_mut()) {
        for (k, v) in bm {
            am.insert(k.clone(), v.clone());
        }
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_shallow() {
        let a = json!({ "context": { "client": "WEB" }, "racyCheckOk": true });
        let b = json!({ "query": "test" });
        let m = merge(a, b);
        assert_eq!(m["query"], "test");
        assert_eq!(m["racyCheckOk"], true);
    }

    #[test]
    fn fallback_chains() {
        assert_eq!(fallbacks_for(EP_PLAYER)[0].name, "ANDROID");
        assert_eq!(fallbacks_for(EP_SEARCH)[0].name, "WEB");
        assert_eq!(fallbacks_for(EP_BROWSE)[0].name, "WEB");
    }

    #[test]
    fn contexts_per_profile() {
        let c = context(&ANDROID, "2.x", "en", "US", None);
        assert_eq!(c["client"]["clientName"], "ANDROID");
        assert_eq!(c["client"]["androidSdkVersion"], 30);
        let w = context(&WEB, "2.20241211.01.00", "en", "US", None);
        assert_eq!(w["client"]["clientVersion"], "2.20241211.01.00");
    }
}
