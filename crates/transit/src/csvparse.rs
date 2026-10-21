//! GTFS CSV reading on top of the `csv` crate, matching the original
//! `parsers/csv.ts` tolerances:
//!
//! - `route_long_name` and `stop_desc` routinely contain commas, so those
//!   fields are quoted (including `""` escapes and embedded CR/LF);
//! - records may be terminated by CRLF or bare LF;
//! - a UTF-8 BOM is common in agency-published archives;
//! - blank lines appear in Excel-produced feeds and are skipped;
//! - headers and values are trimmed (a padded `" 47.621263 "` would otherwise
//!   silently poison the stop index);
//! - ragged rows: missing keys become `""`, extra values are truncated to the
//!   header count.

use std::collections::HashMap;

/// A header-keyed CSV table, the equivalent of the original `CsvTable`.
#[derive(Debug, Clone, Default)]
pub struct CsvTable {
    pub headers: Vec<String>,
    pub records: Vec<HashMap<String, String>>,
}

fn builder() -> csv::ReaderBuilder {
    csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .trim(csv::Trim::All)
}

/// Parses `text` into a header-keyed table.
pub fn parse_csv(text: &str) -> CsvTable {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut reader = builder().from_reader(text.as_bytes());
    let headers: Vec<String> = reader
        .headers()
        .map(|headers| headers.iter().map(|name| name.trim().to_string()).collect())
        .unwrap_or_default();
    let mut records = Vec::new();
    for record in reader.records() {
        let Ok(record) = record else { continue };
        let mut row: HashMap<String, String> = HashMap::with_capacity(headers.len());
        // Records longer than the header list are truncated; records shorter
        // than it leave the missing keys empty.
        for (column, header) in headers.iter().enumerate() {
            row.insert(header.clone(), record.get(column).unwrap_or("").trim().to_string());
        }
        records.push(row);
    }
    CsvTable { headers, records }
}

/// A column value from a parsed row; missing keys read as `""`.
pub fn cell<'a>(row: &'a HashMap<String, String>, column: &str) -> &'a str {
    row.get(column).map(String::as_str).unwrap_or("")
}

/// True when the row carries a non-empty value in `column`.
pub fn has_value(row: &HashMap<String, String>, column: &str) -> bool {
    !cell(row, column).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quoted_fields_with_commas_escapes_and_newlines() {
        let table = parse_csv(
            "route_id,route_long_name\nR1,\"Alpha Line, Main St to Grand\"\nR2,\"Bravo \"\"Express\"\" & Coaches\"\n",
        );
        assert_eq!(table.headers, vec!["route_id", "route_long_name"]);
        assert_eq!(table.records.len(), 2);
        assert_eq!(
            table.records[0].get("route_long_name").map(String::as_str),
            Some("Alpha Line, Main St to Grand")
        );
        assert_eq!(
            table.records[1].get("route_long_name").map(String::as_str),
            Some("Bravo \"Express\" & Coaches")
        );
    }

    #[test]
    fn tolerates_bom_blank_lines_crlf_and_ragged_rows() {
        let table = parse_csv("\u{feff}stop_id,stop_name\r\nS1,North\r\n\r\nS2\r\n");
        assert_eq!(table.records.len(), 2, "the blank line is skipped");
        assert_eq!(table.records[0].get("stop_id").map(String::as_str), Some("S1"));
        assert_eq!(table.records[1].get("stop_id").map(String::as_str), Some("S2"));
        assert_eq!(
            table.records[1].get("stop_name").map(String::as_str),
            Some(""),
            "missing keys are empty strings"
        );
    }

    #[test]
    fn trims_padded_headers_and_values() {
        let table = parse_csv("stop_id , stop_lat\nS1 ,  47.621263 \n");
        assert_eq!(table.headers, vec!["stop_id", "stop_lat"]);
        assert_eq!(table.records[0].get("stop_lat").map(String::as_str), Some("47.621263"));
    }

    #[test]
    fn quoted_field_may_span_lines_and_truncate_extra_columns() {
        let table = parse_csv("stop_id,stop_desc\nS1,\"Serves\nlines A and B\",extra\n");
        assert_eq!(table.records.len(), 1);
        assert_eq!(
            table.records[0].get("stop_desc").map(String::as_str),
            Some("Serves\nlines A and B")
        );
        assert!(table.records[0].get("extra").is_none(), "columns beyond the header are dropped");
        assert_eq!(cell(&table.records[0], "missing"), "");
        assert!(!has_value(&table.records[0], "missing"));
    }
}
