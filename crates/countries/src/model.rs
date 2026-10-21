//! Country domain model + the `fields` catalogue (single source of truth for
//! the `fields` query parameter and `GET /countries/fields`).

use serde_json::{json, Map, Value};

/// One country, normalised from mledoze/countries joined with GeoNames.
/// Every optional upstream value becomes an explicit `null`/empty so no
/// documented response field can ever disappear.
#[derive(Debug, Clone)]
pub struct Country {
    pub name_common: String,
    pub name_official: String,
    pub alpha2: String,
    pub alpha3: String,
    pub numeric: Option<String>,
    pub ioc: Option<String>,
    pub capitals: Vec<String>,
    pub alt_spellings: Vec<String>,
    pub region: String,
    pub subregion: Option<String>,
    pub continent_code: Option<String>,
    pub continent_name: Option<String>,
    pub population: Option<i64>,
    pub area: Option<f64>,
    pub landlocked: bool,
    pub independent: Option<bool>,
    pub un_member: bool,
    pub status: String,
    pub un_regional_group: Option<String>,
    pub borders: Vec<String>,
    pub tlds: Vec<String>,
    pub calling_codes: Vec<String>,
    pub currencies: Vec<Currency>,
    pub languages: Vec<Language>,
    pub latlng: Option<(f64, f64)>,
    pub flag_emoji: String,
    pub geoname_id: Option<i64>,
    pub native_names: Vec<NativeName>,
    pub translations: Map<String, Value>,
    pub demonyms: Map<String, Value>,
}

#[derive(Debug, Clone)]
pub struct Currency {
    pub code: String,
    pub name: Option<String>,
    pub symbol: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Language {
    pub code: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct NativeName {
    pub language: String,
    pub common: Option<String>,
    pub official: Option<String>,
}

/// GeoNames continent codes → human-readable names (same table as upstream).
pub const CONTINENT_NAMES: &[(&str, &str)] = &[
    ("AF", "Africa"),
    ("AN", "Antarctica"),
    ("AS", "Asia"),
    ("EU", "Europe"),
    ("NA", "North America"),
    ("OC", "Oceania"),
    ("SA", "South America"),
];

/// Field catalogue: name, type, description, source. Served by
/// `/countries/fields` and used to validate the `fields` parameter.
pub const FIELD_CATALOGUE: &[(&str, &str, &str, &str)] = &[
    ("name", "string", "Common English name, e.g. \"Germany\".", "mledoze/countries"),
    ("officialName", "string", "Official long-form name, e.g. \"Federal Republic of Germany\".", "mledoze/countries"),
    ("alpha2", "string", "ISO 3166-1 alpha-2 code, e.g. \"DE\".", "mledoze/countries"),
    ("alpha3", "string", "ISO 3166-1 alpha-3 code, e.g. \"DEU\".", "mledoze/countries"),
    ("numeric", "string | null", "ISO 3166-1 numeric code as a zero-padded string. Null for user-assigned codes the dataset leaves blank.", "mledoze/countries"),
    ("ioc", "string | null", "International Olympic Committee code. Null where the dataset has none.", "mledoze/countries"),
    ("capital", "string | null", "Primary capital, or null when the country has none.", "mledoze/countries"),
    ("capitals", "string[]", "All capital cities; an empty array when there is no capital.", "mledoze/countries"),
    ("altSpellings", "string[]", "Alternative spellings and former names.", "mledoze/countries"),
    ("region", "string", "UN region, e.g. \"Europe\".", "mledoze/countries"),
    ("subregion", "string | null", "UN subregion, e.g. \"Western Europe\"; null when unassigned.", "mledoze/countries"),
    ("continent", "string | null", "Continent name, e.g. \"Europe\"; null when unassigned.", "geonames"),
    ("continentCode", "string | null", "GeoNames continent code, e.g. \"EU\"; null when unassigned.", "geonames"),
    ("population", "number | null", "Latest population figure.", "geonames"),
    ("area", "number | null", "Area in square kilometres.", "mledoze/countries"),
    ("landlocked", "boolean", "Whether the country is surrounded by land.", "mledoze/countries"),
    ("independent", "boolean | null", "Sovereign state; null where the upstream has no answer.", "mledoze/countries"),
    ("unMember", "boolean", "Member of the United Nations.", "mledoze/countries"),
    ("status", "string", "ISO 3166-1 assignment status, e.g. \"officially-assigned\".", "mledoze/countries"),
    ("unRegionalGroup", "string | null", "UN regional group; null when unassigned.", "mledoze/countries"),
    ("borders", "string[]", "Border countries as ISO alpha-3 codes.", "mledoze/countries"),
    ("tlds", "string[]", "Country-code top-level domains, e.g. [\".de\"].", "mledoze/countries"),
    ("callingCodes", "string[]", "International dialling codes without the leading \"+\", e.g. [\"49\"].", "mledoze/countries"),
    ("currencies", "{ code, name, symbol }[]", "Currencies in circulation; an empty array when none are listed.", "mledoze/countries"),
    ("languages", "{ code, name }[]", "Official languages; an empty array when none are listed.", "mledoze/countries"),
    ("latlng", "[number, number] | null", "Approximate [latitude, longitude] of the country centroid.", "mledoze/countries"),
    ("flag", "string", "Unicode flag emoji. Flag artwork is not covered by the dataset licence.", "mledoze/countries"),
    ("geonameId", "number | null", "GeoNames identifier, for joining with other GeoNames datasets.", "geonames"),
    ("nativeNames", "{ language, common, official }[]", "Names in each official language of the country.", "mledoze/countries"),
    ("translations", "Record<string, { common, official }>", "Name translated into the languages the dataset carries.", "mledoze/countries"),
    ("demonyms", "Record<string, { feminine, masculine }>", "Demonyms keyed by language code.", "mledoze/countries"),
];

/// Every accepted `fields` value, in catalogue order.
pub const FIELD_NAMES: &[&str] = &[
    "name", "officialName", "alpha2", "alpha3", "numeric", "ioc", "capital", "capitals",
    "altSpellings", "region", "subregion", "continent", "continentCode", "population", "area",
    "landlocked", "independent", "unMember", "status", "unRegionalGroup", "borders", "tlds",
    "callingCodes", "currencies", "languages", "latlng", "flag", "geonameId", "nativeNames",
    "translations", "demonyms",
];

/// Fields returned when `fields` is omitted. Deliberately lean: the bulky
/// reference data (translations, demonyms, nativeNames) is opt-in.
pub const DEFAULT_FIELDS: &[&str] = &[
    "name", "officialName", "alpha2", "alpha3", "numeric", "ioc", "region", "subregion",
    "continent", "population", "area", "capital", "flag",
];

/// Sort keys accepted by `GET /countries/countries?sort=`.
pub const SORT_KEYS: &[&str] = &[
    "name", "officialName", "alpha2", "alpha3", "numeric", "population", "area", "region",
    "subregion", "continent", "capital", "landlocked", "independent", "unMember", "status",
];

pub fn field_catalogue_json() -> Value {
    Value::Array(
        FIELD_CATALOGUE
            .iter()
            .map(|(name, ty, desc, source)| json!({ "name": name, "type": ty, "description": desc, "source": source }))
            .collect(),
    )
}

pub fn is_known_field(field: &str) -> bool {
    FIELD_NAMES.contains(&field)
}

/// `{ alpha2, alpha3, name }` stub used wherever a full record would be noise.
pub fn country_stub(c: &Country) -> Value {
    json!({ "alpha2": c.alpha2, "alpha3": c.alpha3, "name": c.name_common })
}

/// Applies the field projection to one country. Unknown fields were already
/// rejected upstream; `null` is emitted for absent values.
pub fn project_country(c: &Country, fields: &[String]) -> Value {
    let mut out = Map::new();
    for field in fields {
        let value = match field.as_str() {
            "name" => json!(c.name_common),
            "officialName" => json!(c.name_official),
            "alpha2" => json!(c.alpha2),
            "alpha3" => json!(c.alpha3),
            "numeric" => json!(c.numeric),
            "ioc" => json!(c.ioc),
            "capital" => json!(c.capitals.first().cloned()),
            "capitals" => json!(c.capitals),
            "altSpellings" => json!(c.alt_spellings),
            "region" => json!(c.region),
            "subregion" => json!(c.subregion),
            "continent" => json!(c.continent_name),
            "continentCode" => json!(c.continent_code),
            "population" => json!(c.population),
            "area" => match c.area {
                Some(a) => json!(a),
                None => Value::Null,
            },
            "landlocked" => json!(c.landlocked),
            "independent" => json!(c.independent),
            "unMember" => json!(c.un_member),
            "status" => json!(c.status),
            "unRegionalGroup" => json!(c.un_regional_group),
            "borders" => json!(c.borders),
            "tlds" => json!(c.tlds),
            "callingCodes" => json!(c.calling_codes),
            "currencies" => Value::Array(
                c.currencies
                    .iter()
                    .map(|cur| json!({ "code": cur.code, "name": cur.name, "symbol": cur.symbol }))
                    .collect(),
            ),
            "languages" => Value::Array(
                c.languages
                    .iter()
                    .map(|l| json!({ "code": l.code, "name": l.name }))
                    .collect(),
            ),
            "latlng" => match c.latlng {
                Some((lat, lon)) => json!([lat, lon]),
                None => Value::Null,
            },
            "flag" => json!(c.flag_emoji),
            "geonameId" => json!(c.geoname_id),
            "nativeNames" => Value::Array(
                c.native_names
                    .iter()
                    .map(|n| json!({ "language": n.language, "common": n.common, "official": n.official }))
                    .collect(),
            ),
            "translations" => Value::Object(c.translations.clone()),
            "demonyms" => Value::Object(c.demonyms.clone()),
            _ => Value::Null,
        };
        out.insert(field.clone(), value);
    }
    Value::Object(out)
}

/// Lowercased, trimmed text key used for name matching.
pub fn normalise_text(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Case-sensitive-ish text comparator: `None` sorts last, matching upstream.
pub fn compare_text(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b))
}

pub fn compare_optional_text(a: &Option<String>, b: &Option<String>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => compare_text(x, y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

pub fn compare_optional_f64(a: &Option<f64>, b: &Option<f64>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

pub fn compare_optional_i64(a: &Option<i64>, b: &Option<i64>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}
