//! weather service — Open-Meteo forecast, archive, geocoding, elevation and a
//! derived grid endpoint. Ported from `zoofam26/weather-api` (NestJS).

mod routes;

use actix_web::web;
use serde_json::json;
use serde_json::Value;
use utoipa::openapi::OpenApi;

pub const SERVICE_PREFIX: &str = "/weather";
pub const CODE_PREFIX: &str = "WEATHER";
pub const UPSTREAM_OPEN_METEO: &str = "https://api.open-meteo.com";
pub const UPSTREAM_OPEN_METEO_ARCHIVE: &str = "https://archive-api.open-meteo.com";
pub const UPSTREAM_OPEN_METEO_GEOCODING: &str = "https://geocoding-api.open-meteo.com";

pub const FORECAST_PATH: &str = "/v1/forecast";
pub const ELEVATION_PATH: &str = "/v1/elevation";
pub const ARCHIVE_PATH: &str = "/v1/archive";
pub const GEOCODING_SEARCH_PATH: &str = "/v1/search";
pub const GEOCODING_GET_PATH: &str = "/v1/get";

pub const UNIT_SYSTEMS: &[&str] = &["standard", "metric", "imperial"];
pub const GEOCODE_SORTS: &[&str] = &["relevance", "population", "name"];

pub const CURRENT_VARIABLES: &[&str] = &[
    "temperature_2m", "relative_humidity_2m", "apparent_temperature", "is_day",
    "precipitation", "rain", "showers", "snowfall", "weather_code", "cloud_cover",
    "pressure_msl", "surface_pressure", "wind_speed_10m", "wind_direction_10m", "wind_gusts_10m",
];

pub const FORECAST_HOURLY_VARIABLES: &[&str] = &[
    "temperature_2m", "relative_humidity_2m", "dew_point_2m", "apparent_temperature",
    "precipitation_probability", "precipitation", "rain", "showers", "snowfall", "snow_depth",
    "weather_code", "pressure_msl", "surface_pressure", "cloud_cover", "cloud_cover_low",
    "cloud_cover_mid", "cloud_cover_high", "visibility", "evapotranspiration",
    "et0_fao_evapotranspiration", "vapour_pressure_deficit", "cape", "wind_speed_10m",
    "wind_speed_80m", "wind_speed_120m", "wind_speed_180m", "wind_direction_10m",
    "wind_direction_80m", "wind_direction_120m", "wind_direction_180m", "wind_gusts_10m",
    "temperature_80m", "temperature_120m", "temperature_180m", "soil_temperature_0cm",
    "soil_temperature_6cm", "soil_temperature_18cm", "soil_temperature_54cm",
    "soil_moisture_0_to_1cm", "soil_moisture_1_to_3cm", "soil_moisture_3_to_9cm",
    "soil_moisture_9_to_27cm", "soil_moisture_27_to_81cm", "uv_index", "uv_index_clear_sky",
    "sunshine_duration", "wet_bulb_temperature_2m", "total_column_integrated_water_vapour",
    "lifted_index", "convective_inhibition", "freezing_level_height", "boundary_layer_height",
    "shortwave_radiation", "direct_radiation", "diffuse_radiation", "direct_normal_irradiance",
    "global_tilted_irradiance", "terrestrial_solar_radiation",
];

pub const FORECAST_DAILY_VARIABLES: &[&str] = &[
    "weather_code", "temperature_2m_max", "temperature_2m_mean", "temperature_2m_min",
    "apparent_temperature_max", "apparent_temperature_mean", "apparent_temperature_min",
    "uv_index_max", "uv_index_clear_sky_max", "sunrise", "sunset", "daylight_duration",
    "sunshine_duration", "moonrise", "moonset", "moon_phase", "rain_sum", "showers_sum",
    "snowfall_sum", "precipitation_sum", "precipitation_hours", "precipitation_probability_max",
    "wind_speed_10m_max", "wind_gusts_10m_max", "wind_direction_10m_dominant",
    "shortwave_radiation_sum", "et0_fao_evapotranspiration",
];

pub const ARCHIVE_HOURLY_VARIABLES: &[&str] = &[
    "temperature_2m", "relative_humidity_2m", "dew_point_2m", "apparent_temperature",
    "pressure_msl", "surface_pressure", "precipitation", "rain", "snowfall", "snow_depth",
    "weather_code", "cloud_cover", "cloud_cover_low", "cloud_cover_mid", "cloud_cover_high",
    "visibility", "vapour_pressure_deficit", "wind_speed_10m", "wind_speed_100m",
    "wind_direction_10m", "wind_direction_100m", "wind_gusts_10m", "et0_fao_evapotranspiration",
    "shortwave_radiation", "direct_radiation", "diffuse_radiation", "direct_normal_irradiance",
    "global_tilted_irradiance", "sunshine_duration", "soil_temperature_0_to_7cm",
    "soil_temperature_7_to_28cm", "soil_temperature_28_to_100cm", "soil_temperature_100_to_255cm",
    "soil_moisture_0_to_7cm", "soil_moisture_7_to_28cm", "soil_moisture_28_to_100cm",
    "soil_moisture_100_to_255cm",
];

pub const ARCHIVE_DAILY_VARIABLES: &[&str] = &[
    "weather_code", "temperature_2m_max", "temperature_2m_min", "apparent_temperature_max",
    "apparent_temperature_min", "sunrise", "sunset", "sunshine_duration", "precipitation_sum",
    "rain_sum", "snowfall_sum", "precipitation_hours", "wind_speed_10m_max", "wind_gusts_10m_max",
    "wind_direction_10m_dominant", "shortwave_radiation_sum", "et0_fao_evapotranspiration",
];

/// Unit metadata returned alongside every weather payload.
pub fn unit_meta(system: Option<&str>) -> Value {
    match system {
        Some("metric") => json!({
            "system": "metric", "temperature": "°C", "wind_speed": "m/s",
            "precipitation": "mm", "distance": "m",
        }),
        Some("imperial") => json!({
            "system": "imperial", "temperature": "°F", "wind_speed": "mph",
            "precipitation": "inch", "distance": "m",
        }),
        _ => json!({
            "system": "standard", "temperature": "°C", "wind_speed": "km/h",
            "precipitation": "mm", "distance": "m",
        }),
    }
}

fn unit_params(system: &str) -> [(&'static str, &str); 3] {
    match system {
        "metric" => [
            ("temperature_unit", "celsius"),
            ("wind_speed_unit", "ms"),
            ("precipitation_unit", "mm"),
        ],
        "imperial" => [
            ("temperature_unit", "fahrenheit"),
            ("wind_speed_unit", "mph"),
            ("precipitation_unit", "inch"),
        ],
        _ => [
            ("temperature_unit", "celsius"),
            ("wind_speed_unit", "kmh"),
            ("precipitation_unit", "mm"),
        ],
    }
}

/// WMO weather-code table → condition (description + icon name).
pub fn wmo_condition(code: Option<i64>) -> Value {
    let code = code.unwrap_or(-1);
    let (description, icon) = match code {
        0 => ("Clear sky", "clear-sky"),
        1 => ("Mainly clear", "mainly-clear"),
        2 => ("Partly cloudy", "partly-cloudy"),
        3 => ("Overcast", "overcast"),
        45 => ("Fog", "fog"),
        48 => ("Depositing rime fog", "rime-fog"),
        51 => ("Light drizzle", "light-drizzle"),
        53 => ("Moderate drizzle", "moderate-drizzle"),
        55 => ("Dense drizzle", "dense-drizzle"),
        56 => ("Light freezing drizzle", "light-freezing-drizzle"),
        57 => ("Dense freezing drizzle", "dense-freezing-drizzle"),
        61 => ("Slight rain", "slight-rain"),
        63 => ("Moderate rain", "moderate-rain"),
        65 => ("Heavy rain", "heavy-rain"),
        66 => ("Light freezing rain", "light-freezing-rain"),
        67 => ("Heavy freezing rain", "heavy-freezing-rain"),
        71 => ("Slight snowfall", "slight-snowfall"),
        73 => ("Moderate snowfall", "moderate-snowfall"),
        75 => ("Heavy snowfall", "heavy-snowfall"),
        77 => ("Snow grains", "snow-grains"),
        80 => ("Slight rain showers", "slight-rain-showers"),
        81 => ("Moderate rain showers", "moderate-rain-showers"),
        82 => ("Violent rain showers", "violent-rain-showers"),
        85 => ("Slight snow showers", "slight-snow-showers"),
        86 => ("Heavy snow showers", "heavy-snow-showers"),
        95 => ("Thunderstorm", "thunderstorm"),
        96 => ("Thunderstorm with slight hail", "thunderstorm-hail"),
        99 => ("Thunderstorm with heavy hail", "thunderstorm-heavy-hail"),
        _ => ("Unknown", "unknown"),
    };
    json!({ "code": if code >= 0 { json!(code) } else { Value::Null }, "description": description, "icon": icon })
}

pub fn router(cfg: &mut web::ServiceConfig) {
    routes::configure(cfg);
}

pub fn openapi() -> OpenApi {
    routes::openapi_doc()
}

pub fn index() -> serde_json::Value {
    serde_json::json!({
        "name": "weather-api",
        "prefix": SERVICE_PREFIX,
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Weather forecasts, historical archive, place geocoding, elevation lookups and a derived multi-point grid over the Open-Meteo family of APIs.",
        "dataSource": {
            "provider": "Open-Meteo",
            "upstream": [UPSTREAM_OPEN_METEO, UPSTREAM_OPEN_METEO_ARCHIVE, UPSTREAM_OPEN_METEO_GEOCODING],
            "requiresApiKey": false,
            "note": "Responses are cached in-process; unit systems (standard/metric/imperial) are applied upstream.",
        },
        "endpoints": [
            { "method": "GET", "path": "/weather/current", "description": "Current conditions for a point" },
            { "method": "GET", "path": "/weather/forecast", "description": "Hourly/daily forecast for a point" },
            { "method": "GET", "path": "/weather/geocode/search", "description": "Place search with sorting and pagination" },
            { "method": "GET", "path": "/weather/places/{id}", "description": "One place by GeoNames id" },
            { "method": "GET", "path": "/weather/elevation", "description": "Elevation for one or more points" },
            { "method": "GET", "path": "/weather/grid", "description": "Current conditions across a bbox grid" },
            { "method": "GET", "path": "/weather/archive", "description": "ERA5/IFS reanalysis archive" },
            { "method": "GET", "path": "/weather/health", "description": "Liveness plus a real upstream probe" },
            { "method": "GET", "path": "/weather/health/ready", "description": "Readiness probe" },
            { "method": "GET", "path": "/weather/openapi.json", "description": "OpenAPI 3 document for this service" },
        ],
    })
}
