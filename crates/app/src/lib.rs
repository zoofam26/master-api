//! master-api library surface: the assembled router for all eleven services,
//! shared by the binary and the integration tests.

pub mod docs;

use actix_cors::Cors;
use actix_web::web::{self, Json};
use actix_web::HttpResponse;
use serde_json::{json, Value};

use master_common::state::AppState;
#[allow(unused_imports)]
use master_common::Settings;

pub struct ServiceRegistration {
    prefix: &'static str,
    name: &'static str,
    router: fn(&mut web::ServiceConfig),
    openapi: fn() -> utoipa::openapi::OpenApi,
    index: fn() -> Value,
}

pub const SERVICES: &[ServiceRegistration] = &[
    ServiceRegistration { prefix: "/youtube", name: "youtube-api", router: svc_youtube::router, openapi: svc_youtube::openapi, index: svc_youtube::index },
    ServiceRegistration { prefix: "/books", name: "book-api", router: svc_books::router, openapi: svc_books::openapi, index: svc_books::index },
    ServiceRegistration { prefix: "/countries", name: "countries-api", router: svc_countries::router, openapi: svc_countries::openapi, index: svc_countries::index },
    ServiceRegistration { prefix: "/currency", name: "currency-api", router: svc_currency::router, openapi: svc_currency::openapi, index: svc_currency::index },
    ServiceRegistration { prefix: "/earthquake", name: "earthquake-api", router: svc_earthquake::router, openapi: svc_earthquake::openapi, index: svc_earthquake::index },
    ServiceRegistration { prefix: "/ip-geolocation", name: "ip-geolocation-api", router: svc_ip_geolocation::router, openapi: svc_ip_geolocation::openapi, index: svc_ip_geolocation::index },
    ServiceRegistration { prefix: "/postal-code", name: "postal-code-api", router: svc_postal_code::router, openapi: svc_postal_code::openapi, index: svc_postal_code::index },
    ServiceRegistration { prefix: "/public-holidays", name: "public-holidays-api", router: svc_public_holidays::router, openapi: svc_public_holidays::openapi, index: svc_public_holidays::index },
    ServiceRegistration { prefix: "/transit", name: "transit-api", router: svc_transit::router, openapi: svc_transit::openapi, index: svc_transit::index },
    ServiceRegistration { prefix: "/university", name: "university-api", router: svc_university::router, openapi: svc_university::openapi, index: svc_university::index },
    ServiceRegistration { prefix: "/weather", name: "weather-api", router: svc_weather::router, openapi: svc_weather::openapi, index: svc_weather::index },
];

pub fn configure_app(cfg: &mut web::ServiceConfig) {
    for svc in SERVICES {
        (svc.router)(cfg);
    }
}

// ------------------------------------------------------------------ routes

/// `GET /` — the master service catalogue.
pub async fn root() -> Json<Value> {
    Json(json!({
        "name": "master-api",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Unified Rust/Actix-Web backend aggregating eleven microservice APIs, each under its own prefix with its own OpenAPI document.",
        "services": SERVICES.iter().map(|s| {
            let mut idx = (s.index)();
            idx["docs"] = json!(format!("/docs{}", s.prefix));
            idx["openapi"] = json!(format!("{}/openapi.json", s.prefix));
            idx["health"] = json!(format!("{}/health", s.prefix));
            idx
        }).collect::<Vec<_>>(),
        "global": {
            "health": "/health",
            "docsIndex": "/docs",
            "openapiIndex": "/openapi.json",
        },
    }))
}

/// `GET /health` — global liveness (per-service probes live at /{svc}/health).
pub async fn global_health(state: web::Data<AppState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "uptimeSeconds": state.uptime_seconds(),
        "services": SERVICES.iter().map(|s| json!({
            "name": s.name,
            "prefix": s.prefix,
            "health": format!("{}/health", s.prefix),
        })).collect::<Vec<_>>(),
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    }))
}

/// `GET /openapi.json` — the index of per-service OpenAPI documents.
pub async fn openapi_index() -> Json<Value> {
    Json(json!({
        "name": "master-api",
        "note": "Each service publishes its own OpenAPI 3 document; this index points at all of them.",
        "documents": SERVICES.iter().map(|s| json!({
            "service": s.name,
            "prefix": s.prefix,
            "url": format!("{}/openapi.json", s.prefix),
            "docs": format!("/docs{}", s.prefix),
        })).collect::<Vec<_>>(),
    }))
}

/// `GET /docs` — human index of the Swagger UI pages.
pub async fn docs_index() -> HttpResponse {
    let services: Vec<Value> = SERVICES
        .iter()
        .map(|s| json!({ "name": s.name, "prefix": s.prefix, "url": format!("/docs{}", s.prefix) }))
        .collect();
    HttpResponse::Ok().content_type("text/html; charset=utf-8").body(docs::index_page(&services))
}

/// `GET /docs/{service}` — Swagger UI for one service.
pub async fn service_docs(path: web::Path<String>) -> HttpResponse {
    let prefix = format!("/{}", path.into_inner());
    match SERVICES.iter().find(|s| s.prefix == prefix) {
        Some(svc) => HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .body(docs::swagger_page(&format!("{prefix}/openapi.json"), svc.name)),
        None => HttpResponse::NotFound()
            .content_type("text/html; charset=utf-8")
            .body("<h1>404 — unknown service docs</h1>"),
    }
}

/// Serves `/{service}/openapi.json` for the given registration.
pub async fn serve_openapi(svc: &'static ServiceRegistration) -> HttpResponse {
    let doc = (svc.openapi)();
    match serde_json::to_string(&doc) {
        Ok(body) => HttpResponse::Ok().content_type("application/json").body(body),
        Err(e) => HttpResponse::InternalServerError()
            .content_type("application/json")
            .body(json!({ "success": false, "error": { "code": "INTERNAL_ERROR", "message": format!("{e}") } }).to_string()),
    }
}

/// The complete router configuration, shared by `main` and the tests.
pub fn configure_routes(cfg: &mut web::ServiceConfig) {
    cfg.route("/", web::get().to(root))
        .route("/health", web::get().to(global_health))
        .route("/openapi.json", web::get().to(openapi_index))
        .route("/docs", web::get().to(docs_index))
        .route("/docs/{service}", web::get().to(service_docs));
    for svc in SERVICES {
        let path = format!("{}/openapi.json", svc.prefix);
        cfg.route(&path, web::get().to(move || serve_openapi(svc)));
    }
    cfg.configure(configure_app)
        .default_service(web::to(not_found));
}

pub async fn not_found() -> HttpResponse {
    HttpResponse::NotFound().content_type("application/json").body(
        json!({
            "success": false,
            "error": { "code": "NOT_FOUND", "message": "The requested path does not exist. See GET / for the service catalogue." },
            "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        })
        .to_string(),
    )
}

// -------------------------------------------------------------------- main



pub fn cors_middleware(origin: &str) -> Cors {
    if origin == "*" {
        Cors::permissive()
    } else {
        let mut cors = Cors::default();
        for o in origin.split(',') {
            cors = cors.allowed_origin(o.trim());
        }
        cors.allowed_methods(vec!["GET", "POST", "OPTIONS"]).max_age(3600)
    }
}

pub fn workers() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}
