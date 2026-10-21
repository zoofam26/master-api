//! Unified API error type.
//!
//! Every handler returns `Result<HttpResponse, ApiError>`; the `ResponseError`
//! implementation renders every failure into the exact stable shape the
//! original NestJS `AllExceptionsFilter` produced:
//!
//! ```json
//! {
//!   "success": false,
//!   "error": { "code": "NOT_FOUND", "message": "...", "details": { } },
//!   "path": "/youtube/videos/abc",
//!   "timestamp": "2026-10-21T09:00:00.000Z"
//! }
//! ```
//!
//! `path` is injected by the [`crate::middleware::error_envelope`] middleware;
//! it is empty when the response never passed through a request.

use actix_web::{http::StatusCode, HttpResponse, ResponseError};
use serde_json::{json, Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// 400 — the client sent an invalid or missing parameter.
    #[error("{message}")]
    Validation { code: String, message: String, details: Option<Value> },

    /// 404 — the requested resource does not exist.
    #[error("{message}")]
    NotFound { code: String, message: String, details: Option<Value> },

    /// 429 — client hit the master-api rate limiter (or upstream quota).
    #[error("{message}")]
    RateLimited { code: String, message: String, details: Option<Value> },

    /// 502 — the upstream provider answered with an error or unusable payload.
    #[error("{message}")]
    Upstream { code: String, message: String, details: Option<Value> },

    /// 503 — the service is not usable in this deployment.
    #[error("{message}")]
    Unavailable { code: String, message: String, details: Option<Value> },

    /// 504 — the upstream provider timed out.
    #[error("{message}")]
    Timeout { code: String, message: String, details: Option<Value> },

    /// 500 — unexpected internal failure (message suppressed in response).
    #[error("{message}")]
    Internal { message: String },
}

impl ApiError {
    /// 400 with a service-specific validation code (e.g. `COUNTRIES_INVALID_SORT`).
    pub fn validation(code: impl Into<String>, message: impl Into<String>) -> Self {
        ApiError::Validation { code: code.into(), message: message.into(), details: None }
    }

    /// 400 with structured `details`.
    pub fn validation_details(
        code: impl Into<String>,
        message: impl Into<String>,
        details: Value,
    ) -> Self {
        ApiError::Validation { code: code.into(), message: message.into(), details: Some(details) }
    }

    /// 404 with a service-specific code.
    pub fn not_found(code: impl Into<String>, message: impl Into<String>) -> Self {
        ApiError::NotFound { code: code.into(), message: message.into(), details: None }
    }

    pub fn not_found_details(
        code: impl Into<String>,
        message: impl Into<String>,
        details: Value,
    ) -> Self {
        ApiError::NotFound { code: code.into(), message: message.into(), details: Some(details) }
    }

    /// 502 upstream failure with a service-specific code.
    pub fn upstream(code: impl Into<String>, message: impl Into<String>) -> Self {
        ApiError::Upstream { code: code.into(), message: message.into(), details: None }
    }

    /// 503 — capability missing in this deployment (e.g. optional key not set).
    pub fn unavailable(code: impl Into<String>, message: impl Into<String>) -> Self {
        ApiError::Unavailable { code: code.into(), message: message.into(), details: None }
    }

    /// 504 upstream timeout.
    pub fn timeout(code: impl Into<String>, message: impl Into<String>) -> Self {
        ApiError::Timeout { code: code.into(), message: message.into(), details: None }
    }

    /// 500 — internal; the response suppresses the message, logs carry it.
    pub fn internal(message: impl Into<String>) -> Self {
        ApiError::Internal { message: message.into() }
    }

    /// Map an upstream HTTP status to the most descriptive `ApiError`.
    /// `service_code_prefix` becomes the error code, e.g. `YOUTUBE_UPSTREAM`.
    pub fn from_upstream_status(status: u16, code_prefix: &str, body_snippet: &str) -> ApiError {
        let snippet: String = body_snippet.chars().take(240).collect();
        let snippet = snippet.trim();
        match status {
            404 => ApiError::not_found_details(
                format!("{code_prefix}_NOT_FOUND"),
                "The requested resource was not found upstream",
                json!({ "upstreamStatus": status }),
            ),
            429 => ApiError::upstream(
                format!("{code_prefix}_RATE_LIMITED"),
                format!("Upstream rate limit or daily quota exceeded (HTTP 429). {snippet}"),
            ),
            401 | 403 => ApiError::upstream(
                format!("{code_prefix}_FORBIDDEN"),
                format!("Upstream rejected the request (HTTP {status}). {snippet}"),
            ),
            408 => ApiError::timeout(
                format!("{code_prefix}_UPSTREAM_TIMEOUT"),
                "The upstream provider timed out".to_string(),
            ),
            s if (500..600).contains(&s) => ApiError::upstream(
                format!("{code_prefix}_UPSTREAM"),
                format!("Upstream provider error (HTTP {s}). {snippet}"),
            ),
            _ => ApiError::upstream(
                format!("{code_prefix}_UPSTREAM"),
                format!("Unexpected upstream response (HTTP {status}). {snippet}"),
            ),
        }
    }

    fn code(&self) -> &str {
        match self {
            ApiError::Validation { code, .. }
            | ApiError::NotFound { code, .. }
            | ApiError::RateLimited { code, .. }
            | ApiError::Upstream { code, .. }
            | ApiError::Unavailable { code, .. }
            | ApiError::Timeout { code, .. } => code,
            ApiError::Internal { .. } => "INTERNAL_ERROR",
        }
    }

    /// `details` for non-internal errors; internal errors never leak details.
    fn details(&self) -> Option<&Value> {
        match self {
            ApiError::Internal { .. } => None,
            _ => {
                let d = match self {
                    ApiError::Validation { details, .. }
                    | ApiError::NotFound { details, .. }
                    | ApiError::RateLimited { details, .. }
                    | ApiError::Upstream { details, .. }
                    | ApiError::Unavailable { details, .. }
                    | ApiError::Timeout { details, .. } => details,
                    ApiError::Internal { .. } => &None,
                };
                d.as_ref().filter(|v| !v.is_null())
            }
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            ApiError::Validation { .. } => StatusCode::BAD_REQUEST,
            ApiError::NotFound { .. } => StatusCode::NOT_FOUND,
            ApiError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            ApiError::Upstream { .. } => StatusCode::BAD_GATEWAY,
            ApiError::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Timeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            // 5xx internal: message suppressed by `response_message`.
            ApiError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// What the client is allowed to see in `error.message`.
    fn response_message(&self) -> String {
        match self {
            ApiError::Internal { .. } => "An unexpected internal error occurred".to_string(),
            other => other.to_string(),
        }
    }

    /// Render into the error envelope (without `path`, which the middleware adds).
    pub fn to_error_body(&self) -> Value {
        let mut error = Map::new();
        error.insert("code".into(), Value::String(self.code().to_string()));
        error.insert("message".into(), Value::String(self.response_message()));
        if let Some(details) = self.details() {
            error.insert("details".into(), details.clone());
        }
        json!({
            "success": false,
            "error": Value::Object(error),
            "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        })
    }
}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        self.status()
    }

    fn error_response(&self) -> HttpResponse {
        if let ApiError::Internal { message } = self {
            tracing::error!(message = %message, "internal error surfaced as 500");
        }
        HttpResponse::build(self.status())
            .content_type("application/json")
            .body(self.to_error_body().to_string())
    }
}

/// Convenience alias used by all handlers.
pub type ApiResult<T> = Result<T, ApiError>;
