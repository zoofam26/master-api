//! Holiday domain model, upstream parsing and the date helpers every route
//! needs. Ports `holidays/holidays.types.ts`, `parsers/holiday.parser.ts` and
//! `parsers/date.utils.ts`.
//!
//! The [`Holiday`] shape is deliberately NOT the upstream Nager payload: an
//! upstream schema change is contained in [`parse_holiday`], never in the
//! response contract.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use master_common::error::ApiError;

use crate::CODE_PREFIX;

/// The only `holidayTypes` values Nager was observed to emit, established
/// empirically across 1428 real responses (204 countries x 7 supported years).
pub const HOLIDAY_TYPES: &[&str] =
    &["Public", "Bank", "School", "Authorities", "Optional", "Observance"];

/// Sort keys accepted by the collection routes (`-` prefix = descending).
pub const HOLIDAY_SORTS: &[&str] = &["date", "-date", "country", "-country", "name", "-name"];

/// Sort keys accepted by the subdivision route (no cross-country listing, so
/// no `country` key).
pub const REGION_SORTS: &[&str] = &["date", "-date", "name", "-name"];

/// One holiday, normalised into this service's vocabulary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Holiday {
    /// ISO 8601 calendar date, `YYYY-MM-DD`.
    pub date: String,
    /// English name. Nager publishes no localized names.
    pub name: String,
    pub country_code: String,
    /// `national` (applies everywhere) or `subdivision` (region-scoped).
    pub scope: String,
    /// ISO 3166-2 codes this holiday applies to; empty for national holidays.
    pub subdivision_codes: Vec<String>,
    pub types: Vec<String>,
    /// Day of week, derived. `0` = Sunday.
    pub day_of_week: u32,
    /// True when Saturday or Sunday.
    pub is_weekend: bool,
}

/// One supported country.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Country {
    pub country_code: String,
    pub name: String,
    /// Number of ISO 3166-2 subdivisions Nager publishes holiday data for.
    /// `null` unless `include=regionCounts` was requested.
    pub region_count: Option<u64>,
}

fn parse_error(message: impl Into<String>) -> ApiError {
    ApiError::upstream(format!("{CODE_PREFIX}_PARSE"), message)
}

// -------------------------------------------------------------- date utils

/// `YYYY-MM-DD` shape only.
pub fn has_iso_shape(value: &str) -> bool {
    let bytes = value.as_bytes();
    value.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
}

pub fn parse_date(value: &str) -> Option<chrono::NaiveDate> {
    if !has_iso_shape(value) {
        return None;
    }
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()
}

/// A `YYYY-MM-DD` string is not enough to make a date: `2026-02-30` is
/// accepted by naive parsers and silently rolls over to 2 March. This rejects it.
pub fn is_real_iso_date(value: &str) -> bool {
    parse_date(value).is_some()
}

/// `AT` / `at` / ` At ` → `AT`. `None` when not two ASCII letters.
pub fn normalise_country_code(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.len() == 2 && trimmed.bytes().all(|b| b.is_ascii_alphabetic()) {
        Some(trimmed.to_uppercase())
    } else {
        None
    }
}

/// `at-2` / `AT-2 ` → `AT-2`. `None` for anything not ISO 3166-2 shaped.
pub fn normalise_region_code(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let bytes = trimmed.as_bytes();
    // ^[A-Za-z]{2}-[A-Za-z0-9]{1,3}$
    if bytes.len() < 4 || bytes.len() > 6 || bytes[2] != b'-' {
        return None;
    }
    if !bytes[..2].iter().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    if !bytes[3..].iter().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    Some(format!("{}-{}", trimmed[..2].to_uppercase(), trimmed[3..].to_uppercase()))
}

/// `2026-01-05` → `1` (Monday). Timezone-independent, so results are stable.
pub fn day_of_week(iso_date: &str) -> u32 {
    parse_date(iso_date)
        .map(|d| d.weekday().num_days_from_sunday())
        .unwrap_or(0)
}

pub fn is_weekend_date(iso_date: &str) -> bool {
    matches!(day_of_week(iso_date), 0 | 6)
}

/// `2026-01-05` → `20260105`, the lexicographically sortable form.
pub fn date_sort_key(iso_date: &str) -> i64 {
    iso_date.replace('-', "").parse().unwrap_or(0)
}

/// Inclusive list of ISO dates from `from` to `to`.
pub fn each_date(from: &str, to: &str) -> Vec<String> {
    let (Some(f), Some(t)) = (parse_date(from), parse_date(to)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = f;
    while cursor <= t {
        out.push(cursor.format("%Y-%m-%d").to_string());
        cursor += chrono::Duration::days(1);
    }
    out
}

pub fn add_days(iso_date: &str, days: i64) -> String {
    parse_date(iso_date)
        .map(|d| (d + chrono::Duration::days(days)).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| iso_date.to_string())
}

/// Today in UTC, `YYYY-MM-DD` (the original uses `toISOString()`).
pub fn today_iso() -> String {
    chrono::Utc::now().date_naive().format("%Y-%m-%d").to_string()
}

/// ISO-8601 timestamp with milliseconds and a `Z`, like `toISOString()`.
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

// ------------------------------------------------------------------ parsing

/// Maps an upstream `holidayTypes` entry onto this service's vocabulary. An
/// unrecognised value is dropped rather than passed through, because inventing
/// a type would break the contract for every consumer.
fn to_holiday_type(value: &str) -> Option<String> {
    HOLIDAY_TYPES.iter().find(|t| **t == value).map(|t| (*t).to_string())
}

/// Validates one upstream holiday object and maps it onto the [`Holiday`]
/// shape. Every required field is checked here so a partially broken upstream
/// payload surfaces as a single parse error (502) instead of `null` leaking
/// into a 200 response.
pub fn parse_holiday(raw: &Value, fallback_country_code: Option<&str>) -> Result<Holiday, ApiError> {
    let obj = raw
        .as_object()
        .ok_or_else(|| parse_error("Upstream returned a holiday entry that is not an object"))?;

    let date = obj
        .get("date")
        .and_then(Value::as_str)
        .filter(|d| is_real_iso_date(d))
        .ok_or_else(|| parse_error("Upstream holiday is missing a valid `date`"))?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| parse_error("Upstream holiday is missing `name`"))?;

    let country_code = match obj.get("countryCode").and_then(Value::as_str) {
        Some(code) if !code.is_empty() => code.to_uppercase(),
        _ => fallback_country_code
            .ok_or_else(|| parse_error("Upstream holiday is missing `countryCode`"))?
            .to_string(),
    };

    let national = obj
        .get("nationalHoliday")
        .and_then(Value::as_bool)
        .ok_or_else(|| parse_error("Upstream holiday is missing a boolean `nationalHoliday`"))?;

    let codes: Vec<String> = match obj.get("subdivisionCodes") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.to_uppercase())
            .collect(),
        Some(_) => {
            return Err(parse_error("Upstream holiday has a non-array `subdivisionCodes`"));
        }
    };

    let types_raw = obj
        .get("holidayTypes")
        .and_then(Value::as_array)
        .ok_or_else(|| parse_error("Upstream holiday is missing an array `holidayTypes`"))?;
    let mut types: Vec<String> = types_raw
        .iter()
        .filter_map(|v| v.as_str())
        .filter_map(to_holiday_type)
        .collect();
    // Stable de-duplication (Nager has been observed to repeat a type), sorted.
    types.sort();
    types.dedup();

    let mut codes = codes;
    codes.sort();
    codes.dedup();

    let day = day_of_week(date);
    Ok(Holiday {
        date: date.to_string(),
        name: name.to_string(),
        country_code,
        // A national holiday is the one that applies everywhere, so it cannot
        // be meaningfully narrowed to a list of subdivisions.
        scope: if national { "national" } else { "subdivision" }.to_string(),
        subdivision_codes: if national { Vec::new() } else { codes },
        types,
        day_of_week: day,
        is_weekend: matches!(day, 0 | 6),
    })
}

/// Validates the whole upstream list, or fails loudly.
pub fn parse_holiday_list(raw: &Value, country_code: Option<&str>) -> Result<Vec<Holiday>, ApiError> {
    let arr = raw
        .as_array()
        .ok_or_else(|| parse_error("Upstream holiday payload is not a JSON array"))?;
    arr.iter().map(|entry| parse_holiday(entry, country_code)).collect()
}

// --------------------------------------------------------- sorting/filters

/// Deterministic total ordering. `date` alone is not unique — the United
/// States has two holidays on 2026-10-12 — so `countryCode` and `name` break
/// every tie. Without this, offset pagination would silently drop or
/// duplicate rows between requests. The tie-break is identical in both
/// directions so the order is total.
pub fn sort_holidays(items: &mut Vec<Holiday>, sort: &str) {
    let descending = sort.starts_with('-');
    let key = sort.strip_prefix('-').unwrap_or(sort);
    items.sort_by(|a, b| {
        let primary = match key {
            "country" => a.country_code.cmp(&b.country_code),
            "name" => a.name.cmp(&b.name),
            _ => date_sort_key(&a.date).cmp(&date_sort_key(&b.date)),
        };
        let primary = if descending { primary.reverse() } else { primary };
        primary
            .then_with(|| a.date.cmp(&b.date))
            .then_with(|| a.country_code.cmp(&b.country_code))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// Case-insensitive type filter; an entry matches if ANY of its types match.
pub fn filter_by_types(items: Vec<Holiday>, types: Option<&[String]>) -> Vec<Holiday> {
    match types {
        None => items,
        Some(wanted) if wanted.is_empty() => items,
        Some(wanted) => items
            .into_iter()
            .filter(|h| {
                h.types
                    .iter()
                    .any(|t| wanted.iter().any(|w| t.eq_ignore_ascii_case(w)))
            })
            .collect(),
    }
}

pub fn filter_by_date_range(items: Vec<Holiday>, from: Option<&str>, to: Option<&str>) -> Vec<Holiday> {
    items
        .into_iter()
        .filter(|h| from.map(|f| h.date >= f).unwrap_or(true) && to.map(|t| h.date <= t).unwrap_or(true))
        .collect()
}

/// Every distinct ISO 3166-2 code present in the payload for a country.
pub fn known_regions(items: &[Holiday]) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    for holiday in items {
        for code in &holiday.subdivision_codes {
            set.insert(code.clone());
        }
    }
    set.into_iter().collect()
}

/// A holiday applies in `region` when it is national or lists that region.
pub fn applies_to_region(holiday: &Holiday, region: &str) -> bool {
    holiday.scope == "national" || holiday.subdivision_codes.iter().any(|c| c == region)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at_new_year() -> Value {
        // Real `GET /api/v4/Holidays/AT/2026` row (verified against the live API).
        json!({
            "date": "2026-01-01",
            "name": "New Year's Day",
            "countryCode": "AT",
            "nationalHoliday": true,
            "subdivisionCodes": null,
            "holidayTypes": ["Public"]
        })
    }

    fn at_regional() -> Value {
        json!({
            "date": "2026-03-19",
            "name": "Saint Joseph's Day",
            "countryCode": "AT",
            "nationalHoliday": false,
            "subdivisionCodes": ["AT-8", "AT-2", "AT-6", "AT-7"],
            "holidayTypes": ["School"]
        })
    }

    #[test]
    fn parses_a_real_national_holiday_row() {
        let h = parse_holiday(&at_new_year(), None).expect("parses");
        assert_eq!(h.date, "2026-01-01");
        assert_eq!(h.country_code, "AT");
        assert_eq!(h.scope, "national");
        assert!(h.subdivision_codes.is_empty(), "national holidays carry no subdivisions");
        assert_eq!(h.types, vec!["Public"]);
        assert_eq!(h.day_of_week, 4, "2026-01-01 is a Thursday");
        assert!(!h.is_weekend);
        let text = serde_json::to_string(&h).unwrap();
        assert!(text.contains("\"countryCode\":\"AT\""), "serialises as camelCase");
        assert!(text.contains("\"dayOfWeek\":4"));
    }

    #[test]
    fn parses_a_subdivision_holiday_with_sorted_codes() {
        let h = parse_holiday(&at_regional(), Some("AT")).expect("parses");
        assert_eq!(h.scope, "subdivision");
        assert_eq!(h.subdivision_codes, vec!["AT-2", "AT-6", "AT-7", "AT-8"]);
        assert_eq!(h.country_code, "AT", "fallback code fills in");
        assert_eq!(h.day_of_week, 4, "2026-03-19 is a Thursday");
    }

    #[test]
    fn rejects_broken_upstream_rows() {
        assert!(parse_holiday(&json!({ "name": "x" }), None).is_err(), "no date");
        assert!(parse_holiday(&json!({ "date": "2026-13-01", "name": "x", "nationalHoliday": true, "holidayTypes": [] }), None).is_err(), "impossible date");
        assert!(parse_holiday(&json!({ "date": "2026-01-01", "nationalHoliday": true, "holidayTypes": [] }), None).is_err(), "no name");
        assert!(parse_holiday(&json!({ "date": "2026-01-01", "name": "x", "holidayTypes": [] }), None).is_err(), "no nationalHoliday flag");
        assert!(parse_holiday(&json!({ "date": "2026-01-01", "name": "x", "nationalHoliday": true }), None).is_err(), "no holidayTypes");
        assert!(parse_holiday(&at_new_year(), None).is_ok());
        assert!(parse_holiday(&json!({ "date": "2026-01-01", "name": "x", "nationalHoliday": true, "holidayTypes": [] }), None).is_ok(), "empty types are legal");
    }

    #[test]
    fn unknown_types_are_dropped_and_deduplicated() {
        let raw = json!({
            "date": "2026-05-01", "name": "May Day", "countryCode": "AT",
            "nationalHoliday": true, "subdivisionCodes": null,
            "holidayTypes": ["Public", "Public", "CottonIndustry"]
        });
        let h = parse_holiday(&raw, None).expect("parses");
        assert_eq!(h.types, vec!["Public"], "unknown values dropped, duplicates removed");
    }

    #[test]
    fn sort_is_total_across_equal_dates() {
        let mut items = vec![
            parse_holiday(&json!({ "date": "2026-10-12", "name": "Indigenous Peoples' Day", "countryCode": "US", "nationalHoliday": true, "subdivisionCodes": null, "holidayTypes": ["Public"] }), None).unwrap(),
            parse_holiday(&json!({ "date": "2026-10-12", "name": "Columbus Day", "countryCode": "US", "nationalHoliday": true, "subdivisionCodes": null, "holidayTypes": ["Public"] }), None).unwrap(),
            parse_holiday(&json!({ "date": "2026-10-11", "name": "Earlier", "countryCode": "AT", "nationalHoliday": true, "subdivisionCodes": null, "holidayTypes": ["Public"] }), None).unwrap(),
        ];
        sort_holidays(&mut items, "date");
        let names: Vec<&str> = items.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["Earlier", "Columbus Day", "Indigenous Peoples' Day"]);
        // Descending flips the primary key but the tie-break stays ascending,
        // so pagination is stable in both directions.
        sort_holidays(&mut items, "-date");
        let names: Vec<&str> = items.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["Columbus Day", "Indigenous Peoples' Day", "Earlier"]);
    }

    #[test]
    fn type_and_date_filters_are_case_insensitive_and_inclusive() {
        let items = vec![
            parse_holiday(&at_new_year(), None).unwrap(),
            parse_holiday(&at_regional(), None).unwrap(),
        ];
        let public = filter_by_types(items.clone(), Some(&["public".to_string()]));
        assert_eq!(public.len(), 1);
        let ranged = filter_by_date_range(items, Some("2026-02-01"), Some("2026-12-31"));
        assert_eq!(ranged.len(), 1, "the January holiday is excluded");
        assert_eq!(ranged[0].name, "Saint Joseph's Day");
    }

    #[test]
    fn regions_aggregate_and_match_nationally() {
        let items = vec![
            parse_holiday(&at_new_year(), None).unwrap(),
            parse_holiday(&at_regional(), None).unwrap(),
        ];
        assert_eq!(known_regions(&items), vec!["AT-2", "AT-6", "AT-7", "AT-8"]);
        assert!(applies_to_region(&items[0], "AT-2"), "national applies everywhere");
        assert!(applies_to_region(&items[1], "AT-2"));
        assert!(!applies_to_region(&items[1], "AT-1"));
    }

    #[test]
    fn date_helpers_are_utc_and_shape_strict() {
        assert!(is_real_iso_date("2026-02-28"));
        assert!(!is_real_iso_date("2026-02-30"), "rollover dates are rejected");
        assert!(!is_real_iso_date("2026-2-3"));
        assert_eq!(day_of_week("2026-01-05"), 1, "Monday");
        assert_eq!(day_of_week("2026-10-18"), 0, "Sunday");
        assert!(is_weekend_date("2026-10-17"));
        assert_eq!(each_date("2026-10-15", "2026-10-18").len(), 4);
        assert_eq!(add_days("2026-01-01", -1), "2025-12-31");
        assert_eq!(date_sort_key("2026-01-05"), 20_260_105);
    }

    #[test]
    fn country_and_region_codes_normalise() {
        assert_eq!(normalise_country_code(" at ").as_deref(), Some("AT"));
        assert_eq!(normalise_country_code("USA"), None, "three letters are not alpha-2");
        assert_eq!(normalise_region_code("at-2").as_deref(), Some("AT-2"));
        assert_eq!(normalise_region_code("US-CA").as_deref(), Some("US-CA"));
        assert_eq!(normalise_region_code("GB-ENG").as_deref(), Some("GB-ENG"));
        assert_eq!(normalise_region_code("AT"), None);
        assert_eq!(normalise_region_code("AT-1234"), None, "max three alphanumeric");
        assert_eq!(normalise_region_code("AT-"), None);
    }
}
