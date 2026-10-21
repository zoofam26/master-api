//! master-api binary: binds the assembled router and serves it.

use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::web;
use actix_web::{App, HttpServer, middleware};
use master_common::state::AppState;
use master_common::Settings;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let _ = dotenvy::dotenv();
    let settings = Settings::from_env();
    init_tracing(&settings);

    let state = AppState::new(settings.clone()).expect("failed to build app state");
    let bind_addr = settings.bind_addr();
    let rate_limit = settings.rate_limit_per_minute;
    let cors_origin = settings.cors_origin.clone();

    tracing::info!(%bind_addr, count = master_api::SERVICES.len(), "master-api starting");

    // The middleware stack type depends on whether the governor is applied,
    // so the two configurations are built as separate server closures.
    if rate_limit > 0 {
        // Per-IP limiter: burst up to 100, sustained rate_limit/minute.
        let burst = rate_limit.clamp(1, 100) as u32;
        let per_second = (rate_limit / 60).max(1) as u64;
        #[allow(deprecated)]
        let governor = GovernorConfigBuilder::default()
            .per_second(per_second)
            .burst_size(burst)
            .finish()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "rate limiter init failed"))?;
        let state = state.clone();
        HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(state.clone()))
                .wrap(master_api::cors_middleware(&cors_origin))
                .wrap(master_common::middleware::RequestLogging)
                .wrap(master_common::middleware::ErrorEnvelope)
                .wrap(Governor::new(&governor))
                .wrap(middleware::Compress::default())
                .configure(master_api::configure_routes)
        })
        .workers(master_api::workers())
        .shutdown_timeout(30)
        .bind(&bind_addr)?
        .run()
        .await
    } else {
        let state = state.clone();
        HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(state.clone()))
                .wrap(master_api::cors_middleware(&cors_origin))
                .wrap(master_common::middleware::RequestLogging)
                .wrap(master_common::middleware::ErrorEnvelope)
                .wrap(middleware::Compress::default())
                .configure(master_api::configure_routes)
        })
        .workers(master_api::workers())
        .shutdown_timeout(30)
        .bind(&bind_addr)?
        .run()
        .await
    }
}

fn init_tracing(settings: &Settings) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if settings.json_logs {
        tracing_subscriber::fmt().json().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}
