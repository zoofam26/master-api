//! Cross-cutting actix middleware shared by every service:
//!
//! - [`error_envelope`] — injects the request path into error bodies so every
//!   failure carries the exact `"path"` field the original services emitted;
//! - [`request_logging`] — one structured log line per request.

use std::future::{ready, Ready};
use actix_web::body::{BoxBody, MessageBody};
use actix_web::dev::{forward_ready, Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::http::header::CONTENT_TYPE;
use actix_web::web::Bytes;
use actix_web::{Error, HttpResponse};
use futures::future::LocalBoxFuture;
use serde_json::Value;

/// Middleware factory for [`ErrorEnvelope`].
pub struct ErrorEnvelope;

impl<S, B> Transform<S, ServiceRequest> for ErrorEnvelope
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = Error;
    type InitError = ();
    type Transform = ErrorEnvelopeService<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(ErrorEnvelopeService { service }))
    }
}

pub struct ErrorEnvelopeService<S> {
    service: S,
}

impl<S, B> Service<ServiceRequest> for ErrorEnvelopeService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let path = req.path().to_string();
        let fut = self.service.call(req);
        Box::pin(async move {
            let res = fut.await?;
            let status = res.status();
            if status.as_u16() >= 400 && status != actix_web::http::StatusCode::NOT_MODIFIED {
                let is_json = res
                    .response()
                    .headers()
                    .get(CONTENT_TYPE)
                    .map(|v| v.to_str().unwrap_or("").contains("json"))
                    .unwrap_or(false);
                if is_json {
                    let (req, http) = res.into_parts();
                    let body = http.into_body();
                    let bytes = match body.try_into_bytes() {
                        Ok(b) => b,
                        Err(_) => Bytes::new(),
                    };
                    let enriched = if bytes.is_empty() {
                        None
                    } else {
                        match serde_json::from_slice::<Value>(&bytes) {
                            Ok(mut json) => {
                                if json.get("error").is_some() {
                                    json["path"] = Value::String(path);
                                    json["timestamp"] = Value::String(
                                        chrono::Utc::now()
                                            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                                    );
                                    Some(json)
                                } else {
                                    None
                                }
                            }
                            Err(_) => None,
                        }
                    };
                    let response = match enriched {
                        Some(json) => HttpResponse::build(status)
                            .content_type("application/json")
                            .body(json.to_string()),
                        None => HttpResponse::build(status).body(bytes),
                    };
                    return Ok(ServiceResponse::new(req, response));
                }
            }
            Ok(res.map_into_boxed_body())
        })
    }
}

/// Middleware factory for [`RequestLogging`].
pub struct RequestLogging;

impl<S, B> Transform<S, ServiceRequest> for RequestLogging
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type InitError = ();
    type Transform = RequestLoggingService<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(RequestLoggingService { service }))
    }
}

pub struct RequestLoggingService<S> {
    service: S,
}

impl<S, B> Service<ServiceRequest> for RequestLoggingService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let method = req.method().clone();
        let path = req.path().to_string();
        let started = std::time::Instant::now();
        let fut = self.service.call(req);
        Box::pin(async move {
            let res = fut.await?;
            let status = res.status().as_u16();
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let _ = &method;
            if status >= 500 {
                tracing::error!(method = %method, path = %path, status, elapsed_ms, "request");
            } else if status >= 400 {
                tracing::warn!(method = %method, path = %path, status, elapsed_ms, "request");
            } else {
                tracing::info!(method = %method, path = %path, status, elapsed_ms, "request");
            }
            Ok(res)
        })
    }
}

/// Convenience: build an owned `HttpResponse` with a JSON body.
pub fn json_response(status: actix_web::http::StatusCode, body: Value) -> HttpResponse<BoxBody> {
    HttpResponse::build(status)
        .content_type("application/json")
        .body(body.to_string())
}
