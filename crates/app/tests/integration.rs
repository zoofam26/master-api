//! Live integration tests: the full router is exercised in-process against
//! the real upstreams.
//!
//! These tests are tolerant of external free-tier quotas: an endpoint that
//! answers with our own error envelope (502/503/504 due to an upstream's
//! shared-IP quota) counts as "upstream unavailable", not a code failure.
//! Set SKIP_LIVE=1 to skip the whole suite (e.g. in offline CI).

use actix_web::dev::ServiceResponse;
use actix_web::http::StatusCode;
use actix_web::test;
use serde_json::Value;

use master_common::state::AppState;
use master_common::Settings;

fn skip_live() -> bool {
    std::env::var("SKIP_LIVE").map(|v| v == "1").unwrap_or(false)
}

async fn make_state() -> AppState {
    let mut settings = Settings::from_env();
    settings.rate_limit_per_minute = 0; // no limiter in tests
    AppState::new(settings).expect("state")
}

async fn call(state: &AppState, path: &str) -> (StatusCode, Value) {
    let app = test::init_service(
        actix_web::App::new()
            .app_data(actix_web::web::Data::new(state.clone()))
            .configure(master_api::configure_routes),
    )
    .await;
    let req = test::TestRequest::get().uri(path).to_request();
    let resp: ServiceResponse = test::call_service(&app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    let json = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    (status, json)
}

fn is_our_error(json: &Value) -> bool {
    json.get("success").and_then(|v| v.as_bool()) == Some(false)
}

fn assert_ok_or_upstream(json: &Value, status: StatusCode, what: &str) {
    if status.is_success() {
        assert_eq!(
            json.get("success").and_then(|v| v.as_bool()),
            Some(true),
            "{what}: expected success envelope"
        );
    } else {
        assert!(is_our_error(json), "{what}: non-2xx must carry our error envelope, got {json}");
        assert!(
            (500..=504).contains(&status.as_u16()),
            "{what}: upstream quota failures must be 5xx, got {status}"
        );
    }
}

#[actix_web::test]
async fn global_routes() {
    if skip_live() {
        return;
    }
    let state = make_state().await;

    let (status, json) = call(&state, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["services"].as_array().map(|a| a.len()), Some(11));

    let (status, _) = call(&state, "/health").await;
    assert_eq!(status, StatusCode::OK);

    let (status, json) = call(&state, "/openapi.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["documents"].as_array().map(|a| a.len()), Some(11));

    for svc in [
        "youtube", "books", "countries", "currency", "earthquake", "ip-geolocation",
        "postal-code", "public-holidays", "transit", "university", "weather",
    ] {
        let (status, _) = call(&state, &format!("/{svc}/openapi.json")).await;
        assert_eq!(status, StatusCode::OK, "/{svc}/openapi.json");
    }

    let (status, json) = call(&state, "/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(is_our_error(&json));
}

#[actix_web::test]
async fn countries_endpoints() {
    if skip_live() {
        return;
    }
    let state = make_state().await;

    let (status, json) = call(&state, "/countries/countries?limit=5&search=tanz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["meta"]["limit"], 5);
    assert!(json["data"]["countries"].as_array().unwrap().len() <= 5);

    let (status, json) = call(&state, "/countries/countries/TZ").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["data"]["country"]["alpha2"], "TZ");

    let (status, json) = call(&state, "/countries/countries/TZ/neighbours").await;
    assert_eq!(status, StatusCode::OK);
    assert!(json["data"]["count"].as_u64().unwrap_or(0) >= 1);

    let (status, json) = call(&state, "/countries/countries/ZZ").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(is_our_error(&json));
}

#[actix_web::test]
async fn currency_endpoints() {
    if skip_live() {
        return;
    }
    let state = make_state().await;

    let (status, json) = call(&state, "/currency/rates/latest?base=USD&symbols=EUR").await;
    assert_ok_or_upstream(&json, status, "rates/latest");
    if status == StatusCode::OK {
        assert_eq!(json["data"]["base"], "USD");
        assert!(json["data"]["rates"].as_array().unwrap().iter().any(|r| r["quote"] == "EUR"));
    }

    let (status, json) = call(&state, "/currency/convert?from=USD&to=EUR&amount=100").await;
    assert_ok_or_upstream(&json, status, "convert");

    let (status, json) = call(&state, "/currency/providers").await;
    assert_eq!(status, StatusCode::OK);

    let (status, json) = call(&state, "/currency/convert?from=USD").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(is_our_error(&json));
}

#[actix_web::test]
async fn holidays_endpoints() {
    if skip_live() {
        return;
    }
    let state = make_state().await;

    let (status, json) = call(&state, "/public-holidays/holidays/TZ?year=2026").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["countryCode"], "TZ");
    assert!(json["items"].as_array().unwrap().len() >= 1);

    let (status, json) = call(&state, "/public-holidays/holidays/TZ?year=1999").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(is_our_error(&json));
}

#[actix_web::test]
async fn ip_geolocation_endpoints() {
    if skip_live() {
        return;
    }
    let state = make_state().await;

    let (status, json) = call(&state, "/ip-geolocation/validate/192.168.1.1").await;
    assert_eq!(status, StatusCode::OK); // local, no upstream
    assert_eq!(json["data"]["scope"], "private");

    let (status, json) = call(&state, "/ip-geolocation/ip/8.8.8.8").await;
    assert_ok_or_upstream(&json, status, "ip lookup");

    let (status, json) = call(&state, "/ip-geolocation/ip/nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(is_our_error(&json));
}

#[actix_web::test]
async fn university_endpoints() {
    if skip_live() {
        return;
    }
    let state = make_state().await;

    let (status, json) = call(&state, "/university/universities?search=oxford&limit=3").await;
    assert_ok_or_upstream(&json, status, "universities search");
    if status == StatusCode::OK {
        assert!(json["data"]["results"].as_array().unwrap().len() <= 3);
        let id = json["data"]["results"][0]["id"].as_str().unwrap_or_default().to_string();
        let (status2, json2) = call(&state, &format!("/university/universities/{id}")).await;
        assert_eq!(status2, StatusCode::OK);
        assert_eq!(json2["data"]["university"]["id"], id);
    }
}

#[actix_web::test]
async fn earthquake_endpoints() {
    if skip_live() {
        return;
    }
    let state = make_state().await;

    let (status, json) = call(&state, "/earthquake?minmagnitude=5&limit=3&starttime=2026-10-01").await;
    assert_ok_or_upstream(&json, status, "earthquake list");
    if status == StatusCode::OK {
        let eqs = json["data"]["earthquakes"].as_array().unwrap();
        assert!(eqs.len() <= 3);
        if let Some(first) = eqs.first() {
            let id = first["id"].as_str().unwrap_or_default().to_string();
            let (status2, json2) = call(&state, &format!("/earthquake/{id}")).await;
            assert_eq!(status2, StatusCode::OK);
            assert_eq!(json2["data"]["earthquake"]["id"], id);
        }
    }
}

#[actix_web::test]
async fn weather_endpoints() {
    if skip_live() {
        return;
    }
    let state = make_state().await;

    let (status, json) = call(&state, "/weather/geocode/search?q=Dar%20es%20Salaam&count=3").await;
    assert_ok_or_upstream(&json, status, "geocode");

    let (status, json) = call(&state, "/weather/current?latitude=-6.79&longitude=39.21").await;
    assert_ok_or_upstream(&json, status, "current");

    let (status, json) = call(&state, "/weather/current?latitude=999&longitude=0").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(is_our_error(&json));
}

#[actix_web::test]
async fn postal_endpoints() {
    if skip_live() {
        return;
    }
    let state = make_state().await;

    let (status, json) = call(&state, "/postal-code/validate/US/90210").await;
    assert_eq!(status, StatusCode::OK); // countryInfo-based, cheap
    assert_eq!(json["data"]["country"], "US");

    let (status, json) = call(&state, "/postal-code/us/33162").await;
    assert_ok_or_upstream(&json, status, "postal lookup");

    let (status, json) = call(&state, "/postal-code/countries?limit=5").await;
    assert_eq!(status, StatusCode::OK);
}

#[actix_web::test]
async fn youtube_endpoints() {
    if skip_live() {
        return;
    }
    let state = make_state().await;

    let (status, json) = call(&state, "/youtube/search?q=lofi&maxResults=3").await;
    assert_ok_or_upstream(&json, status, "search");
    if status == StatusCode::OK {
        assert!(json["data"]["contents"].as_array().unwrap().len() <= 3);
    }

    let (status, json) = call(&state, "/youtube/resolve?url=https://www.youtube.com/watch?v=dQw4w9WgXcQ").await;
    if status == StatusCode::OK {
        assert_eq!(json["data"]["type"], "video");
        assert_eq!(json["data"]["videoId"], "dQw4w9WgXcQ");
    } else {
        assert!(is_our_error(&json));
    }

    let (status, json) = call(&state, "/youtube/videos/aaaaaaaaaaaaaaaaaaaaaaaaaa").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(is_our_error(&json));
}

#[actix_web::test]
async fn books_endpoints() {
    if skip_live() {
        return;
    }
    let state = make_state().await;

    // /books/countries works offline (embedded dataset).
    let (status, json) = call(&state, "/books/countries?limit=5").await;
    assert_eq!(status, StatusCode::OK);
    assert!(json["data"]["countries"].as_array().unwrap().len() <= 5);

    let (status, json) = call(&state, "/books/books/search?q=test").await;
    assert_ok_or_upstream(&json, status, "books search");

    let (status, json) = call(&state, "/books/books/nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(is_our_error(&json));
}

#[actix_web::test]
async fn transit_endpoints() {
    if skip_live() {
        return;
    }
    let state = make_state().await;

    let (status, json) = call(&state, "/transit/agencies").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["total"], 5);

    let (status, json) = call(&state, "/transit/transport-modes").await;
    assert_eq!(status, StatusCode::OK);
    assert!(json["count"].as_u64().unwrap_or(0) >= 10);

    let (status, json) = call(&state, "/transit/agencies/unknown-agency").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(is_our_error(&json));
}
