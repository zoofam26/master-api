//! Crate-local tunables, resolved once from the environment with the same
//! names (and the same defaults) as the original NestJS service's
//! `transitConfig` in `config/configuration.ts`.
//!
//! The feed snapshot TTL itself comes from the shared
//! `master_common::Settings::feed_cache_ttl` (`FEED_CACHE_TTL_SECONDS`), which
//! is the workspace-wide equivalent of the original
//! `TRANSIT__FEED_CACHE_TTL_MS`.

use once_cell::sync::OnceCell;

/// All knobs the original transit-api exposes, with original defaults.
#[derive(Debug, Clone)]
pub struct TransitConfig {
    /// Per-attempt timeout for a feed download (TRANSIT__FEED_TIMEOUT_MS).
    pub feed_timeout_ms: u64,
    /// Timeout for the cheap `Range` probe used by `/health`
    /// (TRANSIT__PROBE_TIMEOUT_MS).
    pub probe_timeout_ms: u64,
    /// Retries for transport errors, 429 and 5xx only (TRANSIT__MAX_RETRIES).
    pub max_retries: u32,
    /// Memory budget for a single inflated `stop_times.txt`
    /// (TRANSIT__MAX_STOP_TIMES_BYTES).
    pub max_stop_times_bytes: u64,
    /// Hard ceiling on the compressed feed size this service downloads
    /// (TRANSIT__MAX_FEED_BYTES).
    pub max_feed_bytes: u64,
    /// Simultaneous feed downloads; exceeding this queues
    /// (TRANSIT__MAX_CONCURRENCY).
    pub max_concurrency: usize,
    /// Hard ceiling on `limit` for list routes (TRANSIT__MAX_LIMIT).
    pub max_limit: i64,
    /// Default page size (TRANSIT__DEFAULT_LIMIT).
    pub default_limit: i64,
    /// Maximum `radius` accepted by `/transit/stops/nearby`, in metres
    /// (TRANSIT__MAX_RADIUS_METRES).
    pub max_radius_metres: i64,
    /// Largest `count` accepted by `/transit/departures`
    /// (TRANSIT__MAX_DEPARTURE_COUNT).
    pub max_departure_count: i64,
    /// User agent for feed requests (TRANSIT__USER_AGENT).
    pub user_agent: String,
}

impl Default for TransitConfig {
    fn default() -> Self {
        Self {
            feed_timeout_ms: 180_000,
            probe_timeout_ms: 15_000,
            max_retries: 1,
            max_stop_times_bytes: 48 * 1024 * 1024,
            max_feed_bytes: 64 * 1024 * 1024,
            max_concurrency: 3,
            max_limit: 200,
            default_limit: 20,
            max_radius_metres: 50_000,
            max_departure_count: 50,
            user_agent: "transit-api/1.0 (GTFS schedule data)".to_string(),
        }
    }
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
}

fn env_str(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

impl TransitConfig {
    fn from_env() -> Self {
        Self {
            feed_timeout_ms: env_num("TRANSIT__FEED_TIMEOUT_MS", 180_000),
            probe_timeout_ms: env_num("TRANSIT__PROBE_TIMEOUT_MS", 15_000),
            max_retries: env_num("TRANSIT__MAX_RETRIES", 1),
            max_stop_times_bytes: env_num("TRANSIT__MAX_STOP_TIMES_BYTES", 48 * 1024 * 1024),
            max_feed_bytes: env_num("TRANSIT__MAX_FEED_BYTES", 64 * 1024 * 1024),
            max_concurrency: env_num("TRANSIT__MAX_CONCURRENCY", 3usize).max(1),
            max_limit: env_num("TRANSIT__MAX_LIMIT", 200),
            default_limit: env_num("TRANSIT__DEFAULT_LIMIT", 20),
            max_radius_metres: env_num("TRANSIT__MAX_RADIUS_METRES", 50_000),
            max_departure_count: env_num("TRANSIT__MAX_DEPARTURE_COUNT", 50),
            user_agent: env_str("TRANSIT__USER_AGENT", "transit-api/1.0 (GTFS schedule data)"),
        }
    }
}

static CONFIG: OnceCell<TransitConfig> = OnceCell::new();

/// The resolved configuration, read from the environment at most once.
pub fn config() -> &'static TransitConfig {
    CONFIG.get_or_init(TransitConfig::from_env)
}
