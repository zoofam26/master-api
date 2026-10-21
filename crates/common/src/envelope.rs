//! Success envelope, byte-compatible with the original NestJS services:
//!
//! ```json
//! { "success": true, "data": { /* endpoint specific */ }, "meta": { /* ... */ } }
//! ```

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct Envelope {
    pub success: bool,
    pub data: Value,
    pub meta: Value,
}

impl Envelope {
    /// Build a success envelope from an endpoint-specific payload and meta.
    pub fn ok(data: impl Serialize, meta: Value) -> Self {
        Self {
            success: true,
            data: serde_json::to_value(data).unwrap_or(Value::Null),
            meta: if meta.is_null() {
                Value::Object(Default::default())
            } else {
                meta
            },
        }
    }

    /// Serialise the full envelope (`{ success, data, meta }`) to JSON.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Convenience builder for the recurring `meta` fields used across services
/// (`upstream`, `provider`, `cached`, `durationMs`). Services extend the
/// returned object freely via [`meta_extend`].
pub fn meta(upstream: &str, provider: &str, cached: bool, duration_ms: u64) -> Value {
    serde_json::json!({
        "upstream": upstream,
        "provider": provider,
        "cached": cached,
        "durationMs": duration_ms,
    })
}

/// Merge additional key/value pairs into a meta object.
pub fn meta_extend(base: Value, extra: Value) -> Value {
    match (base, extra) {
        (Value::Object(mut a), Value::Object(b)) => {
            for (k, v) in b {
                a.insert(k, v);
            }
            Value::Object(a)
        }
        (a, _) => a,
    }
}
