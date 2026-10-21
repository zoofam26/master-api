//! Shared application state: the pooled HTTP client, resolved settings, and
//! a registry of named TTL caches.
//!
//! Every service stores its upstream JSON responses in the registry, which
//! gives it for free:
//! - TTL expiry per cache name (`CACHE_TTL_<NAME>` overrides the default),
//! - **single-flight**: concurrent requests for the same key share one
//!   upstream call (moka `try_get_with`),
//! - bounded memory (`CACHE_MAX_CAPACITY` entries per cache).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use moka::future::Cache;
use serde_json::Value;

use crate::error::ApiError;
use crate::settings::Settings;

pub type JsonCache = Cache<String, Arc<Value>>;

/// Registry of named caches, created lazily on first use.
#[derive(Default)]
pub struct CacheRegistry {
    caches: RwLock<HashMap<String, JsonCache>>,
}

impl CacheRegistry {
    fn get_or_create(&self, name: &str, ttl: std::time::Duration, max_capacity: u64) -> JsonCache {
        if let Some(existing) = self.caches.read().expect("cache lock poisoned").get(name) {
            return existing.clone();
        }
        let cache = Cache::builder()
            .time_to_live(ttl)
            .max_capacity(max_capacity)
            .name(name)
            .build();
        self.caches
            .write()
            .expect("cache lock poisoned")
            .insert(name.to_string(), cache.clone());
        cache
    }
}

/// Shared state handed to every handler via `web::Data<AppState>`.
#[derive(Clone)]
pub struct AppState {
    pub http: reqwest::Client,
    pub settings: Settings,
    pub started_at: Instant,
    caches: Arc<CacheRegistry>,
}

impl AppState {
    pub fn new(settings: Settings) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(settings.upstream_timeout)
            .connect_timeout(std::time::Duration::from_secs(10))
            .pool_max_idle_per_host(20)
            .user_agent(concat!(
                "master-api/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/zoofam26/master-api)"
            ))
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))?;
        Ok(Self {
            http,
            settings,
            started_at: Instant::now(),
            caches: Arc::new(CacheRegistry::default()),
        })
    }

    /// Fetch (or lazily create) the named JSON cache with the right TTL.
    pub fn json_cache(&self, name: &str) -> JsonCache {
        let ttl = self.settings.ttl_for(name);
        self.caches
            .get_or_create(name, ttl, self.settings.cache_max_capacity)
    }

    /// Uptime in whole seconds, for health reports.
    pub fn uptime_seconds(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

/// Cache names used across services (single source of truth).
pub mod cache_names {
    pub const YOUTUBE: &str = "YOUTUBE";
    pub const BOOKS: &str = "BOOKS";
    pub const COUNTRIES: &str = "COUNTRIES";
    pub const CURRENCY: &str = "CURRENCY";
    pub const EARTHQUAKE: &str = "EARTHQUAKE";
    pub const AIR_QUALITY: &str = "AIR_QUALITY";
    pub const IP_GEOLOCATION: &str = "IP_GEOLOCATION";
    pub const POSTAL_CODE: &str = "POSTAL_CODE";
    pub const PUBLIC_HOLIDAYS: &str = "PUBLIC_HOLIDAYS";
    pub const TRANSIT: &str = "TRANSIT";
    pub const UNIVERSITY: &str = "UNIVERSITY";
    pub const WEATHER: &str = "WEATHER";
    pub const GEOCODING: &str = "GEOCODING";
    /// Probe results for health endpoints (very short TTL).
    pub const HEALTH_PROBES: &str = "HEALTH_PROBES";
}

/// Error type for `try_get_with` loaders.
pub type CacheLoadError = ApiError;
