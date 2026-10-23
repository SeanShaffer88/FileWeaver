//! Import of explicit current-name → proposed-name mappings from CSV.
//!
//! The format is deliberately the same one the preview's CSV export writes
//! (`current_name,proposed_name`), so a reviewed list can be edited in a
//! spreadsheet and imported back. Parsing is tolerant about presentation
//! (a header row is optional, extra columns are ignored, quoted fields may
//! contain commas) and strict about meaning: duplicate or incomplete rows
//! are reported as problems instead of being silently dropped or guessed.

use serde_json::json;
use thiserror::Error;

/// One usable `current_name,proposed_name` pair from a mapping file.
pub type MappingEntry = (String, String);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MappingImport {
    /// Usable pairs, in file order. A name appears at most once: the first
    /// row wins and later rows for the same name become problems.
    pub entries: Vec<MappingEntry>,
    /// Human-readable problems for rows that were skipped, in file order.
    pub problems: Vec<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MappingError {
    #[error("the mapping file is empty")]
    Empty,
    #[error("the mapping file contained no usable current_name,proposed_name rows")]
    NoUsableEntries,
}

/// Parses mapping CSV text.
///
/// A first row naming `current_name` and `proposed_name` columns (in any
/// position, extra columns ignored) is treated as a header; without one,
/// every row is data and the first two columns are used. Blank rows are
/// skipped. Rows that cannot be used are collected in
/// [`MappingImport::problems`] with their 1-based row number.
pub fn parse_mapping_csv(text: &str) -> Result<MappingImport, MappingError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if text.trim().is_empty() {
        return Err(MappingError::Empty);
    }
    let records = parse_csv_records(text);
    let mut import = MappingImport::default();
    let mut first_row = true;
    let mut columns: Option<(usize, usize)> = None;

    for (offset, record) in records.iter().enumerate() {
        let row_number = offset + 1;
        if record.iter().all(|field| field.trim().is_empty()) {
            continue;
        }
        if first_row {
            first_row = false;
            let lowered: Vec<String> = record
                .iter()
                .map(|field| field.trim().to_lowercase())
                .collect();
            if let (Some(current), Some(proposed)) = (
                lowered.iter().position(|name| name == "current_name"),
                lowered.iter().position(|name| name == "proposed_name"),
            ) {
                columns = Some((current, proposed));
                continue;
            }
        }
        let (current_at, proposed_at) = columns.unwrap_or((0, 1));
        let needed = current_at.max(proposed_at) + 1;
        if record.len() < needed {
            import.problems.push(format!(
                "Row {row_number}: expected at least {needed} columns, found {} — skipped.",
                record.len()
            ));
            continue;
        }
        let current = record[current_at].trim().to_string();
        let proposed = record[proposed_at].trim().to_string();
        if current.is_empty() || proposed.is_empty() {
            import.problems.push(format!(
                "Row {row_number}: current and proposed names must both be filled in — skipped."
            ));
            continue;
        }
        if import
            .entries
            .iter()
            .any(|(existing, _)| existing == &current)
        {
            import.problems.push(format!(
                "Row {row_number}: \"{current}\" already has a mapping; the first one wins — skipped."
            ));
            continue;
        }
        import.entries.push((current, proposed));
    }

    if import.entries.is_empty() {
        return Err(MappingError::NoUsableEntries);
    }
    Ok(import)
}

/// Deterministic marker stored as a mapping plan's `pipeline_json`, so the
/// shell can tell "the rules changed" apart from "this plan came from a
/// mapping file" and can verify the plan still matches that file's entries.
pub fn mapping_pipeline_marker(entries: &[MappingEntry]) -> String {
    serde_json::to_string(&json!({
        "filenameweaver-mapping": 1,
        "entries": entries,
    }))
    .unwrap_or_default()
}

/// Minimal RFC 4180 reader: quoted fields may contain commas, quotes
/// (doubled), and newlines; both LF and CRLF line endings are accepted.
fn parse_csv_records(text: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut in_quotes = false;
    let mut field_started = false;

    macro_rules! end_field {
        () => {{
            record.push(std::mem::take(&mut field));
            field_started = false;
        }};
    }
    macro_rules! end_record {
        () => {{
            end_field!();
            records.push(std::mem::take(&mut record));
        }};
    }

    while let Some(character) = chars.next() {
        if in_quotes {
            if character == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(character);
            }
            continue;
        }
        match character {
            '"' if !field_started => {
                in_quotes = true;
                field_started = true;
            }
            ',' => end_field!(),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                end_record!();
            }
            '\n' => end_record!(),
            _ => {
                field.push(character);
                field_started = true;
            }
        }
    }
    if field_started || !field.is_empty() || !record.is_empty() {
        end_record!();
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_export_format_with_a_header() {
        let import = parse_mapping_csv(
            "included,current_name,proposed_name,status,issues\nyes,a.txt,b.txt,Ready,\n",
        )
        .expect("export-shaped CSV parses");
        assert_eq!(
            import.entries,
            vec![("a.txt".to_string(), "b.txt".to_string())]
        );
        assert!(import.problems.is_empty());
    }

    #[test]
    fn header_columns_can_be_in_any_position() {
        let import = parse_mapping_csv("proposed_name,notes,current_name\nnew.txt,x,old.txt\n")
            .expect("reordered header parses");
        assert_eq!(
            import.entries,
            vec![("old.txt".to_string(), "new.txt".to_string())]
        );
    }

    #[test]
    fn headerless_rows_use_the_first_two_columns() {
        let import = parse_mapping_csv("a.txt,b.txt\nc.txt,d.txt\n").expect("pairs parse");
        assert_eq!(import.entries.len(), 2);
    }

    #[test]
    fn quoted_fields_keep_commas_and_quotes() {
        let import = parse_mapping_csv("\"a, one.txt\",\"say \"\"hi\"\".txt\"\n")
            .expect("quoted fields parse");
        assert_eq!(
            import.entries,
            vec![("a, one.txt".to_string(), "say \"hi\".txt".to_string())]
        );
    }

    #[test]
    fn bom_crlf_and_blank_rows_are_tolerated() {
        let import = parse_mapping_csv("\u{feff}current_name,proposed_name\r\n\r\na.txt,b.txt\r\n")
            .expect("BOM/CRLF parse");
        assert_eq!(import.entries.len(), 1);
    }

    #[test]
    fn duplicate_and_incomplete_rows_become_problems_not_guesses() {
        let import = parse_mapping_csv("a.txt,b.txt\na.txt,c.txt\nonly-one-column\n,d.txt\n")
            .expect("first mapping still usable");
        assert_eq!(
            import.entries,
            vec![("a.txt".to_string(), "b.txt".to_string())]
        );
        assert_eq!(import.problems.len(), 3);
        assert!(import.problems[0].contains("Row 2"));
        assert!(import.problems[1].contains("Row 3"));
        assert!(import.problems[2].contains("Row 4"));
    }

    #[test]
    fn empty_and_unusable_files_are_errors() {
        assert_eq!(parse_mapping_csv("  \n"), Err(MappingError::Empty));
        assert_eq!(
            parse_mapping_csv("current_name,proposed_name\n"),
            Err(MappingError::NoUsableEntries)
        );
    }

    #[test]
    fn marker_is_deterministic_and_entry_sensitive() {
        let entries = vec![("a.txt".to_string(), "b.txt".to_string())];
        assert_eq!(
            mapping_pipeline_marker(&entries),
            mapping_pipeline_marker(&entries)
        );
        let changed = vec![("a.txt".to_string(), "c.txt".to_string())];
        assert_ne!(
            mapping_pipeline_marker(&entries),
            mapping_pipeline_marker(&changed)
        );
    }
}
