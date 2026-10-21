//! master-common — shared infrastructure for every service of the master-api
//! monolith.
//!
//! Provides: the success envelope (`Envelope`), the unified error type
//! (`ApiError`) matching the error body the original NestJS services
//! produced, environment-driven `Settings`, the shared `AppState`
//! (HTTP client + per-name TTL caches with single-flight) and resilient
//! upstream helpers (timeouts, retries, timing).

pub mod envelope;
pub mod error;
pub mod httpx;
pub mod middleware;
pub mod settings;
pub mod state;

pub use envelope::{meta, meta_extend, Envelope};
pub use error::ApiError;
pub use settings::Settings;
pub use state::{AppState, CacheRegistry};
