# master-api

**Eleven microservice APIs, rewritten from NestJS into one Rust binary.**

A unified backend that consolidates eleven independently deployed NestJS
services — youtube, books, countries, currency, earthquake, ip-geolocation,
postal-code, public-holidays, transit, university, weather — into a single
[Actix Web](https://actix.rs) workspace. Same routes, same response envelopes,
same error bodies the original services produced; one process, one port, one
deployment.

```
Before: 11 × Node processes, 11 ports, 11 pipelines, 11 dashboards
After:   1 × Rust binary,   1 port,  1 pipeline,  1 dashboard
```

| Service | Prefix | Upstream(s) | Key |
|---|---|---|---|
| YouTube | `/youtube` | YouTube InnerTube (keyless) + Data API v3 | optional `YOUTUBE_API_KEY` |
| Books | `/books` | Open Library | optional `GOOGLE_BOOKS_API_KEY` |
| Countries | `/countries` | embedded mledoze/countries dataset (250 countries, zero upstream calls) | — |
| Currency | `/currency` | Frankfurter (ECB) | — |
| Earthquake | `/earthquake` | USGS FDSN + Open-Meteo air quality | — |
| IP geolocation | `/ip-geolocation` | ipwho.is (+ ipinfo fallback) | — |
| Postal codes | `/postal-code` | GeoNames ZIP datasets (150+ countries) | — |
| Public holidays | `/public-holidays` | Nager.Date v4 | — |
| Transit | `/transit` | GTFS static feeds (MTA, Sound Transit, BART, Monorail, SNCF) | — |
| Universities | `/university` | Hipo labs universities dataset | — |
| Weather | `/weather` | Open-Meteo suite (forecast/current/air-quality/elevation/geocode/archive) | — |

Every service also answers `GET /{service}/health` and
`GET /{service}/health/ready`, and publishes its own OpenAPI document.

## Quick start

```bash
git clone https://github.com/zoofam26/master-api.git
cd master-api
cp .env.example .env          # optional: keys & tuning (defaults work)
cargo run --release           # http://localhost:8080
```

or with Docker:

```bash
docker compose up -d          # builds & serves on :8080
curl localhost:8080/health
```

### Try it

```bash
curl 'localhost:8080/youtube/search?q=lofi+beats&maxResults=5'
curl 'localhost:8080/countries/CH?fields=name,capital,currencies'
curl 'localhost:8080/currency/convert?from=USD&to=EUR&amount=100'
curl 'localhost:8080/earthquake/summary?minMagnitude=6'
curl 'localhost:8080/postal-code/us/90210'
curl 'localhost:8080/public-holidays/holidays/upcoming?countryCode=DE'
curl 'localhost:8080/transit/agencies'
curl 'localhost:8080/university/universities?country=kenya&limit=5'
curl 'localhost:8080/weather/current?latitude=52.52&longitude=13.41'
```

## Documentation

Every service ships an independent OpenAPI 3 document plus Swagger UI:

| OpenAPI JSON | Swagger UI |
|---|---|
| `/{service}/openapi.json` | `/docs/{service}` — e.g. `/docs/youtube` |

Aggregated: `/openapi.json` (catalogue of all services), `/docs` (docs index),
`/` (service catalogue JSON), `/health` (liveness), `/health/ready` (readiness
with upstream probes).

## Response contract

Success (all services):

```json
{
  "success": true,
  "data": { "...": "service payload" },
  "meta": { "durationMs": 182, "upstream": "frankfurter", "cached": false }
}
```

Failure (shape produced by the original NestJS `AllExceptionsFilter`, kept
byte-compatible):

```json
{
  "success": false,
  "error": {
    "code": "YOUTUBE_UPSTREAM",
    "message": "Upstream provider error (HTTP 503). ...",
    "details": { "upstreamStatus": 503 }
  },
  "path": "/youtube/search?q=lofi",
  "timestamp": "2026-10-21T09:00:00.123Z"
}
```

Error codes are namespaced per service (`<SERVICE>_<REASON>`), mapped from
upstream HTTP statuses: 404 → `*_NOT_FOUND`, 429 → `*_RATE_LIMITED`,
401/403 → `*_FORBIDDEN`, 408 → `*_UPSTREAM_TIMEOUT`, 5xx → `*_UPSTREAM`.

## Architecture

```
crates/
├── common/            master-common: error model, envelope, settings, AppState,
│                      resilient HTTP client (timeout/retry/jitter), caches
├── youtube/           InnerTube engine (WEB/ANDROID/IOS client profiles),
│                      protobuf search-filter builder, transcript/formats
├── books/             Open Library client, ISBN-10/13 validation & bridging
├── countries/         embedded dataset + lazy GeoNames join + field projection
├── currency/          Frankfurter v2 client, date series w/ carried-forward rates
├── earthquake/        USGS FDSN queries + summary analytics + air quality
├── ip_geolocation/    multi-provider lookup, ASN, validation, field projection
├── postal_code/       GeoNames ZIP dataset downloads, in-memory indexing,
│                      radius search, haversine distance
├── public_holidays/   Nager client, multi-country fan-out, working days
├── transit/           GTFS static feed registry, service-day resolution,
│                      departures, nearby stops
├── university/        Hipo dataset snapshot with slug/id indexes
├── weather/           Open-Meteo suite + WMO code mapping + unit systems
└── app/               the master-api binary: router assembly, CORS, rate
                       limiting, logging, Swagger UI, integration tests
```

Full design rationale: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

### Engineering highlights

- **Cargo workspace** — one shared dependency graph, one `cargo test` for
  everything; services are plain libraries, the binary is 30 lines of wiring.
- **Resilient upstreams** — every outbound call has a timeout, bounded retries
  with exponential backoff + jitter, and status-to-error mapping.
- **Single-flight caching** — per-service TTL caches (moka) collapse concurrent
  identical upstream calls into one; concurrent misses share the fetch.
- **Graceful degradation** — optional keys, optional upstreams, degraded
  dataset joins; a dead dependency degrades one endpoint, never the process.
- **Observability** — `tracing` structured logs (human or JSON), per-request
  logging middleware with duration, health/readiness endpoints, error codes
  safe to alert on.
- **Rate limiting** — per-IP token bucket (actix-governor), configurable,
  0 disables.
- **Zero system OpenSSL** — reqwest with rustls; the release binary drops into
  any glibc container.
- **Release profile** — `lto = "thin"`, `codegen-units = 1`, stripped binary,
  ~15 MB runtime footprint, single-digit-megabyte RSS at idle.

### Scalability model

The process is **stateless**: all mutable state lives in bounded in-process
caches, everything else is env-config. That gives three scaling axes:

1. **Vertical** — actix-web's worker-per-core model scales with CPU; cache
   capacity is the only knob that trades memory for upstream QPS.
2. **Horizontal** — run N replicas behind any load balancer (no sticky
   sessions, no shared store); compose file + healthcheck included, and the
   readiness endpoint keeps rolling deploys safe.
3. **Isolation** — each service crate is independently reusable; if one
   service's traffic spikes, it can be split out of the workspace and deployed
   on its own port with no code change (the router is a registration list).

Upstream pressure is bounded by design: TTL caches with per-service overrides
(`CACHE_TTL_*`), dataset snapshots with 6-hour TTLs, and bounded fan-out
requests (e.g. holidays fetches at most 6 countries per call).

## Configuration

Everything is environment-driven (12-factor); see
[`.env.example`](.env.example) for the full catalogue. Notables:

| Variable | Default | Meaning |
|---|---|---|
| `APP_PORT` | `8080` | bind port |
| `CORS_ORIGIN` | `*` | allowed origins |
| `RATE_LIMIT_PER_MINUTE` | `600` | per-IP client limit, 0 disables |
| `UPSTREAM_TIMEOUT_SECONDS` | `15` | per-upstream-call timeout |
| `CACHE_TTL_SECONDS` | `300` | default response cache TTL |
| `CACHE_TTL_<NAME>` | — | per-service TTL override |
| `UPSTREAM_<ALIAS>` | — | point an upstream at a proxy/fixture |
| `YOUTUBE_API_KEY` | — | enables Data API v3 mode for `/youtube` |
| `GOOGLE_BOOKS_API_KEY` | — | lifts anonymous quota for `/books` |
| `JSON_LOGS` | `false` | structured JSON logging |

## Development

```bash
cargo test --workspace          # 85 unit tests + live integration suite
SKIP_LIVE=1 cargo test          # offline-safe subset
BASE=http://localhost:8080 ./scripts/smoke_test.sh   # every route, PASS/FAIL table
```

The smoke script exercises every endpoint of every service against a running
instance; upstream free-quota answers (502/503 with our error envelope) are
reported as `QUOTA`, not failures.

## License

[MIT](LICENSE)
