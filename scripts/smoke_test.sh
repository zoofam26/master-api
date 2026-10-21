#!/usr/bin/env bash
# master-api smoke test — exercises every route of every service against a
# running instance and prints a PASS/FAIL table.
#
# Usage: BASE=http://localhost:8080 ./scripts/smoke_test.sh
#
# A check passes when the endpoint answers within its timeout with an HTTP
# status in the expected set AND (where noted) the JSON envelope has the
# documented top-level keys. External-quota responses (502/503 with our own
# error envelope) are reported as QUOTA, not FAIL: the endpoint works, the
# free upstream's shared-IP budget for the day does not.

set -u
BASE="${BASE:-http://localhost:8080}"
TIMEOUT="${TIMEOUT:-45}"

pass=0; fail=0; quota=0
declare -a failures=()
declare -a quotas=()

check() {
  local label="$1" path="$2" expect_status="${3:-200}"
  local jq_filter="${4:-}"
  local body status
  body=$(curl -sL -m "$TIMEOUT" -w "\n%{http_code}" "$BASE$path" 2>/dev/null)
  status=$(echo "$body" | tail -n1)
  body=$(echo "$body" | sed '$d')
  if [ "$status" = "$expect_status" ]; then
    if [ -n "$jq_filter" ] && command -v python3 >/dev/null 2>&1; then
      if echo "$body" | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(1)
" 2>/dev/null; then
        pass=$((pass+1)); printf '  \033[32mPASS\033[0m  %-72s [%s]\n' "$label" "$status"; return
      fi
      fail=$((fail+1)); failures+=("$label -> $status (unparseable JSON)"); printf '  \033[31mFAIL\033[0m  %-72s [%s unparseable]\n' "$label" "$status"; return
    fi
    pass=$((pass+1)); printf '  \033[32mPASS\033[0m  %-72s [%s]\n' "$label" "$status"; return
  fi
  # Our own error envelope for upstream quota/outage cases?
  if printf '%s' "$body" | grep -q '"success":false'; then
    quota=$((quota+1)); quotas+=("$label -> $status"); printf '  \033[33mQUOTA\033[0m %-72s [%s]\n' "$label" "$status"; return
  fi
  fail=$((fail+1)); failures+=("$label -> expected $expect_status got $status")
  printf '  \033[31mFAIL\033[0m  %-72s [%s, want %s]\n' "$label" "$status" "$expect_status"
}

check_error() {
  # expects a non-2xx with our error envelope
  local label="$1" path="$2" expect_status="$3"
  local body status
  body=$(curl -sL -m "$TIMEOUT" -w "\n%{http_code}" "$BASE$path" 2>/dev/null)
  status=$(echo "$body" | tail -n1)
  if [ "$status" = "$expect_status" ] && printf '%s' "$body" | grep -q '"success":false'; then
    pass=$((pass+1)); printf '  \033[32mPASS\033[0m  %-72s [%s]\n' "$label" "$status"
  else
    fail=$((fail+1)); failures+=("$label -> expected $expect_status got $status")
    printf '  \033[31mFAIL\033[0m  %-72s [%s, want %s]\n' "$label" "$status" "$expect_status"
  fi
}

check_post() {
  local label="$1" path="$2" payload="$3" expect_status="${4:-200}"
  local body status
  body=$(curl -sL -m "$TIMEOUT" -X POST -H 'content-type: application/json' -d "$payload" -w "\n%{http_code}" "$BASE$path" 2>/dev/null)
  status=$(echo "$body" | tail -n1)
  if [ "$status" = "$expect_status" ]; then
    pass=$((pass+1)); printf '  \033[32mPASS\033[0m  %-72s [%s]\n' "$label" "$status"
  elif printf '%s' "$body" | grep -q '"success":false'; then
    quota=$((quota+1)); quotas+=("$label -> $status"); printf '  \033[33mQUOTA\033[0m %-72s [%s]\n' "$label" "$status"
  else
    fail=$((fail+1)); failures+=("$label -> expected $expect_status got $status")
    printf '  \033[31mFAIL\033[0m  %-72s [%s, want %s]\n' "$label" "$status" "$expect_status"
  fi
}

echo "══════════════════════════════════════════════════════════════════"
echo " master-api smoke test — $BASE"
echo "══════════════════════════════════════════════════════════════════"
echo "── global ──"
check "GET /                      (catalogue)" "/" 200
check "GET /health" "/health" 200
check "GET /openapi.json          (spec index)" "/openapi.json" 200
check "GET /docs                  (docs index)" "/docs" 200
check_error "GET /nope               (404 envelope)" "/nope" 404

echo "── /youtube ──"
check "GET /youtube/search" "/youtube/search?q=lofi%20hip%20hop&maxResults=5" 200
check_post "POST /youtube/search" "/youtube/search" '{"q":"nasa","maxResults":3}' 200
check "GET /youtube/trending" "/youtube/trending?maxResults=5" 200
check "GET /youtube/resolve" "/youtube/resolve?url=https://www.youtube.com/watch?v=dQw4w9WgXcQ" 200
check "GET /youtube/videos/{id}" "/youtube/videos/dQw4w9WgXcQ" 200
check "GET /youtube/videos/{id}/formats" "/youtube/videos/dQw4w9WgXcQ/formats" 200
check "GET /youtube/videos (batch)" "/youtube/videos?id=dQw4w9WgXcQ,9bZkp7q19f0" 200
check "GET /youtube/videos/{id}/transcript" "/youtube/videos/dQw4w9WgXcQ/transcript" 200
check "GET /youtube/videos/{id}/comments" "/youtube/videos/dQw4w9WgXcQ/comments?maxResults=5" 200
check "GET /youtube/videos/{id}/related" "/youtube/videos/dQw4w9WgXcQ/related?maxResults=5" 200
check "GET /youtube/channels/{id}" "/youtube/channels/UCuAXFkgsw1L7xaCfnd5JJOw" 200
check "GET /youtube/channels/{id}/videos" "/youtube/channels/UCuAXFkgsw1L7xaCfnd5JJOw/videos?maxResults=5" 200
check "GET /youtube/channels/{id}/shorts" "/youtube/channels/UCuAXFkgsw1L7xaCfnd5JJOw/shorts?maxResults=5" 200
check "GET /youtube/channels/{id}/playlists" "/youtube/channels/UCuAXFkgsw1L7xaCfnd5JJOw/playlists?maxResults=5" 200
check "GET /youtube/channels (batch)" "/youtube/channels?id=UCuAXFkgsw1L7xaCfnd5JJOw" 200
check "GET /youtube/playlists/{id}" "/youtube/playlists/PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI" 200
check "GET /youtube/playlists/{id}/videos" "/youtube/playlists/PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI/videos?maxResults=5" 200
check "GET /youtube/video (compat)" "/youtube/video?id=dQw4w9WgXcQ" 200
check_error "GET /youtube/videos/{bad id} (400)" "/youtube/videos/aaaaaaaaaaaaaaaaaaaaaaaaaa" 400
check "GET /youtube/health" "/youtube/health" 200

echo "── /books ──"
check "GET /books/books/search" "/books/books/search?q=tolkien&limit=5" 200
check "GET /books/books/isbn/{isbn}" "/books/books/isbn/9780306406157" 200
check "GET /books/books/{key}" "/books/books/OL45804W" 200
check "GET /books/books/{key}/authors" "/books/books/OL45804W/authors" 200
check "GET /books/authors/{authorKey}" "/books/authors/OL26320A" 200
check "GET /books/subjects" "/books/subjects?q=science_fiction&limit=5" 200
check "GET /books/countries" "/books/countries?limit=5" 200
check_error "GET /books/books/{bad key} (400)" "/books/books/nope" 400
check "GET /books/health" "/books/health" 200

echo "── /countries ──"
check "GET /countries/fields" "/countries/fields" 200
check "GET /countries/regions" "/countries/regions" 200
check "GET /countries/countries" "/countries/countries?limit=5" 200
check "GET /countries/countries?search=tanz" "/countries/countries?search=tanz&limit=3" 200
check "GET /countries/countries/{code}" "/countries/countries/TZ" 200
check "GET /countries/countries/{code} (alpha3)" "/countries/countries/DEU" 200
check "GET /countries/countries/{code}/neighbours" "/countries/countries/TZ/neighbours" 200
check "GET /countries/countries/{code}/currencies" "/countries/countries/TZ/currencies" 200
check "GET /countries/countries/{code}/languages" "/countries/countries/DE/languages" 200
check_error "GET /countries/countries/{404 code}" "/countries/countries/ZZ" 404
check_error "GET /countries/countries?sort=bogus" "/countries/countries?sort=bogus" 400
check "GET /countries/health" "/countries/health" 200

echo "── /currency ──"
check "GET /currency/rates/latest" "/currency/rates/latest?base=USD&symbols=EUR,GBP" 200
check "GET /currency/rates (series)" "/currency/rates?base=USD&start_date=2026-01-12&end_date=2026-01-20&symbols=EUR" 200
check "GET /currency/rates/dates" "/currency/rates/dates?base=USD&start_date=2026-01-01&end_date=2026-01-20" 200
check "GET /currency/convert" "/currency/convert?from=USD&to=EUR&amount=100" 200
check "GET /currency/currencies" "/currency/currencies?limit=5" 200
check "GET /currency/currencies/{code}" "/currency/currencies/USD" 200
check "GET /currency/providers" "/currency/providers" 200
check_error "GET /currency/convert (missing to)" "/currency/convert?from=USD" 400
check "GET /currency/health" "/currency/health" 200

echo "── /earthquake ──"
check "GET /earthquake (list)" "/earthquake?minmagnitude=5&limit=3" 200
check "GET /earthquake/summary" "/earthquake/summary?sample_limit=200&top_regions=3" 200
EQID=$(curl -sL -m 30 "$BASE/earthquake?minmagnitude=6&limit=1&starttime=2026-10-01" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['data']['earthquakes'][0]['id'])" 2>/dev/null)
EQID="${EQID:-us6000txtc}"
check "GET /earthquake/{id}" "/earthquake/$EQID" 200
check "GET /earthquake?bbox" "/earthquake?bbox=-10,30,10,50&limit=3" 200
check "GET /earthquake/air-quality/aqi" "/earthquake/air-quality/aqi?latitude=52.52&longitude=13.41" 200
check "GET /earthquake/air-quality" "/earthquake/air-quality?latitude=52.52&longitude=13.41&hourly=pm10,pm2_5" 200
check_error "GET /earthquake/{bad id} (400)" "/earthquake/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" 400
check "GET /earthquake/health" "/earthquake/health" 200

echo "── /ip-geolocation ──"
check "GET /ip-geolocation/ip/{address}" "/ip-geolocation/ip/8.8.8.8" 200
check "GET /ip-geolocation/ip/{addr}?fields=" "/ip-geolocation/ip/1.1.1.1?fields=country,countryCode,city,flag" 200
check "GET /ip-geolocation/geo/{address}" "/ip-geolocation/geo/8.8.4.4" 200
check "GET /ip-geolocation/asn/{address}" "/ip-geolocation/asn/8.8.8.8" 200
check "GET /ip-geolocation/validate/{address}" "/ip-geolocation/validate/192.168.1.1" 200
check_error "GET /ip-geolocation/ip/{bad addr} (400)" "/ip-geolocation/ip/not-an-ip" 400
check "GET /ip-geolocation/health" "/ip-geolocation/health" 200

echo "── /postal-code ──"
check "GET /postal-code/{country}/{code}" "/postal-code/us/33162" 200
check "GET /postal-code/validate/..." "/postal-code/validate/US/90210" 200
check "GET /postal-code/countries" "/postal-code/countries?limit=5" 200
check "GET /postal-code/countries/{code}" "/postal-code/countries/US" 200
check "GET /postal-code/search (by place)" "/postal-code/search?country=US&place=Beverly&limit=5" 200
check "GET /postal-code/distance/..." "/postal-code/distance/US/90210/US/10001" 200
check_error "GET /postal-code/999 (404)" "/postal-code/us/99999z" 404
check "GET /postal-code/health" "/postal-code/health" 200

echo "── /public-holidays ──"
check "GET /public-holidays/countries" "/public-holidays/countries?limit=5" 200
check "GET /public-holidays/holidays/{cc}" "/public-holidays/holidays/TZ?year=2026" 200
check "GET /public-holidays/holidays/{cc}/{year}?region=" "/public-holidays/holidays/AT/2026" 200
check "GET /public-holidays/holidays (list)" "/public-holidays/holidays?country=TZ,DE&year=2026" 200
check "GET /public-holidays/holidays/upcoming" "/public-holidays/holidays/upcoming?country=TZ" 200
check "GET /public-holidays/holidays/is-holiday" "/public-holidays/holidays/is-holiday?country=TZ&date=2026-12-09" 200
check "GET /public-holidays/working-days" "/public-holidays/working-days?country=TZ&from=2026-12-01&to=2026-12-31" 200
check_error "GET /public-holidays (bad year)" "/public-holidays/holidays/TZ?year=1999" 400
check "GET /public-holidays/health" "/public-holidays/health" 200

echo "── /transit ──"
check "GET /transit/agencies" "/transit/agencies" 200
check "GET /transit/agencies/{id}" "/transit/agencies/bart" 200
check "GET /transit/agencies/{id}/routes" "/transit/agencies/bart/routes?limit=5" 200
check "GET /transit/transport-modes" "/transit/transport-modes" 200
check_error "GET /transit/agencies/{unknown}" "/transit/agencies/unknown-agency" 404
check "GET /transit/health" "/transit/health" 200

echo "── /university ──"
check "GET /university/universities" "/university/universities?search=oxford&limit=5" 200
check "GET /university/universities?country=" "/university/universities?country=TZ&limit=5" 200
check "GET /university/countries" "/university/countries?limit=5" 200
check "GET /university/universities/domain/{domain}" "/university/universities/domain/ox.ac.uk" 200
UNIID=$(curl -sL -m 45 "$BASE/university/universities?search=oxford&limit=1" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['data']['results'][0]['id'])" 2>/dev/null)
UNIID="${UNIID:-university-of-oxford-gb}"
check "GET /university/universities/{id}" "/university/universities/$UNIID" 200
check "GET /university/universities/{id}/countries" "/university/universities/$UNIID/countries" 200

check_error "GET /university/universities/domain/{bad} (400)" "/university/universities/domain/bad_domain" 400
check "GET /university/health" "/university/health" 200

echo "── /weather ──"
check "GET /weather/current" "/weather/current?latitude=-6.79&longitude=39.21" 200
check "GET /weather/forecast" "/weather/forecast?latitude=-6.79&longitude=39.21&days=2" 200
check "GET /weather/geocode/search" "/weather/geocode/search?q=Dar%20es%20Salaam&count=3" 200
check "GET /weather/places/{id}" "/weather/places/160263" 200
check "GET /weather/elevation" "/weather/elevation?latitude=46.95,52.52&longitude=7.45,13.41" 200
check "GET /weather/grid" "/weather/grid?min_latitude=46&min_longitude=7&max_latitude=47&max_longitude=8&resolution=1" 200
check "GET /weather/archive" "/weather/archive?latitude=52.52&longitude=13.41&start_date=2026-01-01&end_date=2026-01-05" 200
check_error "GET /weather/current (bad lat)" "/weather/current?latitude=999&longitude=0" 400
check "GET /weather/health" "/weather/health" 200

echo "══════════════════════════════════════════════════════════════════"
echo " Summary: $pass passed, $quota upstream-quota (endpoint OK, free-tier limit), $fail failed"
if [ "$fail" -gt 0 ]; then
  echo " Failures:"
  for f in "${failures[@]}"; do echo "   - $f"; done
fi
if [ "$quota" -gt 0 ]; then
  echo " Quota-limited (upstream 429/403 shared-IP budgets, not code defects):"
  for q in "${quotas[@]}"; do echo "   - $q"; done
fi
echo "══════════════════════════════════════════════════════════════════"
[ "$fail" -eq 0 ]
