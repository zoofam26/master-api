//! The original service's error vocabulary
//! (`common/errors/transit.errors.ts`) expressed through
//! `master_common::error::ApiError`, with the `TRANSIT_` code prefix.
//!
//! - `InvalidRequestError` (400) → [`invalid_request`]
//! - `NotFoundError` (404) → [`not_found`]
//! - `FeatureUnavailableError` (422 upstream) → [`feature_unavailable`] (503,
//!   the closest status the shared envelope offers for "well formed but not
//!   answerable from this feed")
//! - `UpstreamError` / `ParseError` (502) → [`upstream`]

use serde_json::{json, Value};

use master_common::error::ApiError;

use crate::CODE_PREFIX;

/// 400 — the caller supplied a query or path parameter the service cannot use.
pub fn invalid_request(message: impl Into<String>, details: Value) -> ApiError {
    ApiError::validation_details(
        format!("{CODE_PREFIX}_INVALID_REQUEST"),
        message,
        details,
    )
}

/// 404 — the requested agency, route or stop does not exist.
pub fn not_found(message: impl Into<String>, details: Value) -> ApiError {
    ApiError::not_found_details(format!("{CODE_PREFIX}_NOT_FOUND"), message, details)
}

/// 503 — the request is well formed but cannot be answered from this feed.
pub fn feature_unavailable(message: impl Into<String>, details: Value) -> ApiError {
    ApiError::Unavailable {
        code: format!("{CODE_PREFIX}_FEATURE_UNAVAILABLE"),
        message: message.into(),
        details: Some(details),
    }
}

/// 502 — a feed host refused the request, or a feed failed to parse.
pub fn upstream(message: impl Into<String>, details: Value) -> ApiError {
    ApiError::Upstream {
        code: format!("{CODE_PREFIX}_UPSTREAM_ERROR"),
        message: message.into(),
        details: Some(details),
    }
}

/// A cloneable mirror of [`ApiError`].
///
/// Feed loads run inside moka's `try_get_with`, which hands the loader's error
/// back wrapped in an `Arc`. `ApiError` is not `Clone`, so the load path maps
/// it into this mirror (status, code, message, details are preserved) and maps
/// back to `ApiError` at the handler boundary.
#[derive(Debug, Clone)]
pub struct SnapshotError {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub details: Option<Value>,
}

impl From<ApiError> for SnapshotError {
    fn from(error: ApiError) -> Self {
        match error {
            ApiError::Validation { code, message, details } => Self { status: 400, code, message, details },
            ApiError::NotFound { code, message, details } => Self { status: 404, code, message, details },
            ApiError::RateLimited { code, message, details } => Self { status: 429, code, message, details },
            ApiError::Upstream { code, message, details } => Self { status: 502, code, message, details },
            ApiError::Unavailable { code, message, details } => Self { status: 503, code, message, details },
            ApiError::Timeout { code, message, details } => Self { status: 504, code, message, details },
            ApiError::Internal { message } => Self {
                status: 500,
                code: "INTERNAL_ERROR".to_string(),
                message,
                details: None,
            },
        }
    }
}

impl From<SnapshotError> for ApiError {
    fn from(error: SnapshotError) -> Self {
        let SnapshotError { status, code, message, details } = error;
        match status {
            400 => ApiError::Validation { code, message, details },
            404 => ApiError::NotFound { code, message, details },
            429 => ApiError::RateLimited { code, message, details },
            503 => ApiError::Unavailable { code, message, details },
            504 => ApiError::Timeout { code, message, details },
            _ => ApiError::Upstream { code, message, details },
        }
    }
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// 502 detail for a feed that downloaded but did not parse
/// (the original `ParseError`, message shaped like the TS).
pub fn parse_error(agency_name: &str, message: &str, agency_id: &str, feed_url: &str, zip_bytes: u64) -> ApiError {
    upstream(
        format!("The {agency_name} feed could not be parsed: {message}"),
        json!({ "agencyId": agency_id, "feedUrl": feed_url, "zipBytes": zip_bytes }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_carry_the_transit_prefix() {
        let error = invalid_request("bad", json!({}));
        assert_eq!(error.to_error_body()["error"]["code"], json!("TRANSIT_INVALID_REQUEST"));
        let error = not_found("gone", json!({}));
        assert_eq!(error.to_error_body()["error"]["code"], json!("TRANSIT_NOT_FOUND"));
        let error = feature_unavailable("nope", json!({}));
        assert_eq!(
            error.to_error_body()["error"]["code"],
            json!("TRANSIT_FEATURE_UNAVAILABLE")
        );
    }

    #[test]
    fn snapshot_errors_round_trip_through_the_mirror() {
        let original = ApiError::not_found_details(
            "TRANSIT_NOT_FOUND",
            "missing",
            json!({ "agencyId": "bart" }),
        );
        let mirror = SnapshotError::from(original);
        assert_eq!(mirror.status, 404);
        assert_eq!(mirror.code, "TRANSIT_NOT_FOUND");
        assert_eq!(mirror.details.as_ref(), Some(&json!({ "agencyId": "bart" })));
        let back = ApiError::from(mirror);
        assert_eq!(back.to_error_body()["error"]["details"]["agencyId"], json!("bart"));
        let _ = format!("{mirror}"); // Display is usable for logs
    }
}
