//! Environment-driven configuration with sane defaults.
//!
//! Every knob is an env var so the same binary runs unmodified in local dev,
//! Docker Compose, or Kubernetes (12-factor style). No env var is required:
//! the defaults work out of the box.

use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Settings {
    /// Bind address (default `0.0.0.0`).
    pub host: String,
    /// Bind port (default 8080).
    pub port: u16,
    /// CORS allowed origins (`*` by default, like the original services).
    pub cors_origin: String,
    /// Per-upstream-request timeout (default 15s).
    pub upstream_timeout: Duration,
    /// Extra attempts after the first failure for idempotent GETs (default 1).
    pub upstream_retries: u32,
    /// Base backoff step, doubled per attempt with jitter (default 200ms).
    pub retry_base_delay_ms: u64,
    /// Default TTL for JSON response caches (default 300s).
    pub cache_ttl: Duration,
    /// Max cached entries per cache (default 10_000).
    pub cache_max_capacity: u64,
    /// Per-cache TTL overrides, e.g. `CACHE_TTL_CURRENCY=900`.
    pub cache_ttl_overrides: HashMap<String, Duration>,
    /// TTL for downloaded dataset/feed snapshots (GeoNames, GTFS): default 6h.
    pub feed_cache_ttl: Duration,
    /// Client-facing rate limit in requests/minute per IP (0 disables).
    pub rate_limit_per_minute: u32,
    /// Optional Google Books API key (avoids anonymous shared-IP quota).
    pub books_api_key: Option<String>,
    /// Emit logs as JSON (default false).
    pub json_logs: bool,
    /// Upstream base-URL overrides, keyed by upper-case service alias,
    /// e.g. `UPSTREAM_NAGER=https://date.nager.at`.
    pub upstream_overrides: HashMap<String, String>,
}

fn env_str(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    env_str(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

impl Settings {
    pub fn from_env() -> Self {
        let mut cache_ttl_overrides = HashMap::new();
        let mut upstream_overrides = HashMap::new();
        for (key, value) in std::env::vars() {
            let key = key.trim().to_uppercase();
            let value = value.trim().to_string();
            if value.is_empty() {
                continue;
            }
            if let Some(rest) = key.strip_prefix("CACHE_TTL_") {
                if let Ok(secs) = value.parse::<u64>() {
                    cache_ttl_overrides.insert(rest.to_string(), Duration::from_secs(secs));
                }
            } else if let Some(rest) = key.strip_prefix("UPSTREAM_") {
                upstream_overrides.insert(rest.to_string(), value);
            }
        }

        Self {
            host: env_str("APP_HOST").unwrap_or_else(|| "0.0.0.0".into()),
            port: env_parse("APP_PORT", 8080),
            cors_origin: env_str("CORS_ORIGIN").unwrap_or_else(|| "*".into()),
            upstream_timeout: Duration::from_secs(env_parse("UPSTREAM_TIMEOUT_SECONDS", 15u64)),
            upstream_retries: env_parse("UPSTREAM_MAX_RETRIES", 1u32),
            retry_base_delay_ms: env_parse("RETRY_BASE_DELAY_MS", 200u64),
            cache_ttl: Duration::from_secs(env_parse("CACHE_TTL_SECONDS", 300u64)),
            cache_max_capacity: env_parse("CACHE_MAX_CAPACITY", 10_000u64),
            cache_ttl_overrides,
            feed_cache_ttl: Duration::from_secs(env_parse("FEED_CACHE_TTL_SECONDS", 21_600u64)),
            rate_limit_per_minute: env_parse("RATE_LIMIT_PER_MINUTE", 600u32),
            books_api_key: env_str("GOOGLE_BOOKS_API_KEY"),
            json_logs: env_str("JSON_LOGS")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            upstream_overrides,
        }
    }

    /// TTL for a named cache, honouring per-cache overrides (`CACHE_TTL_<NAME>`).
    pub fn ttl_for(&self, cache: &str) -> Duration {
        let name = cache.to_uppercase();
        *self
            .cache_ttl_overrides
            .get(&name)
            .unwrap_or(&self.cache_ttl)
    }

    /// Upstream base URL for a service alias, honouring `UPSTREAM_<ALIAS>`.
    /// Example: `settings.upstream("NAGER", "https://date.nager.at")`.
    pub fn upstream(&self, alias: &str, default: &str) -> String {
        self.upstream_overrides
            .get(alias)
            .cloned()
            .unwrap_or_else(|| default.to_string())
    }

    pub fn bind_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
