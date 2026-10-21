# master-api — Architecture

This document explains how eleven NestJS microservices were consolidated into
one Rust/Actix-Web workspace, what was preserved from the originals, and how
the result is engineered for reliability and scale.

## 1. Goals & non-goals

**Goals**

1. Preserve the public contract: routes, query parameters, response envelopes
   and error bodies stay compatible so existing clients can migrate by
   changing the base URL alone.
2. One deployable unit: single binary, single port, single pipeline.
3. Zero-required-configuration: the service runs with no keys, no databases,
   no external infrastructure; keys and tuning are additive env vars.
4. Every upstream call is resilient: timeout, bounded retry with backoff,
   caching, and clean error mapping.
5. Per-service OpenAPI documents and Swagger UI, generated from the same code
   that serves the routes.

**Non-goals**

- Not a gateway: master-api does not proxy arbitrary requests; each service
  has a typed, validated surface.
- No persistent storage: everything is either upstream-derived and cached, or
  embedded and static. There is nothing to back up.

## 2. Workspace layout

```
Cargo workspace
├── crates/common        master-common — the shared kernel (no service logic)
├── crates/<service> ×11 svc-<name> — pure libraries: routes + upstream clients
└── crates/app           master-api — binary: wiring, middleware, docs, tests
```

The split is deliberate:

- **common** contains cross-cutting infrastructure only (error model, settings,
  HTTP client, caches, middleware). It never imports a service.
- **each service crate** is independently buildable and testable; its `router`
  and `openapi` functions are the only integration surface. A service can be
  extracted into its own binary by writing a 20-line `main.rs` — useful if one
  service ever needs isolated scaling.
- **app** is a registration list: `SERVICES: &[ServiceRegistration]` maps a
  prefix to `{router, openapi, index}`. Adding a twelfth service is one
  manifest entry, one dependency, one line in the list.

## 3. Request lifecycle

```
client
  │
  ▼
[actix-governor]        per-IP token bucket (RATE_LIMIT_PER_MINUTE, 0=off)
[actix-cors]            CORS_ORIGIN, preflight handling
[RequestLogging]        method, path, status, duration µs
[ErrorEnvelope]         injects `path` into error bodies
[Compress]              gzip/br negotiation
  │
  ▼
{service}::router       /{prefix}/**  → handler(state) → ApiResult<Json<Value>>
  │
  ├─ cache hit ─────────► Envelope::ok(data, cached=true)
  ▼
httpx::get_json         timeout + retry/backoff + status mapping
  │
  ▼
upstream provider
```

Handlers are `async fn(web::Data<AppState>, …) -> Result<Json<Value>, ApiError>`.
`?` on any `ApiError` flows into the error middleware; the success path always
wraps through `Envelope::ok`.

## 4. The common kernel

| Module | Responsibility |
|---|---|
| `error.rs` | `ApiError` enum + `ResponseError` impl producing the exact original error body `{success:false, error:{code,message,details?}, path, timestamp}`; `from_upstream_status` maps upstream HTTP codes to namespaced codes |
| `envelope.rs` | `Envelope {success, data, meta}` and `meta!` builder |
| `settings.rs` | env-driven `Settings` with defaults for every knob; `CACHE_TTL_*` and `UPSTREAM_*` namespaces parsed generically |
| `state.rs` | `AppState {settings, http, caches}` — cheap to clone, shared across workers; named `moka` caches with **single-flight** (`try_get_with` collapses concurrent identical misses into one upstream call) |
| `httpx.rs` | `get_json` / `get_json_opt` / `post_json` / `get_bytes`: timeout, retry with exponential backoff + jitter (only for idempotent failures), timing, structured logs, `cached_json` convenience |
| `middleware.rs` | request logging + error `path` injection |

## 5. Service design patterns

Eleven services, four recurring shapes:

**A. Embedded dataset** (`countries`, `university`, postal reference tables) —
the dataset is compiled in or snapshotted once, indexed in memory
(hash/btree), served with O(1) lookups. No upstream dependency on the hot
path. Refreshes happen in the background via a lazy TTL snapshot; a failed
refresh keeps serving the previous snapshot (**stale-while-error**).

**B. Snapshot datasets over HTTP** (`postal-code` ZIP datasets, `transit`
GTFS feeds) — downloads are cached as parsed in-memory structures with
6-hour TTL, bounded parse sizes, and per-feed isolation: one broken feed
degrades one agency, not the service.

**C. Live upstream proxies** (`currency`, `public-holidays`, `earthquake`,
`weather`, `ip-geolocation`, `books`) — typed clients for one primary
upstream, response shaping/filtering server-side, TTL caching with
per-service overrides, and quota-aware error mapping (429 → descriptive
message, never a silent retry storm).

**D. Reverse-engineered upstream** (`youtube`) — the InnerTube engine speaks
YouTube's private web API with three client profiles (WEB/ANDROID/IOS) and
per-endpoint fallback chains, a protobuf search-filter builder (hand-rolled
varint/base64 encoder, matching the original's algorithm), recursive renderer
parsers that tolerate YouTube's frequent layout changes, and an optional Data
API v3 mode when `YOUTUBE_API_KEY` is set.

## 6. Reliability engineering

- **Timeouts everywhere**: client→master-api is actix's default; every
  master-api→upstream call carries `UPSTREAM_TIMEOUT_SECONDS` (15s default).
- **Bounded retries**: idempotent GETs retry `UPSTREAM_MAX_RETRIES` times with
  exponential backoff + jitter; 4xx never retries.
- **Error mapping**: upstream 404/429/401/403/408/5xx become namespaced,
  client-safe error codes; internal errors never leak internals.
- **Graceful degradation**: optional upstreams (GeoNames join, ipinfo
  fallback, open-meteo fallbacks) fail soft; per-service health endpoints
  distinguish "I'm up" (`/health`) from "I can do my job" (`/health/ready`).
- **Cache discipline**: every upstream response is cached under a
  deterministic key; TTLs are per-service (`CACHE_TTL_<NAME>`) so a twitchy
  upstream (weather) can be tuned without touching calm ones (countries).
- **Single-flight**: concurrent identical misses share one upstream call —
  protects free-tier quotas under burst load.

## 7. Scalability

**Stateless by construction.** No database, no sessions, no local files at
runtime. Any replica can serve any request, which makes the scaling story:

- **Scale up**: actix uses worker-per-core; throughput scales with CPU count.
  Memory scales with cache capacity (`CACHE_MAX_CAPACITY`), which is the only
  tunable trade of RAM for upstream QPS.
- **Scale out**: put N replicas behind a load balancer. No sticky sessions.
  `/health/ready` gates rolling deploys; the Docker healthcheck uses it.
- **Scale apart**: any service can be lifted into its own binary (the crate is
  already a library) if its traffic profile diverges — e.g. run the
  InnerTube-heavy YouTube service on dedicated hardware while the rest share a
  small instance.

**Upstream pressure** is the real constraint for a public-API aggregator, and
it is addressed at three layers: request collapsing (single-flight), TTL
caching (per-service), and bounded fan-out (multi-entity endpoints cap the
number of upstream calls they make per request, e.g. holidays ≤ 6 countries,
weather grid ≤ 100 points).

## 8. Observability

- `tracing` spans per request via `RequestLogging` (method, path, status,
  duration); `JSON_LOGS=1` switches the subscriber to JSON for log shippers.
- Error codes are a closed vocabulary (`<SERVICE>_<REASON>`) — dashboards can
  alert on `*_RATE_LIMITED` spikes without regex-parsing messages.
- `/health` (liveness), `/health/ready` (readiness incl. upstream probes),
  per-service `/{svc}/health{,/ready}`, `/` catalogue with per-service
  endpoint counts, `/openapi.json` machine-readable catalogue.

## 9. Testing strategy

| Layer | Suite | Gate |
|---|---|---|
| Unit (per crate) | parsers, validators, encoders, slugs, ISBN math, GTFS service-day resolution, cache behaviour | `cargo test --workspace` — 85 tests, must pass everywhere incl. CI |
| Integration (app) | full router in-process against real upstreams; tolerant of free-tier quota (asserts our error envelope on 5xx) | `cargo test -p master-api` (set `SKIP_LIVE=1` for offline) |
| Smoke (ops) | every route of every service against a running binary, PASS/FAIL/QUOTA table | `scripts/smoke_test.sh` |

## 10. Security posture

- No secrets in code: keys come from env only; `.env` is git-ignored.
- Unprivileged container user (`masterapi`, uid 10001), read-only runtime
  image inputs, minimal runtime packages (ca-certificates + curl for the
  healthcheck).
- rustls instead of dynamic OpenSSL — no system TLS library to patch.
- Strict input validation per service (parameter whitelists, length caps,
  pattern re-anchoring on postal regexes, bounded sizes on downloads/parses).
- Rate limiting on by default (600 req/min/IP) to protect the free upstreams
  behind the service.

## 11. Migration mapping (NestJS → Rust)

| NestJS | master-api |
|---|---|
| `@Controller('v1')` etc. | `/{service}/...` scopes; youtube keeps `/youtube/v1/**` compat aliases |
| DTO + ValidationPipe | per-service query structs with explicit whitelists & caps |
| `AllExceptionsFilter` | `ApiError` + `ErrorEnvelope` middleware (same body) |
| `TtlCache` util | `AppState` named moka caches, single-flight |
| `@nestjs/throttler` | actix-governor per-IP bucket |
| Swagger plugin (`@ApiTags`) | utoipa `#[utoipa::path]` annotations, one `OpenApi` per service |
| 11 × PM2/systemd units | 1 × systemd unit / 1 × container |
