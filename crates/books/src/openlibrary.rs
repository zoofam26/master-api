//! Endpoint construction and pure URL helpers for Open Library.
//!
//! Ported 1:1 from `book-api/src/openlibrary/openlibrary.urls.ts` — keeping
//! every upstream path in one module means a change to their URL scheme cannot
//! leak into the handlers.

use crate::CODE_PREFIX;

pub const OPEN_LIBRARY_BASE: &str = "https://openlibrary.org";
pub const COVER_BASE: &str = "https://covers.openlibrary.org";

/// Sort facets accepted by `search.json` (verified live upstream; the broken
/// and non-deterministic facets the original excluded stay excluded).
pub const SEARCH_SORTS: &[&str] = &["relevance", "new", "old", "title", "rating", "key"];

/// Fields requested from `search.json`; the projection is always explicit
/// because `fields=*` is an expensive response upstream.
pub const SEARCH_FIELDS: &[&str] = &[
    "key",
    "title",
    "subtitle",
    "author_name",
    "author_key",
    "first_publish_year",
    "edition_count",
    "has_fulltext",
    "cover_i",
    "language",
    "ratings_average",
    "ratings_count",
];

/// Maps the public sort vocabulary onto what `search.json` accepts.
/// `relevance` maps to `None`: the upstream default is the relevance ranking.
pub fn upstream_sort(sort: &str) -> Option<&'static str> {
    match sort {
        "new" => Some("new"),
        "old" => Some("old"),
        "title" => Some("title"),
        "rating" => Some("rating"),
        "key" => Some("key"),
        _ => None,
    }
}

/// `encodeURIComponent` semantics: unreserved ASCII plus `!`, `'`, `(`, `)`,
/// `*`; everything else percent-encoded.
pub fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'!'
            | b'*' | b'\'' | b'(' | b')' => out.push(*byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Parameters accepted by [`search_url`].
#[derive(Debug, Default, Clone)]
pub struct SearchParams {
    pub q: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub publisher: Option<String>,
    pub language: Option<String>,
    pub has_fulltext: Option<bool>,
    pub sort: Option<String>,
    pub limit: usize,
    pub offset: usize,
}

pub fn search_url(params: &SearchParams) -> String {
    let mut parts: Vec<String> = Vec::new();
    // `q` is omitted rather than sent empty when the caller filters by
    // title/author instead.
    if let Some(q) = &params.q {
        parts.push(format!("q={}", encode_component(q)));
    }
    if let Some(title) = &params.title {
        parts.push(format!("title={}", encode_component(title)));
    }
    if let Some(author) = &params.author {
        parts.push(format!("author={}", encode_component(author)));
    }
    if let Some(publisher) = &params.publisher {
        parts.push(format!("publisher={}", encode_component(publisher)));
    }
    if let Some(language) = &params.language {
        parts.push(format!("language={}", encode_component(language)));
    }
    if let Some(has_fulltext) = params.has_fulltext {
        parts.push(format!(
            "has_fulltext={}",
            if has_fulltext { "true" } else { "false" }
        ));
    }
    if let Some(sort) = params.sort.as_deref().and_then(upstream_sort) {
        parts.push(format!("sort={sort}"));
    }
    parts.push(format!("limit={}", params.limit));
    // `offset` is the only pagination parameter the endpoint honours.
    parts.push(format!("offset={}", params.offset));
    parts.push(format!("fields={}", SEARCH_FIELDS.join(",")));
    format!("{OPEN_LIBRARY_BASE}/search.json?{}", parts.join("&"))
}

pub fn work_url(key: &str) -> String {
    format!("{OPEN_LIBRARY_BASE}/works/{}.json", encode_component(key))
}

pub fn author_url(key: &str) -> String {
    format!("{OPEN_LIBRARY_BASE}/authors/{}.json", encode_component(key))
}

/// `/isbn/{isbn}.json` 302-redirects to the concrete edition record.
pub fn isbn_url(isbn: &str) -> String {
    format!("{OPEN_LIBRARY_BASE}/isbn/{}.json", encode_component(isbn))
}

pub fn subject_url(name: &str, limit: usize, offset: usize, sort: Option<&str>) -> String {
    let mut parts = vec![
        format!("limit={limit}"),
        format!("offset={offset}"),
    ];
    if let Some(sort) = sort {
        parts.push(format!("sort={}", encode_component(sort)));
    }
    format!(
        "{OPEN_LIBRARY_BASE}/subjects/{}.json?{}",
        encode_component(name),
        parts.join("&")
    )
}

/// Cover URLs are built from the CoverID only — the ISBN-keyed form is
/// rate-limited by upstream and never emitted.
pub fn cover_url(cover_id: i64, size: &str) -> String {
    format!("{COVER_BASE}/b/id/{cover_id}-{size}.jpg")
}

/// Author photographs are addressable by OLID, which is un-rate-limited.
pub fn author_photo_url(author_key: &str, size: &str) -> String {
    format!(
        "{COVER_BASE}/a/olid/{}-{size}.jpg",
        encode_component(author_key)
    )
}

pub fn open_library_page_url(path: &str) -> String {
    format!("{OPEN_LIBRARY_BASE}/{}", path.trim_start_matches('/'))
}

/// Merges a `subject` filter into the free-text query as a Solr clause.
/// Ported from `composeQuery` in books.service.ts.
pub fn compose_query(q: Option<&str>, subject: Option<&str>) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(q) = q {
        let trimmed = q.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }
    if let Some(subject) = subject {
        let trimmed = subject.trim();
        if !trimmed.is_empty() {
            parts.push(format!("subject:{trimmed}"));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// Service-code helper so callers never have to spell the prefix out.
pub fn code(suffix: &str) -> String {
    format!("{CODE_PREFIX}_{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_url_builds_the_upstream_query_in_documented_order() {
        let url = search_url(&SearchParams {
            q: Some("subject:fantasy tolkien".into()),
            title: None,
            author: Some("Ursula K. Le Guin".into()),
            publisher: None,
            language: Some("eng".into()),
            has_fulltext: Some(true),
            sort: Some("new".into()),
            limit: 20,
            offset: 40,
        });
        assert!(url.starts_with("https://openlibrary.org/search.json?"));
        assert!(url.contains("q=subject%3Afantasy%20tolkien"));
        assert!(url.contains("author=Ursula%20K.%20Le%20Guin"));
        assert!(url.contains("language=eng"));
        assert!(url.contains("has_fulltext=true"));
        assert!(url.contains("sort=new"));
        assert!(url.contains("limit=20"));
        assert!(url.contains("offset=40"));
        assert!(url.ends_with(&format!("fields={}", SEARCH_FIELDS.join(","))));
    }

    #[test]
    fn relevance_sort_is_omitted_and_empty_filters_are_dropped() {
        let url = search_url(&SearchParams {
            q: None,
            sort: Some("relevance".into()),
            limit: 1,
            offset: 0,
            ..Default::default()
        });
        assert!(!url.contains("q="));
        assert!(!url.contains("sort="));
        assert!(url.contains("limit=1&offset=0"));
    }

    #[test]
    fn record_urls_encode_their_segment() {
        assert_eq!(work_url("OL45804W"), "https://openlibrary.org/works/OL45804W.json");
        assert_eq!(author_url("OL26320A"), "https://openlibrary.org/authors/OL26320A.json");
        assert_eq!(
            isbn_url("978-0-306-40615-7"),
            "https://openlibrary.org/isbn/978-0-306-40615-7.json"
        );
        assert_eq!(
            subject_url("science fiction", 10, 20, Some("new")),
            "https://openlibrary.org/subjects/science%20fiction.json?limit=10&offset=20&sort=new"
        );
    }

    #[test]
    fn cover_and_photo_urls_follow_the_original_builder() {
        assert_eq!(cover_url(1234, "M"), "https://covers.openlibrary.org/b/id/1234-M.jpg");
        assert_eq!(
            author_photo_url("OL26320A", "M"),
            "https://covers.openlibrary.org/a/olid/OL26320A-M.jpg"
        );
        assert_eq!(
            open_library_page_url("/works/OL45804W"),
            "https://openlibrary.org/works/OL45804W"
        );
    }

    #[test]
    fn compose_query_merges_subject_as_a_solr_clause() {
        assert_eq!(compose_query(Some(" tolkien "), None).as_deref(), Some("tolkien"));
        assert_eq!(
            compose_query(Some("tolkien"), Some(" fantasy ")).as_deref(),
            Some("tolkien subject:fantasy")
        );
        assert_eq!(compose_query(None, Some("  ")), None);
        assert_eq!(compose_query(None, None), None);
    }
}
