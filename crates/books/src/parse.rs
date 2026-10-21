//! Parser layer: turns loose Open Library / country-dataset JSON into the
//! strict response DTOs, and owns the stable-sort guarantee.
//!
//! Ported from `book-api/src/books/parsers/{parse.utils,openlibrary.parser,
//! countries.parser}.ts` plus the response DTOs in `books/dto/response.types.ts`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Number, Value};

use master_common::error::ApiError;

use crate::openlibrary::{author_photo_url, cover_url, open_library_page_url, code};

/// 502 — the upstream answered, but the payload did not match the documented
/// shape (original `ParseError`).
pub fn parse_error(message: impl Into<String>, details: Value) -> ApiError {
    ApiError::Upstream {
        code: code("PARSE_ERROR"),
        message: message.into(),
        details: Some(details),
    }
}

/// 400 — the caller supplied something we cannot work with (original
/// `InvalidRequestError`).
pub fn invalid_request(message: impl Into<String>, details: Value) -> ApiError {
    ApiError::validation_details(code("INVALID_REQUEST"), message, details)
}

/// 404 — the upstream answered, but the record does not exist (original
/// `NotFoundError`).
pub fn not_found(message: impl Into<String>, details: Value) -> ApiError {
    ApiError::not_found_details(code("NOT_FOUND"), message, details)
}

// ---------------------------------------------------------------- primitives

/// Reads the last segment of an Open Library key, e.g. `OL45804W` from
/// `/works/OL45804W` and `eng` from `/languages/eng`. A bare id with no path
/// prefix is tolerated as well.
pub fn parse_ol_id(value: &Value) -> Option<String> {
    let s = value.as_str()?;
    if let Some(pos) = s.rfind('/') {
        let segment = &s[pos + 1..];
        return if segment.is_empty() {
            None
        } else {
            Some(segment.to_string())
        };
    }
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        Some(s.to_string())
    } else {
        None
    }
}

/// Non-empty trimmed string; a finite number is stringified, as upstream.
pub fn as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A JSON number, tolerating a numeric string (`"1970"`). Integers stay
/// integers in the output.
pub fn as_number(value: &Value) -> Option<Number> {
    match value {
        Value::Number(n) => Some(n.clone()),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                return None;
            }
            if let Ok(i) = t.parse::<i64>() {
                return Some(Number::from(i));
            }
            t.parse::<f64>().ok().and_then(Number::from_f64)
        }
        _ => None,
    }
}

/// `true`/`"true"`/`false`/`"false"` only.
pub fn as_boolean(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::String(s) if s == "true" => Some(true),
        Value::String(s) if s == "false" => Some(false),
        _ => None,
    }
}

/// A string array, tolerating a lone string in place of a one-element array.
pub fn as_string_array(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items.iter().filter_map(as_string).collect(),
        v => as_string(v).into_iter().collect(),
    }
}

/// Language codes, tolerating both shapes Open Library uses: a plain
/// `string[]` of MARC codes on `search.json` and a `[{"key": "/languages/eng"}]`
/// list on an edition record.
pub fn as_language_codes(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|entry| match entry {
                Value::String(_) => as_string(entry),
                Value::Object(_) => {
                    let key = entry.get("key").unwrap_or(&Value::Null);
                    parse_ol_id(key).or_else(|| as_string(key))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Open Library stores free text either as a plain string or as a typed text
/// node `{ "type": "/type/text", "value": "..." }`; both are accepted.
pub fn as_text(value: &Value) -> Option<String> {
    if let Some(direct) = as_string(value) {
        return Some(direct);
    }
    value.get("value").and_then(as_string)
}

/// Cover ids with the `-1` "absent" sentinel removed.
pub fn as_image_ids(value: &Value) -> Vec<i64> {
    as_string_array(value)
        .iter()
        .filter_map(|s| s.parse::<i64>().ok())
        .filter(|id| *id > 0)
        .collect()
}

fn get<'a>(doc: &'a Value, key: &str) -> &'a Value {
    doc.get(key).unwrap_or(&Value::Null)
}

// -------------------------------------------------------------------- shapes

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverSet {
    pub id: i64,
    pub small: String,
    pub medium: String,
    pub large: String,
}

pub fn to_cover_set(cover_id: Option<i64>) -> Option<CoverSet> {
    let id = cover_id?;
    if id <= 0 {
        return None;
    }
    Some(CoverSet {
        id,
        small: cover_url(id, "S"),
        medium: cover_url(id, "M"),
        large: cover_url(id, "L"),
    })
}

/// Reads the cover id from any of the three shapes a work arrives in:
/// `cover_i` (search), `cover_id` (subjects) or a `covers` array with `-1`
/// placeholders (single work).
pub fn cover_id_from_doc(doc: &Value) -> Option<i64> {
    as_number(get(doc, "cover_i"))
        .and_then(|n| n.as_i64())
        .or_else(|| as_number(get(doc, "cover_id")).and_then(|n| n.as_i64()))
        .or_else(|| as_image_ids(get(doc, "covers")).first().copied())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorRef {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub photo: Option<String>,
    /// Set when the author record was resolved for `GET /books/books/{key}/authors`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub birth_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub death_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopLink {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Excerpt {
    pub excerpt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorSummary {
    pub key: String,
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub photo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub birth_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub death_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    #[serde(default)]
    pub alternate_names: Vec<String>,
    #[serde(default)]
    pub top_links: Vec<TopLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkSummary {
    pub key: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    pub authors: Vec<AuthorRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_publish_year: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edition_count: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_fulltext: Option<bool>,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover: Option<CoverSet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating_average: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating_count: Option<Number>,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkDetail {
    #[serde(flatten)]
    pub summary: WorkSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_sentence: Option<String>,
    #[serde(default)]
    pub subjects: Vec<String>,
    #[serde(default)]
    pub subject_people: Vec<String>,
    #[serde(default)]
    pub subject_places: Vec<String>,
    #[serde(default)]
    pub subject_times: Vec<String>,
    #[serde(default)]
    pub excerpts: Vec<Excerpt>,
    #[serde(default)]
    pub links: Vec<TopLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditionDetail {
    pub key: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edition_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_key: Option<String>,
    #[serde(default)]
    pub publishers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_date: Option<String>,
    #[serde(default)]
    pub publish_places: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number_of_pages: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pagination: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_format: Option<String>,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover: Option<CoverSet>,
    #[serde(default)]
    pub isbn10: Vec<String>,
    #[serde(default)]
    pub isbn13: Vec<String>,
    #[serde(default)]
    pub oclc_numbers: Vec<String>,
    #[serde(default)]
    pub lc_classifications: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lccn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copyright_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internet_archive_id: Option<String>,
    #[serde(default)]
    pub external_ids: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified_at: Option<String>,
    pub url: String,
}

// ------------------------------------------------------------------- authors

/// Reads the two author shapes Open Library uses, then falls back to the
/// parallel arrays of `search.json`.
pub fn parse_author_refs(doc: &Value) -> Vec<AuthorRef> {
    let mut refs: Vec<AuthorRef> = Vec::new();

    if let Some(entries) = get(doc, "authors").as_array() {
        // `works/{key}.json`: [{ author: { key: '/authors/OL..A' }, type: { key: '..' } }]
        for entry in entries {
            let key = parse_ol_id(get(get(entry, "author"), "key"));
            let Some(key) = key else { continue };
            refs.push(AuthorRef {
                key: key.clone(),
                name: None,
                role: parse_ol_id(get(get(entry, "type"), "key")),
                url: open_library_page_url(&format!("authors/{key}")),
                photo: None,
                birth_date: None,
                death_date: None,
            });
        }
        if !refs.is_empty() {
            return refs;
        }

        // `subjects/{name}.json`: [{ key: '/authors/OL..A', name: '...' }]
        for entry in entries {
            let Some(key) = parse_ol_id(get(entry, "key")) else {
                continue;
            };
            refs.push(AuthorRef {
                key: key.clone(),
                name: as_string(get(entry, "name")),
                role: None,
                url: open_library_page_url(&format!("authors/{key}")),
                photo: Some(author_photo_url(&key, "M")),
                birth_date: None,
                death_date: None,
            });
        }
        if !refs.is_empty() {
            return refs;
        }
    }

    // `search.json`: parallel arrays.
    let names = as_string_array(get(doc, "author_name"));
    let keys = as_string_array(get(doc, "author_key"));
    let count = names.len().max(keys.len());
    for i in 0..count {
        let key = keys.get(i).cloned().unwrap_or_default();
        let mut r = AuthorRef {
            key: key.clone(),
            name: None,
            role: None,
            url: String::new(),
            photo: None,
            birth_date: None,
            death_date: None,
        };
        if !key.is_empty() {
            r.url = open_library_page_url(&format!("authors/{key}"));
            r.photo = Some(author_photo_url(&key, "M"));
        }
        if let Some(name) = names.get(i) {
            r.name = Some(name.clone());
        }
        if !r.key.is_empty() || r.name.is_some() {
            refs.push(r);
        }
    }

    refs
}

pub fn parse_author_summary(raw: &Value) -> AuthorSummary {
    let key = parse_ol_id(get(raw, "key")).unwrap_or_default();
    let name = as_string(get(raw, "name"))
        .or_else(|| as_string(get(raw, "personal_name")))
        .unwrap_or_else(|| key.clone());
    let photo = as_image_ids(get(raw, "photos")).first().copied();

    let top_links: Vec<TopLink> = get(raw, "links")
        .as_array()
        .map(|links| {
            links
                .iter()
                .filter_map(|link| {
                    let url = as_string(get(link, "url"))?;
                    Some(TopLink {
                        title: as_string(get(link, "title")),
                        url,
                    })
                })
                .take(5)
                .collect()
        })
        .unwrap_or_default();

    AuthorSummary {
        photo: photo.map(|_| author_photo_url(&key, "M")),
        url: open_library_page_url(&format!("authors/{key}")),
        key,
        name,
        birth_date: as_string(get(raw, "birth_date")),
        death_date: as_string(get(raw, "death_date")),
        bio: as_text(get(raw, "bio")),
        alternate_names: as_string_array(get(raw, "alternate_names")),
        top_links,
    }
}

// --------------------------------------------------------------------- works

pub fn parse_work_summary(doc: &Value) -> WorkSummary {
    let key = parse_ol_id(get(doc, "key")).unwrap_or_default();
    let title = as_string(get(doc, "title")).unwrap_or_else(|| "Untitled".to_string());
    let cover = to_cover_set(cover_id_from_doc(doc));
    let url = if key.is_empty() {
        open_library_page_url("search")
    } else {
        open_library_page_url(&format!("works/{key}"))
    };

    WorkSummary {
        subtitle: as_string(get(doc, "subtitle")),
        first_publish_year: as_number(get(doc, "first_publish_year")),
        edition_count: as_number(get(doc, "edition_count")),
        has_fulltext: as_boolean(get(doc, "has_fulltext")),
        languages: as_string_array(get(doc, "language")),
        cover,
        rating_average: as_number(get(doc, "ratings_average")),
        rating_count: as_number(get(doc, "ratings_count")),
        authors: parse_author_refs(doc),
        url,
        key,
        title,
    }
}

pub fn parse_work_detail(raw: &Value) -> WorkDetail {
    let summary = parse_work_summary(raw);

    let excerpts: Vec<Excerpt> = get(raw, "excerpts")
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|entry| {
                    let excerpt = as_string(get(entry, "excerpt"))?;
                    Some(Excerpt {
                        excerpt,
                        comment: as_string(get(entry, "comment")),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let links: Vec<TopLink> = get(raw, "links")
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|entry| {
                    let url = as_string(get(entry, "url"))?;
                    Some(TopLink {
                        title: as_string(get(entry, "title")),
                        url,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    WorkDetail {
        summary,
        description: as_text(get(raw, "description")),
        first_sentence: as_text(get(raw, "first_sentence")),
        subjects: as_string_array(get(raw, "subjects")),
        subject_people: as_string_array(get(raw, "subject_people")),
        subject_places: as_string_array(get(raw, "subject_places")),
        subject_times: as_string_array(get(raw, "subject_times")),
        excerpts,
        links,
        created_at: as_text(get(raw, "created")),
        last_modified_at: as_text(get(raw, "last_modified")),
    }
}

// ------------------------------------------------------------------ editions

fn parse_external_ids(value: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    let Some(ids) = value.as_object() else {
        return out;
    };
    for (scheme, list) in ids {
        let items = as_string_array(list);
        if !items.is_empty() {
            out.insert(scheme.clone(), json!(items));
        }
    }
    out
}

pub fn parse_edition(raw: &Value) -> EditionDetail {
    let key = parse_ol_id(get(raw, "key")).unwrap_or_default();
    let work_ref = get(raw, "works")
        .as_array()
        .and_then(|works| {
            works
                .iter()
                .find_map(|entry| parse_ol_id(get(entry, "key")))
        });

    let cover = to_cover_set(as_image_ids(get(raw, "covers")).first().copied());

    EditionDetail {
        url: open_library_page_url(&format!("books/{key}")),
        work_key: work_ref,
        cover,
        key,
        title: as_string(get(raw, "title")).unwrap_or_else(|| "Untitled".to_string()),
        subtitle: as_string(get(raw, "subtitle")),
        edition_name: as_string(get(raw, "edition_name")),
        publishers: as_string_array(get(raw, "publishers")),
        publish_date: as_string(get(raw, "publish_date")),
        publish_places: as_string_array(get(raw, "publish_places")),
        number_of_pages: as_number(get(raw, "number_of_pages")),
        pagination: as_string(get(raw, "pagination")),
        physical_format: as_string(get(raw, "physical_format")),
        languages: as_language_codes(get(raw, "languages")),
        description: as_text(get(raw, "description")),
        isbn10: as_string_array(get(raw, "isbn_10")),
        isbn13: as_string_array(get(raw, "isbn_13")),
        oclc_numbers: as_string_array(get(raw, "oclc_numbers")),
        lc_classifications: as_string_array(get(raw, "lc_classifications")),
        lccn: as_string(get(raw, "lccn")),
        copyright_date: as_string(get(raw, "copyright_date")),
        internet_archive_id: as_string(get(raw, "ocaid")),
        external_ids: parse_external_ids(get(raw, "identifiers")),
        created_at: as_text(get(raw, "created")),
        last_modified_at: as_text(get(raw, "last_modified")),
    }
}

// -------------------------------------------------------------------- search

#[derive(Debug, Clone, Serialize)]
pub struct ParsedSearch {
    pub total: Option<Number>,
    pub works: Vec<WorkSummary>,
}

pub fn parse_search_response(raw: &Value, sort: &str) -> Result<ParsedSearch, ApiError> {
    let Some(docs) = get(raw, "docs").as_array() else {
        return Err(parse_error(
            "Open Library search response has no \"docs\" array",
            json!({ "received": type_of(get(raw, "docs")) }),
        ));
    };

    let total = as_number(get(raw, "numFound")).or_else(|| as_number(get(raw, "num_found")));

    // A document without a resolvable work key cannot be linked to or fetched
    // again, so it is dropped rather than surfaced as an unusable record.
    let mut works: Vec<WorkSummary> = docs
        .iter()
        .filter(|doc| doc.is_object())
        .map(parse_work_summary)
        .filter(|work| !work.key.is_empty())
        .collect();
    sort_works(&mut works, sort);

    Ok(ParsedSearch { total, works })
}

// ------------------------------------------------------------------ subjects

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedSubject {
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_count: Option<Number>,
    pub works: Vec<WorkSummary>,
}

pub fn parse_subject_response(raw: &Value, sort: &str) -> Result<ParsedSubject, ApiError> {
    let Some(_) = get(raw, "works").as_array() else {
        return Err(parse_error(
            "Open Library subject response has no \"works\" array",
            json!({ "received": type_of(get(raw, "works")) }),
        ));
    };

    let name = as_string(get(raw, "name"))
        .or_else(|| {
            as_string(get(raw, "key")).map(|k| {
                k.strip_prefix("/subjects/").unwrap_or(&k).to_string()
            })
        })
        .unwrap_or_default();
    let work_count = as_number(get(raw, "work_count"));

    // The subject endpoint ignores `fields=`, so each work arrives with its
    // full `subject[]` array attached. Projecting here keeps the payload small.
    let mut works: Vec<WorkSummary> = get(raw, "works")
        .as_array()
        .map(|docs| {
            docs.iter()
                .filter(|doc| doc.is_object())
                .map(parse_work_summary)
                .filter(|work| !work.key.is_empty())
                .collect()
        })
        .unwrap_or_default();
    sort_works(&mut works, sort);

    Ok(ParsedSubject {
        subject: name,
        subject_type: as_string(get(raw, "subject_type")),
        work_count,
        works,
    })
}

// -------------------------------------------------------------------- limits

/// Numeric field comparator: records that lack the value always sort last.
fn by_number(direction_desc: bool) -> impl Fn(Option<f64>, Option<f64>) -> std::cmp::Ordering {
    move |left, right| match (left, right) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(l), Some(r)) => {
            if direction_desc {
                r.partial_cmp(&l).unwrap_or(std::cmp::Ordering::Equal)
            } else {
                l.partial_cmp(&r).unwrap_or(std::cmp::Ordering::Equal)
            }
        }
    }
}

fn collate(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase())
}

fn number_f64(n: &Option<Number>) -> Option<f64> {
    n.as_ref().and_then(Number::as_f64)
}

/// The stable-sort guarantee, ported from `sortWorks` in parse.utils.ts:
///
/// - `relevance` means "Open Library's own ranking": the upstream order is
///   passed through untouched.
/// - The explicit sorts are re-sorted with a comparator whose last criterion
///   is the unique work key, making the page byte-identical across identical
///   requests. Records lacking the sort value are always placed last.
pub fn sort_works(works: &mut [WorkSummary], sort: &str) {
    if sort == "relevance" {
        return;
    }
    works.sort_by(|a, b| {
        let primary = match sort {
            "new" => by_number(true)(number_f64(&a.first_publish_year), number_f64(&b.first_publish_year)),
            "old" => by_number(false)(number_f64(&a.first_publish_year), number_f64(&b.first_publish_year)),
            "title" => collate(&a.title, &b.title),
            "rating" => by_number(true)(number_f64(&a.rating_average), number_f64(&b.rating_average)),
            // `key`: the work-key tiebreaker below is the whole ordering.
            _ => std::cmp::Ordering::Equal,
        };
        primary.then_with(|| a.key.cmp(&b.key))
    });
}

// ----------------------------------------------------------------- countries

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Country {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub official_name: Option<String>,
    pub cca2: String,
    pub cca3: String,
    #[serde(default)]
    pub capital: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subregion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area_km2: Option<Number>,
    #[serde(default)]
    pub tld: Vec<String>,
    #[serde(default)]
    pub currencies: Vec<CountryCurrency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
    pub un_member: bool,
    pub independent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CountryCurrency {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
}

/// Projects the world-countries dataset onto our own record. The upstream body
/// is over a megabyte for 250 entries; without this projection every request
/// would carry translations and demonyms nobody asked for.
pub fn parse_countries(raw: &Value) -> Result<Vec<Country>, ApiError> {
    let Some(entries) = raw.as_array() else {
        return Err(parse_error(
            "Country dataset is not an array",
            json!({ "received": type_of(raw) }),
        ));
    };

    let mut countries: Vec<Country> = Vec::new();
    for entry in entries {
        let Some(cca2) = as_string(get(entry, "cca2")) else {
            continue;
        };
        let Some(cca3) = as_string(get(entry, "cca3")) else {
            continue;
        };

        countries.push(Country {
            name: as_string(get(get(entry, "name"), "common")).unwrap_or_else(|| cca3.clone()),
            official_name: as_string(get(get(entry, "name"), "official")),
            cca2: cca2.to_uppercase(),
            cca3: cca3.to_uppercase(),
            capital: as_string_array(get(entry, "capital")),
            region: as_string(get(entry, "region")),
            subregion: as_string(get(entry, "subregion")),
            area_km2: as_number(get(entry, "area")),
            tld: as_string_array(get(entry, "tld")),
            currencies: parse_currencies(get(entry, "currencies")),
            flag: as_string(get(entry, "flag")),
            un_member: as_boolean(get(entry, "unMember")).unwrap_or(false),
            independent: as_boolean(get(entry, "independent")).unwrap_or(false),
        });
    }

    if countries.is_empty() {
        return Err(parse_error("Country dataset contained no usable entries", json!({})));
    }

    // Deterministic order so pagination is stable: the dataset's own array
    // order is not part of its contract, the alpha-2 code is.
    countries.sort_by(|a, b| a.cca2.cmp(&b.cca2));
    Ok(countries)
}

/// `{ "USD": { name, symbol } }` -> `[{ code: 'USD', ... }]`, sorted by code.
fn parse_currencies(value: &Value) -> Vec<CountryCurrency> {
    let Some(entries) = value.as_object() else {
        return Vec::new();
    };
    let mut out: Vec<CountryCurrency> = entries
        .iter()
        .map(|(code, entry)| CountryCurrency {
            code: code.clone(),
            name: as_string(get(entry, "name")),
            symbol: as_string(get(entry, "symbol")),
        })
        .collect();
    out.sort_by(|a, b| a.code.cmp(&b.code));
    out
}

/// Case-insensitive substring match; `None` means "no filter".
/// Ported from `filterCountries` in countries.parser.ts.
pub fn filter_countries(
    countries: &[Country],
    q: Option<&str>,
    region: Option<&str>,
    un_member: Option<bool>,
    independent: Option<bool>,
) -> Vec<Country> {
    let needle = q.map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    let region = region.map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());

    countries
        .iter()
        .filter(|country| {
            if let Some(un) = un_member {
                if country.un_member != un {
                    return false;
                }
            }
            if let Some(ind) = independent {
                if country.independent != ind {
                    return false;
                }
            }
            if let Some(region) = &region {
                if !country.region.as_deref().unwrap_or("").to_lowercase().contains(region) {
                    return false;
                }
            }
            if let Some(needle) = &needle {
                let haystack = format!(
                    "{} {} {} {}",
                    country.name,
                    country.official_name.as_deref().unwrap_or(""),
                    country.cca2,
                    country.cca3
                )
                .to_lowercase();
                if !haystack.contains(needle) {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect()
}

// ------------------------------------------------------------------ helpers

/// Ported from `pageInfo` in books.service.ts. `hasMore` needs both signals:
/// the upstream total alone says a page 1 of 500 has more; a short page says
/// the index has nothing left even when the total disagrees.
pub fn page_info(limit: usize, offset: usize, returned: usize, total: Option<i64>) -> Value {
    let mut page = Map::new();
    page.insert("limit".into(), json!(limit));
    page.insert("offset".into(), json!(offset));
    page.insert("returned".into(), json!(returned));
    if let Some(total) = total {
        let more = returned >= limit && (offset as i64 + returned as i64) < total;
        page.insert("total".into(), json!(total));
        page.insert("hasMore".into(), json!(more));
        if more {
            page.insert("nextOffset".into(), json!(offset + limit));
        }
    }
    Value::Object(page)
}

/// Validates a record key locally rather than letting the upstream discover
/// the mistake (`/works/OL26320A.json` would 301-redirect to the author
/// record, so without this check a caller would get something they did not
/// ask for).
pub fn require_key(raw: &str, pattern: KeyPattern) -> Result<String, ApiError> {
    let trimmed = raw.trim();
    if pattern.matches(trimmed) {
        return Ok(trimmed.to_string());
    }
    Err(invalid_request(
        format!(
            "A {} looks like \"{}\"; \"{}\" is not one",
            pattern.label(),
            pattern.example(),
            trimmed
        ),
        json!({ "received": trimmed, "example": pattern.example() }),
    ))
}

#[derive(Debug, Clone, Copy)]
pub enum KeyPattern {
    /// `^OL\d+W$`
    Work,
    /// `^OL\d+A$`
    Author,
}

impl KeyPattern {
    fn matches(self, value: &str) -> bool {
        let bytes = value.as_bytes();
        let suffix = match self {
            KeyPattern::Work => b'W',
            KeyPattern::Author => b'A',
        };
        value.len() > 3
            && bytes.starts_with(b"OL")
            && bytes[2..bytes.len() - 1].iter().all(|b| b.is_ascii_digit())
            && bytes[bytes.len() - 1] == suffix
    }

    pub fn example(self) -> &'static str {
        match self {
            KeyPattern::Work => "OL45804W",
            KeyPattern::Author => "OL26320A",
        }
    }

    fn label(self) -> &'static str {
        match self {
            KeyPattern::Work => "work key",
            KeyPattern::Author => "author key",
        }
    }
}

fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "undefined",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openlibrary::SEARCH_SORTS;

    fn search_doc(key: &str, title: &str, year: Value, rating: Value) -> Value {
        json!({
            "key": format!("/works/{key}"),
            "title": title,
            "author_name": ["Author One", "Author Two"],
            "author_key": ["OL1A", "OL2A"],
            "first_publish_year": year,
            "ratings_average": rating,
            "cover_i": 4321,
            "language": ["eng"],
            "edition_count": 12,
            "has_fulltext": true,
        })
    }

    #[test]
    fn search_doc_becomes_a_work_summary_with_authors_and_cover() {
        let doc = search_doc("OL45804W", "Fantastic Mr Fox", json!(1970), json!(4.1));
        let work = parse_work_summary(&doc);
        assert_eq!(work.key, "OL45804W");
        assert_eq!(work.title, "Fantastic Mr Fox");
        assert_eq!(work.authors.len(), 2);
        assert_eq!(work.authors[0].key, "OL1A");
        assert_eq!(work.authors[0].name.as_deref(), Some("Author One"));
        assert_eq!(
            work.authors[0].url,
            "https://openlibrary.org/authors/OL1A"
        );
        assert_eq!(
            work.authors[0].photo.as_deref(),
            Some("https://covers.openlibrary.org/a/olid/OL1A-M.jpg")
        );
        assert_eq!(work.cover.as_ref().unwrap().id, 4321);
        assert_eq!(
            work.cover.as_ref().unwrap().medium,
            "https://covers.openlibrary.org/b/id/4321-M.jpg"
        );
        assert_eq!(work.first_publish_year.as_ref().and_then(Number::as_i64), Some(1970));
        assert_eq!(work.url, "https://openlibrary.org/works/OL45804W");
    }

    #[test]
    fn work_detail_shapes_the_typed_node_and_references() {
        let raw = json!({
            "key": "/works/OL45804W",
            "title": "Fantastic Mr Fox",
            "description": { "type": "/type/text", "value": "A clever fox." },
            "first_sentence": "Down in the valley...",
            "subjects": ["foxes"],
            "subject_people": ["Roald Dahl"],
            "excerpts": [{ "excerpt": "Boggis was a chicken farmer.", "comment": "opening" }],
            "links": [{ "url": "https://example.com", "title": "Fan page" }],
            "authors": [{ "author": { "key": "/authors/OL34184A" }, "type": { "key": "/type/author_role" } }],
            "created": { "type": "/type/datetime", "value": "2009-09-02T13:36:53.345741" },
        });
        let detail = parse_work_detail(&raw);
        assert_eq!(detail.description.as_deref(), Some("A clever fox."));
        assert_eq!(detail.first_sentence.as_deref(), Some("Down in the valley..."));
        assert_eq!(detail.subjects, vec!["foxes".to_string()]);
        assert_eq!(detail.excerpts.len(), 1);
        assert_eq!(detail.excerpts[0].excerpt, "Boggis was a chicken farmer.");
        assert_eq!(detail.links.len(), 1);
        assert_eq!(detail.authors[0].key, "OL34184A");
        assert_eq!(detail.authors[0].role.as_deref(), Some("author_role"));
        assert!(detail.created_at.is_some());
        // `url` comes from the flattened summary.
        assert_eq!(detail.summary.url, "https://openlibrary.org/works/OL45804W");
    }

    #[test]
    fn sorting_is_deterministic_and_keeps_missing_values_last() {
        let mut works = vec![
            parse_work_summary(&search_doc("OL9W", "b title", Value::Null, json!(3.0))),
            parse_work_summary(&search_doc("OL2W", "B Title", json!(2001), Value::Null)),
            parse_work_summary(&search_doc("OL5W", "a title", json!(2001), json!(4.5))),
            parse_work_summary(&search_doc("OL1W", "a title", json!(1990), json!(4.5))),
        ];

        // relevance: upstream order preserved untouched.
        let before: Vec<String> = works.iter().map(|w| w.key.clone()).collect();
        sort_works(&mut works, "relevance");
        assert_eq!(before, works.iter().map(|w| w.key.clone()).collect::<Vec<_>>());

        // new: newest first, missing year last, key tiebreak.
        sort_works(&mut works, "new");
        assert_eq!(works[0].key, "OL5W");
        assert_eq!(works[1].key, "OL2W");
        assert_eq!(works[2].key, "OL1W");
        assert_eq!(works[3].key, "OL9W");

        // old: oldest first, missing last.
        sort_works(&mut works, "old");
        assert_eq!(works[0].key, "OL1W");
        assert_eq!(works[3].key, "OL9W");

        // title: base-sensitivity, key tiebreak.
        sort_works(&mut works, "title");
        assert_eq!(works[0].key, "OL1W");
        assert_eq!(works[1].key, "OL5W");
        assert_eq!(works[2].key, "OL2W");
        assert_eq!(works[3].key, "OL9W");

        // rating: highest first, unrated last.
        sort_works(&mut works, "rating");
        assert_eq!(works[0].key, "OL5W");
        assert_eq!(works[1].key, "OL1W");
        assert_eq!(works[3].key, "OL9W");

        // key: pure ascending key order.
        sort_works(&mut works, "key");
        let keys: Vec<&str> = works.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, vec!["OL1W", "OL2W", "OL5W", "OL9W"]);

        assert!(SEARCH_SORTS.contains(&"relevance"));
    }

    #[test]
    fn page_info_reports_total_has_more_and_next_offset() {
        let with_total = page_info(20, 0, 20, Some(500));
        assert_eq!(with_total["total"], json!(500));
        assert_eq!(with_total["hasMore"], json!(true));
        assert_eq!(with_total["nextOffset"], json!(20));
        assert_eq!(with_total["returned"], json!(20));

        let short_page = page_info(20, 0, 12, Some(500));
        assert_eq!(short_page["hasMore"], json!(false));
        assert!(short_page.get("nextOffset").is_none(), "nextOffset only appears when there is a next page");

        let no_total = page_info(20, 0, 20, None);
        assert!(no_total.get("total").is_none());
        assert!(no_total.get("hasMore").is_none());
    }

    #[test]
    fn key_patterns_mirror_the_original_regexes() {
        assert_eq!(require_key(" OL45804W ", KeyPattern::Work).unwrap(), "OL45804W");
        assert_eq!(require_key("OL26320A", KeyPattern::Author).unwrap(), "OL26320A");
        let err = require_key("OL26320A", KeyPattern::Work).unwrap_err();
        assert!(err.to_string().contains("A work key looks like \"OL45804W\""));
        assert!(require_key("OL45804", KeyPattern::Work).is_err());
        assert!(require_key("", KeyPattern::Work).is_err());
        assert!(require_key("OL12XW", KeyPattern::Work).is_err());
    }

    #[test]
    fn parses_and_filters_the_embedded_countries_dataset() {
        let raw = svc_countries::dataset_raw_public();
        let countries = parse_countries(&raw).expect("embedded dataset parses");
        assert!(countries.len() >= 250);
        // Deterministic cca2 order.
        let mut sorted = countries.clone();
        sorted.sort_by(|a, b| a.cca2.cmp(&b.cca2));
        assert_eq!(countries, sorted);

        let de = countries.iter().find(|c| c.cca2 == "DE").expect("Germany present");
        assert_eq!(de.name, "Germany");
        assert_eq!(de.capital, vec!["Berlin".to_string()]);
        assert!(de.currencies.iter().any(|c| c.code == "EUR"));
        assert!(de.un_member);

        let filtered = filter_countries(&countries, Some("german"), Some("Europe"), None, None);
        assert!(filtered.iter().all(|c| c.cca2 == "DE"));

        let un_only = filter_countries(&countries, None, None, Some(true), None);
        assert!(un_only.iter().all(|c| c.un_member));
    }

    #[test]
    fn parse_author_summary_projects_links_and_photo() {
        let raw = json!({
            "key": "/authors/OL26320A",
            "name": "Ursula K. Le Guin",
            "birth_date": "21 October 1929",
            "death_date": "22 January 2018",
            "bio": "American author.",
            "alternate_names": ["Ursula Le Guin"],
            "photos": [123456, -1],
            "links": [
                { "url": "https://example.com/a", "title": "Site" },
                { "title": "No url — dropped" }
            ],
        });
        let author = parse_author_summary(&raw);
        assert_eq!(author.key, "OL26320A");
        assert_eq!(author.name, "Ursula K. Le Guin");
        assert_eq!(author.photo.as_deref(), Some("https://covers.openlibrary.org/a/olid/OL26320A-M.jpg"));
        assert_eq!(author.alternate_names, vec!["Ursula Le Guin".to_string()]);
        assert_eq!(author.top_links.len(), 1);
        assert_eq!(author.top_links[0].url, "https://example.com/a");
    }

    #[test]
    fn search_response_without_docs_is_a_parse_error() {
        let raw = json!({ "numFound": 3 });
        let err = parse_search_response(&raw, "relevance").unwrap_err();
        assert!(err.to_string().contains("no \"docs\" array"));
        let err = parse_subject_response(&json!({}), "relevance").unwrap_err();
        assert!(err.to_string().contains("no \"works\" array"));
    }
}
