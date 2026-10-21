//! Parsing of Open Library records into this service's own DTOs.
//! Ported from `book-api/src/books/parsers/{openlibrary,parse.utils}.ts`.

use serde_json::{json, Value};

use crate::{UPSTREAM_COVERS, UPSTREAM_OPENLIBRARY};

/// Extracts the record id after the final `/` of an Open Library key.
pub fn parse_ol_id(key: Option<&Value>) -> Option<String> {
    let s = key?.as_str()?;
    let segment = s.rsplit('/').next()?;
    if segment.is_empty() { None } else { Some(segment.to_string()) }
}

pub fn is_work_key(v: &str) -> bool {
    let bytes = v.as_bytes();
    bytes.len() >= 3
        && bytes[0] == b'O'
        && bytes[1] == b'L'
        && bytes[2..bytes.len() - 1].iter().all(|b| b.is_ascii_digit())
        && v.ends_with('W')
}

pub fn is_author_key(v: &str) -> bool {
    let bytes = v.as_bytes();
    bytes.len() >= 3
        && bytes[0] == b'O'
        && bytes[1] == b'L'
        && bytes[2..bytes.len() - 1].iter().all(|b| b.is_ascii_digit())
        && v.ends_with('A')
}

pub fn as_string(v: Option<&Value>) -> Option<String> {
    let s = v?.as_str()?.trim();
    if s.is_empty() { None } else { Some(s.to_string()) }
}

pub fn as_number(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub fn as_bool(v: Option<&Value>) -> Option<bool> {
    match v? {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => {
            if n.as_f64() == Some(1.0) { Some(true) } else if n.as_f64() == Some(0.0) { Some(false) } else { None }
        }
        _ => None,
    }
}

pub fn as_string_array(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(arr)) => arr.iter().filter_map(|x| as_string(Some(x))).collect(),
        other => as_string(other).into_iter().collect(),
    }
}

/// `description` may be a string or `{ value }`.
pub fn as_text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => as_string(Some(&Value::String(s.clone()))),
        Value::Object(o) => as_string(o.get("value")),
        _ => None,
    }
}

/// ISO-ish datetime extraction: `{ value: .. }` objects or plain strings.
pub fn as_datetime(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Object(o) => as_string(o.get("value")),
        other => as_string(Some(other)),
    }
}

/// `languages: [{ key: '/languages/eng' }]` → `["eng"]`.
pub fn as_language_codes(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|x| parse_ol_id(Some(x)))
            .collect(),
        _ => Vec::new(),
    }
}

pub fn as_image_ids(v: Option<&Value>) -> Vec<i64> {
    match v {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|x| as_number(Some(x)))
            .filter(|n| *n > 0.0)
            .map(|n| n as i64)
            .collect(),
        _ => Vec::new(),
    }
}

pub fn cover_url(cover_id: i64, size: &str) -> String {
    format!("{UPSTREAM_COVERS}/b/id/{cover_id}-{size}.jpg")
}

pub fn author_photo_url(author_key: &str, size: &str) -> String {
    format!("{UPSTREAM_COVERS}/a/olid/{author_key}-{size}.jpg")
}

pub fn open_library_page_url(path: &str) -> String {
    format!("{UPSTREAM_OPENLIBRARY}/{}", path.trim_start_matches('/'))
}

fn to_cover_set(cover_id: Option<i64>) -> Option<Value> {
    let id = cover_id?;
    Some(json!({
        "id": id,
        "small": cover_url(id, "S"),
        "medium": cover_url(id, "M"),
        "large": cover_url(id, "L"),
    }))
}

fn cover_id_from_doc(doc: &Value) -> Option<i64> {
    as_number(doc.get("cover_i"))
        .or_else(|| as_number(doc.get("cover_id")))
        .map(|n| n as i64)
        .or_else(|| as_image_ids(doc.get("covers")).first().copied())
}

/// Sort facet for search/subjects.
pub const SEARCH_SORTS: &[&str] = &["relevance", "new", "old", "title", "rating", "key"];

pub fn is_valid_sort(s: &str) -> bool {
    SEARCH_SORTS.contains(&s)
}

/// Stable sort: relevance keeps upstream order; others order by the facet with
/// a work-key tiebreaker.
pub fn sort_works(works: &mut [Value], sort: &str) {
    if sort == "relevance" || sort == "key" {
        works.sort_by(|a, b| {
            let ka = a.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let kb = b.get("key").and_then(|v| v.as_str()).unwrap_or("");
            ka.cmp(kb)
        });
        return;
    }
    works.sort_by(|a, b| {
        let ord = match sort {
            "new" | "old" => {
                let ya = a.get("firstPublishYear").and_then(|v| v.as_f64()).unwrap_or(f64::NEG_INFINITY);
                let yb = b.get("firstPublishYear").and_then(|v| v.as_f64()).unwrap_or(f64::NEG_INFINITY);
                if sort == "new" { yb.partial_cmp(&ya).unwrap_or(std::cmp::Ordering::Equal) } else { ya.partial_cmp(&yb).unwrap_or(std::cmp::Ordering::Equal) }
            }
            "title" => {
                let ta = a.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let tb = b.get("title").and_then(|v| v.as_str()).unwrap_or("");
                ta.to_lowercase().cmp(&tb.to_lowercase())
            }
            "rating" => {
                let ra = a.get("ratingAverage").and_then(|v| v.as_f64()).unwrap_or(f64::NEG_INFINITY);
                let rb = b.get("ratingAverage").and_then(|v| v.as_f64()).unwrap_or(f64::NEG_INFINITY);
                rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal)
            }
            _ => std::cmp::Ordering::Equal,
        };
        let ka = a.get("key").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("key").and_then(|v| v.as_str()).unwrap_or("");
        ord.then_with(|| ka.cmp(kb))
    });
}

/// Reads the three author shapes Open Library uses (works refs, inline
/// subjects authors, and search.json parallel arrays).
pub fn parse_author_refs(doc: &Value) -> Vec<Value> {
    let mut refs: Vec<Value> = Vec::new();

    if let Some(work_authors) = doc.get("authors").and_then(|v| v.as_array()) {
        for entry in work_authors {
            let key = parse_ol_id(entry.get("author").and_then(|a| a.get("key")));
            if let Some(key) = key {
                let role = parse_ol_id(entry.get("type").and_then(|t| t.get("key")));
                refs.push(json!({
                    "key": key,
                    "role": role,
                    "url": open_library_page_url(&format!("authors/{key}")),
                }));
            }
        }
        if !refs.is_empty() {
            return refs;
        }
        // subjects/{name}.json inline authors
        for entry in work_authors {
            let key = parse_ol_id(entry.get("key"));
            if let Some(key) = key {
                let name = as_string(entry.get("name"));
                let mut r = json!({
                    "key": key,
                    "url": open_library_page_url(&format!("authors/{key}")),
                    "photo": author_photo_url(&key, "M"),
                });
                if let Some(n) = name {
                    r["name"] = json!(n);
                }
                refs.push(r);
            }
        }
        if !refs.is_empty() {
            return refs;
        }
    }

    // search.json parallel arrays
    let names = as_string_array(doc.get("author_name"));
    let keys = as_string_array(doc.get("author_key"));
    let count = names.len().max(keys.len());
    for i in 0..count {
        let key = keys.get(i).cloned().unwrap_or_default();
        let mut r = serde_json::Map::new();
        r.insert("key".into(), json!(key));
        r.insert("url".into(), json!(if key.is_empty() { String::new() } else { open_library_page_url(&format!("authors/{key}")) }));
        if !key.is_empty() {
            r.insert("photo".into(), json!(author_photo_url(&key, "M")));
        }
        if let Some(name) = names.get(i) {
            r.insert("name".into(), json!(name));
        }
        if !key.is_empty() || names.get(i).is_some() {
            refs.push(Value::Object(r));
        }
    }
    refs
}

pub fn parse_author_summary(raw: &Value) -> Value {
    let key = parse_ol_id(raw.get("key")).unwrap_or_default();
    let name = as_string(raw.get("name"))
        .or_else(|| as_string(raw.get("personal_name")))
        .unwrap_or_else(|| key.clone());
    let photo = as_image_ids(raw.get("photos")).first().copied();

    let mut out = json!({
        "key": key,
        "name": name,
        "url": open_library_page_url(&format!("authors/{key}")),
    });
    if photo.is_some() {
        out["photo"] = json!(author_photo_url(&key, "M"));
    }
    if let Some(b) = as_string(raw.get("birth_date")) {
        out["birthDate"] = json!(b);
    }
    if let Some(d) = as_string(raw.get("death_date")) {
        out["deathDate"] = json!(d);
    }
    if let Some(bio) = as_text(raw.get("bio")) {
        out["bio"] = json!(bio);
    }
    out["alternateNames"] = json!(as_string_array(raw.get("alternate_names")));
    let top_links: Vec<Value> = raw
        .get("links")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| {
                    let url = as_string(l.get("url"))?;
                    let mut link = json!({ "url": url });
                    if let Some(t) = as_string(l.get("title")) {
                        link["title"] = json!(t);
                    }
                    Some(link)
                })
                .take(5)
                .collect()
        })
        .unwrap_or_default();
    out["topLinks"] = Value::Array(top_links);
    out
}

pub fn parse_work_summary(doc: &Value, fallback_title: &str) -> Value {
    let key = parse_ol_id(doc.get("key")).unwrap_or_default();
    let title = as_string(doc.get("title")).unwrap_or_else(|| fallback_title.to_string());
    let cover = to_cover_set(cover_id_from_doc(doc));

    let mut out = serde_json::Map::new();
    out.insert("key".into(), json!(key));
    out.insert("title".into(), json!(title));
    if let Some(s) = as_string(doc.get("subtitle")) {
        out.insert("subtitle".into(), json!(s));
    }
    out.insert("authors".into(), Value::Array(parse_author_refs(doc)));
    if let Some(y) = as_number(doc.get("first_publish_year")) {
        out.insert("firstPublishYear".into(), json!(y as i64));
    }
    if let Some(e) = as_number(doc.get("edition_count")) {
        out.insert("editionCount".into(), json!(e as i64));
    }
    if let Some(h) = as_bool(doc.get("has_fulltext")) {
        out.insert("hasFulltext".into(), json!(h));
    }
    out.insert("languages".into(), json!(as_string_array(doc.get("language"))));
    if let Some(c) = cover {
        out.insert("cover".into(), c);
    }
    if let Some(r) = as_number(doc.get("ratings_average")) {
        out.insert("ratingAverage".into(), json!(r));
    }
    if let Some(r) = as_number(doc.get("ratings_count")) {
        out.insert("ratingCount".into(), json!(r as i64));
    }
    out.insert(
        "url".into(),
        json!(if key.is_empty() {
            open_library_page_url("search")
        } else {
            open_library_page_url(&format!("works/{key}"))
        }),
    );
    Value::Object(out)
}

pub fn parse_work_detail(raw: &Value) -> Value {
    let mut out = match parse_work_summary(raw, "Untitled") {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    if let Some(d) = as_text(raw.get("description")) {
        out.insert("description".into(), json!(d));
    }
    if let Some(f) = as_text(raw.get("first_sentence")) {
        out.insert("firstSentence".into(), json!(f));
    }
    out.insert("subjects".into(), json!(as_string_array(raw.get("subjects"))));
    out.insert("subjectPeople".into(), json!(as_string_array(raw.get("subject_people"))));
    out.insert("subjectPlaces".into(), json!(as_string_array(raw.get("subject_places"))));
    out.insert("subjectTimes".into(), json!(as_string_array(raw.get("subject_times"))));
    let excerpts: Vec<Value> = raw
        .get("excerpts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let text = as_string(e.get("excerpt"))?;
                    let mut item = json!({ "excerpt": text });
                    if let Some(c) = as_string(e.get("comment")) {
                        item["comment"] = json!(c);
                    }
                    Some(item)
                })
                .collect()
        })
        .unwrap_or_default();
    out.insert("excerpts".into(), Value::Array(excerpts));
    let links: Vec<Value> = raw
        .get("links")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| {
                    let url = as_string(l.get("url"))?;
                    let mut item = json!({ "url": url });
                    if let Some(t) = as_string(l.get("title")) {
                        item["title"] = json!(t);
                    }
                    Some(item)
                })
                .collect()
        })
        .unwrap_or_default();
    out.insert("links".into(), Value::Array(links));
    if let Some(c) = as_datetime(raw.get("created")) {
        out.insert("createdAt".into(), json!(c));
    }
    if let Some(m) = as_datetime(raw.get("last_modified")) {
        out.insert("lastModifiedAt".into(), json!(m));
    }
    Value::Object(out)
}

pub fn parse_edition(raw: &Value) -> Value {
    let key = parse_ol_id(raw.get("key")).unwrap_or_default();
    let work_ref = raw
        .get("works")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter().find_map(|w| parse_ol_id(w.get("key")))
        });

    let mut out = serde_json::Map::new();
    out.insert("key".into(), json!(key));
    out.insert("title".into(), json!(as_string(raw.get("title")).unwrap_or_else(|| "Untitled".into())));
    if let Some(s) = as_string(raw.get("subtitle")) {
        out.insert("subtitle".into(), json!(s));
    }
    if let Some(e) = as_string(raw.get("edition_name")) {
        out.insert("editionName".into(), json!(e));
    }
    if let Some(w) = work_ref {
        out.insert("workKey".into(), json!(w));
    }
    out.insert("publishers".into(), json!(as_string_array(raw.get("publishers"))));
    if let Some(d) = as_string(raw.get("publish_date")) {
        out.insert("publishDate".into(), json!(d));
    }
    out.insert("publishPlaces".into(), json!(as_string_array(raw.get("publish_places"))));
    if let Some(p) = as_number(raw.get("number_of_pages")) {
        out.insert("numberOfPages".into(), json!(p as i64));
    }
    if let Some(p) = as_string(raw.get("pagination")) {
        out.insert("pagination".into(), json!(p));
    }
    if let Some(f) = as_string(raw.get("physical_format")) {
        out.insert("physicalFormat".into(), json!(f));
    }
    out.insert("languages".into(), json!(as_language_codes(raw.get("languages"))));
    if let Some(d) = as_text(raw.get("description")) {
        out.insert("description".into(), json!(d));
    }
    if let Some(c) = to_cover_set(as_image_ids(raw.get("covers")).first().copied()) {
        out.insert("cover".into(), c);
    }
    out.insert("isbn10".into(), json!(as_string_array(raw.get("isbn_10"))));
    out.insert("isbn13".into(), json!(as_string_array(raw.get("isbn_13"))));
    out.insert("oclcNumbers".into(), json!(as_string_array(raw.get("oclc_numbers"))));
    out.insert("lcClassifications".into(), json!(as_string_array(raw.get("lc_classifications"))));
    if let Some(l) = as_string(raw.get("lccn")) {
        out.insert("lccn".into(), json!(l));
    }
    if let Some(c) = as_string(raw.get("copyright_date")) {
        out.insert("copyrightDate".into(), json!(c));
    }
    if let Some(o) = as_string(raw.get("ocaid")) {
        out.insert("internetArchiveId".into(), json!(o));
    }
    // external identifiers
    let mut external = serde_json::Map::new();
    if let Some(Value::Object(ids)) = raw.get("identifiers") {
        for (scheme, list) in ids {
            let arr = as_string_array(Some(list));
            if !arr.is_empty() {
                external.insert(scheme.clone(), Value::Array(arr.into_iter().map(Value::String).collect()));
            }
        }
    }
    out.insert("externalIds".into(), Value::Object(external));
    if let Some(c) = as_datetime(raw.get("created")) {
        out.insert("createdAt".into(), json!(c));
    }
    if let Some(m) = as_datetime(raw.get("last_modified")) {
        out.insert("lastModifiedAt".into(), json!(m));
    }
    out.insert("url".into(), json!(open_library_page_url(&format!("books/{key}"))));
    Value::Object(out)
}

pub fn parse_search_response(raw: &Value, offset: usize, sort: &str) -> Result<(Option<i64>, usize, Vec<Value>), String> {
    let docs = raw
        .get("docs")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "Open Library search response has no \"docs\" array".to_string())?;
    let total = as_number(raw.get("numFound"))
        .or_else(|| as_number(raw.get("num_found")))
        .map(|n| n as i64);
    let mut works: Vec<Value> = docs
        .iter()
        .map(|doc| parse_work_summary(doc, "Untitled"))
        .filter(|w| w.get("key").and_then(|v| v.as_str()).map(|k| !k.is_empty()).unwrap_or(false))
        .collect();
    sort_works(&mut works, sort);
    Ok((total, offset, works))
}

pub fn parse_subject_response(raw: &Value, sort: &str) -> Result<Value, String> {
    let works_raw = raw
        .get("works")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "Open Library subject response has no \"works\" array".to_string())?;
    let name = as_string(raw.get("name"))
        .or_else(|| as_string(raw.get("key")).map(|k| k.trim_start_matches("/subjects/").to_string()))
        .unwrap_or_default();
    let mut works: Vec<Value> = works_raw
        .iter()
        .map(|doc| parse_work_summary(doc, "Untitled"))
        .filter(|w| w.get("key").and_then(|v| v.as_str()).map(|k| !k.is_empty()).unwrap_or(false))
        .collect();
    sort_works(&mut works, sort);

    let mut out = json!({ "subject": name, "works": works });
    if let Some(t) = as_string(raw.get("subject_type")) {
        out["subjectType"] = json!(t);
    }
    if let Some(c) = as_number(raw.get("work_count")) {
        out["workCount"] = json!(c as i64);
    }
    Ok(out)
}

/// `q` + Solr subject clause composition.
pub fn compose_query(q: Option<&str>, subject: Option<&str>) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(q) = q.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(q.to_string());
    }
    if let Some(s) = subject.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(format!("subject:{s}"));
    }
    if parts.is_empty() { None } else { Some(parts.join(" ")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ol_ids() {
        assert_eq!(parse_ol_id(Some(&json!("/works/OL45804W"))).unwrap(), "OL45804W");
        assert_eq!(parse_ol_id(Some(&json!("OL1W"))).unwrap(), "OL1W");
        assert!(parse_ol_id(Some(&json!(""))).is_none());
        assert!(is_work_key("OL45804W"));
        assert!(!is_work_key("OL45804A"));
        assert!(is_author_key("OL26320A"));
    }

    #[test]
    fn parses_work_summary_shapes() {
        let doc = json!({
            "key": "/works/OL45804W",
            "title": "The Lord of the Rings",
            "authors": [{ "author": { "key": "/authors/OL8079A" } }],
            "first_publish_year": 1954,
            "cover_i": 14627509,
            "ratings_average": 4.2
        });
        let work = parse_work_summary(&doc, "Untitled");
        assert_eq!(work["key"], "OL45804W");
        assert_eq!(work["authors"][0]["key"], "OL8079A");
        assert_eq!(work["cover"]["medium"], "https://covers.openlibrary.org/b/id/14627509-M.jpg");
        assert_eq!(work["firstPublishYear"], 1954);
    }

    #[test]
    fn composes_subject_clause() {
        assert_eq!(compose_query(Some("tolkien"), Some("fantasy")).unwrap(), "tolkien subject:fantasy");
        assert_eq!(compose_query(None, Some("poetry")).unwrap(), "subject:poetry");
        assert!(compose_query(None, None).is_none());
    }

    #[test]
    fn handles_description_object_form() {
        assert_eq!(as_text(Some(&json!({"value": "A great book"}))).unwrap(), "A great book");
        assert_eq!(as_text(Some(&json!("plain"))).unwrap(), "plain");
    }
}
