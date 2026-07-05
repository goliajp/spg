//! v7.22 (mailrs round-13 / T2) — shared COPY text-format helpers.
//!
//! PG's COPY is not an engine statement in SPG: both consumers
//! lower it to per-row INSERTs. The wire path (spg-server pgwire)
//! has done this since v7.15 for `COPY … FROM stdin` CopyData
//! frames; the embed path (`Database::execute_script` /
//! `spg import`) gained it in v7.22 because **default-format
//! pg_dump emits COPY blocks**, and the zero-change import promise
//! covers the default format, not just `--column-inserts`.
//!
//! This module is the single home for the pure pieces: text-row
//! decoding (tab-separated, `\N` nulls, backslash escapes) and
//! INSERT synthesis. The wire path delegates here; wire-specific
//! concerns (CopyData framing, SKIP/ON_ERROR/JSON options) stay in
//! pgwire.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// The head of an embed-path `COPY … FROM stdin;` statement.
#[derive(Debug, PartialEq, Eq)]
pub struct CopyFromSpec {
    /// Bare table name (any `schema.` qualifier stripped — same
    /// treatment the SQL parser gives table names).
    pub table: String,
    /// Explicit column list when the statement carries one
    /// (pg_dump always emits it). `None` = positional against the
    /// table's full column order.
    pub columns: Option<Vec<String>>,
}

/// Parse the head of a `COPY <table> [(cols)] FROM stdin` statement
/// (text format). Returns `None` when the statement is not that
/// shape — including `COPY … TO stdout` and file endpoints. A
/// trailing `WITH (…)` options tail is accepted and ignored except
/// that a non-text `FORMAT` makes this return `None` (the embed
/// path only lowers the text format; callers surface a clear
/// error).
#[must_use]
pub fn parse_copy_from_stdin_head(sql: &str) -> Option<CopyFromSpec> {
    let trimmed = sql.trim();
    let lower = trimmed.to_ascii_lowercase();
    let rest = lower.strip_prefix("copy")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest_orig = &trimmed[trimmed.len() - rest.len()..];
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    // Table name: read to whitespace or '('.
    let t0 = i;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'(' {
        i += 1;
    }
    if i == t0 {
        return None;
    }
    let raw_table = &rest_orig[t0..i];
    let table = match raw_table.rsplit_once('.') {
        Some((_, bare)) => bare,
        None => raw_table,
    }
    .trim_matches('"')
    .to_string();
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    // Optional column list.
    let mut columns = None;
    if bytes.get(i) == Some(&b'(') {
        let cols_start = i + 1;
        let mut depth = 1usize;
        i += 1;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        let cols_str = &rest_orig[cols_start..i.saturating_sub(1)];
        columns = Some(
            cols_str
                .split(',')
                .map(|c| c.trim().trim_matches('"').to_string())
                .filter(|c| !c.is_empty())
                .collect::<Vec<_>>(),
        );
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
    }
    // `FROM stdin` (case-folded via `lower`).
    let tail = &rest[i..];
    let tail = tail.trim_start();
    let tail = tail.strip_prefix("from")?;
    if !tail.starts_with(char::is_whitespace) {
        return None;
    }
    let tail = tail.trim_start();
    if !(tail == "stdin" || tail.starts_with("stdin")) {
        return None;
    }
    let after = tail["stdin".len()..].trim();
    // Options tail: only the default text format lowers here.
    if after.contains("format") && !after.contains("text") {
        return None;
    }
    Some(CopyFromSpec { table, columns })
}

/// Decode one COPY text-format data row: tab-separated cells,
/// `\N` = NULL, C-style backslash escapes.
#[must_use]
pub fn decode_copy_text_row(line: &str) -> Vec<Option<String>> {
    line.split('\t')
        .map(|cell| {
            if cell == "\\N" {
                None
            } else {
                let mut out = String::with_capacity(cell.len());
                let mut chars = cell.chars();
                while let Some(c) = chars.next() {
                    if c == '\\'
                        && let Some(n) = chars.next()
                    {
                        out.push(match n {
                            'b' => '\u{08}',
                            'f' => '\u{0c}',
                            'n' => '\n',
                            'r' => '\r',
                            't' => '\t',
                            'v' => '\u{0b}',
                            '\\' => '\\',
                            other => other,
                        });
                    } else {
                        out.push(c);
                    }
                }
                Some(out)
            }
        })
        .collect()
}

/// Decode one CSV data record (`COPY … FROM stdin WITH (FORMAT csv)`)
/// into its fields. A field that starts with the quote character is a
/// quoted field: its content runs to the matching close quote, a
/// doubled quote (`""`) is one literal quote, and it is never NULL — a
/// quoted empty string stays `Some("")`. An unquoted field runs to the
/// next delimiter; if its text equals `null_str` it decodes to NULL, so
/// with the default empty null string an empty *unquoted* field is NULL
/// while `""` is the empty string (PG's exact CSV distinction). Embedded
/// delimiters and newlines are only meaningful inside quotes.
#[must_use]
pub fn decode_copy_csv_record(
    record: &str,
    delimiter: char,
    quote: char,
    null_str: &str,
) -> Vec<Option<String>> {
    let chars: Vec<char> = record.chars().collect();
    let n = chars.len();
    let mut fields: Vec<Option<String>> = Vec::new();
    let mut i = 0;
    loop {
        if i < n && chars[i] == quote {
            // Quoted field: read to the matching close quote.
            i += 1;
            let mut content = String::new();
            while i < n {
                let c = chars[i];
                if c == quote {
                    if i + 1 < n && chars[i + 1] == quote {
                        content.push(quote);
                        i += 2;
                    } else {
                        i += 1; // closing quote
                        break;
                    }
                } else {
                    content.push(c);
                    i += 1;
                }
            }
            fields.push(Some(content));
            // Skip any characters between the close quote and the next
            // delimiter (PG rejects them; we are lenient).
            while i < n && chars[i] != delimiter {
                i += 1;
            }
        } else {
            // Unquoted field: read to the next delimiter.
            let start = i;
            while i < n && chars[i] != delimiter {
                i += 1;
            }
            let content: String = chars[start..i].iter().collect();
            fields.push(if content == null_str {
                None
            } else {
                Some(content)
            });
        }
        if i < n && chars[i] == delimiter {
            i += 1; // step over the delimiter, parse the next field
        } else {
            break;
        }
    }
    fields
}

/// Byte length of the first complete CSV record in `buf` — including its
/// terminating `\n` — or `None` if the buffer does not yet hold a full
/// record (an unterminated quoted field, or no record-ending newline
/// yet). Quote-aware: a newline inside a quoted field is part of the
/// record. The quote character only opens a quoted field at the start of
/// a field (buffer start or right after a delimiter), so `delimiter` is
/// needed to track field boundaries. Scanning raw bytes is UTF-8-safe
/// because the ASCII delimiter / quote / newline never collide with a
/// multi-byte continuation byte (which is always ≥ 0x80).
#[must_use]
pub fn csv_record_end(buf: &[u8], delimiter: u8, quote: u8) -> Option<usize> {
    let mut in_quote = false;
    let mut at_field_start = true;
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        if in_quote {
            if b == quote {
                if buf.get(i + 1) == Some(&quote) {
                    i += 2; // escaped quote, still inside the field
                    continue;
                }
                in_quote = false; // closing quote
            }
            // Any other byte (including '\n') stays inside the field.
        } else if b == quote && at_field_start {
            in_quote = true;
            at_field_start = false;
        } else if b == b'\n' {
            return Some(i + 1);
        } else if b == delimiter {
            at_field_start = true;
        } else {
            at_field_start = false;
        }
        i += 1;
    }
    None
}

/// Build `INSERT INTO <table> [(cols)] VALUES (…)` from a decoded
/// row. Numeric-looking and boolean cells go in bare so the engine
/// sees typed literals; everything else is single-quoted with SQL
/// escaping.
#[must_use]
pub fn build_copy_insert(
    table: &str,
    columns: Option<&[String]>,
    values: &[Option<String>],
) -> String {
    let mut sql = format!("INSERT INTO {table} ");
    if let Some(cols) = columns {
        sql.push('(');
        for (i, c) in cols.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str(c);
        }
        sql.push_str(") ");
    }
    sql.push_str("VALUES (");
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        match v {
            None => sql.push_str("NULL"),
            Some(s) => {
                if copy_cell_looks_numeric(s)
                    || matches!(s.as_str(), "true" | "false" | "TRUE" | "FALSE")
                {
                    sql.push_str(s);
                } else {
                    sql.push('\'');
                    for ch in s.chars() {
                        if ch == '\'' {
                            sql.push('\'');
                        }
                        sql.push(ch);
                    }
                    sql.push('\'');
                }
            }
        }
    }
    sql.push(')');
    sql
}

/// True when the cell can ride into the INSERT as a bare numeric
/// literal. Deliberately conservative — anything ambiguous goes
/// quoted and lets column-type coercion decide.
fn copy_cell_looks_numeric(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let b = s.as_bytes();
    let mut i = 0;
    if b[0] == b'-' || b[0] == b'+' {
        if b.len() == 1 {
            return false;
        }
        i = 1;
    }
    let mut seen_dot = false;
    let mut seen_digit = false;
    while i < b.len() {
        match b[i] {
            b'0'..=b'9' => seen_digit = true,
            b'.' if !seen_dot => seen_dot = true,
            _ => return false,
        }
        i += 1;
    }
    // Leading-zero integers ("0042") stay quoted: they're usually
    // identifiers/codes, and PG would render them back differently.
    if !seen_dot && s.trim_start_matches(['-', '+']).len() > 1 {
        let digits = s.trim_start_matches(['-', '+']);
        if digits.starts_with('0') {
            return false;
        }
    }
    seen_digit
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    #[test]
    fn parses_pg_dump_copy_head() {
        let spec =
            parse_copy_from_stdin_head("COPY public.messages (id, subject, body) FROM stdin")
                .unwrap();
        assert_eq!(spec.table, "messages");
        assert_eq!(
            spec.columns.as_deref(),
            Some(&["id".to_string(), "subject".to_string(), "body".to_string()][..])
        );
        // No column list.
        let bare = parse_copy_from_stdin_head("copy t from stdin").unwrap();
        assert_eq!(bare.table, "t");
        assert_eq!(bare.columns, None);
        // Not the embed shape.
        assert!(parse_copy_from_stdin_head("COPY t TO stdout").is_none());
        assert!(parse_copy_from_stdin_head("COPY t FROM '/tmp/f.csv'").is_none());
        assert!(parse_copy_from_stdin_head("COPY t FROM stdin WITH (FORMAT csv)").is_none());
    }

    #[test]
    fn decodes_text_rows() {
        assert_eq!(
            decode_copy_text_row("1\thello\t\\N\ta\\tb"),
            vec![
                Some("1".to_string()),
                Some("hello".to_string()),
                None,
                Some("a\tb".to_string())
            ]
        );
    }

    #[test]
    fn builds_inserts_with_column_list() {
        let cols = vec!["id".to_string(), "note".to_string()];
        let row = vec![Some("7".to_string()), Some("it's".to_string())];
        assert_eq!(
            build_copy_insert("t", Some(&cols), &row),
            "INSERT INTO t (id, note) VALUES (7, 'it''s')"
        );
        assert_eq!(
            build_copy_insert("t", None, &[None, Some("0042".to_string())]),
            "INSERT INTO t VALUES (NULL, '0042')"
        );
    }

    fn csv(record: &str) -> Vec<Option<String>> {
        decode_copy_csv_record(record, ',', '"', "")
    }

    #[test]
    fn decodes_csv_quoting_and_null() {
        // Quoted field with embedded delimiter + doubled quote; PG18.4.
        assert_eq!(
            csv("p,\"x,y\",\"a\"\"b\""),
            vec![
                Some("p".to_string()),
                Some("x,y".to_string()),
                Some("a\"b".to_string()),
            ]
        );
        // Spaces preserved; trailing empty *unquoted* field → NULL.
        assert_eq!(
            csv("q, spaced ,"),
            vec![Some("q".to_string()), Some(" spaced ".to_string()), None]
        );
        // Empty unquoted → NULL; empty quoted → "" (the CSV distinction).
        assert_eq!(csv(",\"\""), vec![None, Some(String::new())]);
        // A quoted field may hold a newline (the record spans lines).
        assert_eq!(
            csv("\"line\nbreak\",r"),
            vec![Some("line\nbreak".to_string()), Some("r".to_string())]
        );
    }

    #[test]
    fn decodes_csv_custom_delimiter_quote_and_null() {
        assert_eq!(
            decode_copy_csv_record("1;#a;b#;NULO", ';', '#', "NULO"),
            vec![Some("1".to_string()), Some("a;b".to_string()), None]
        );
    }

    #[test]
    fn csv_record_end_is_quote_aware() {
        // A newline outside quotes ends the record (length includes it).
        assert_eq!(csv_record_end(b"a,b\nrest", b',', b'"'), Some(4));
        // A newline *inside* a quoted field does not end the record; the
        // record ends at the newline after the closing quote.
        assert_eq!(csv_record_end(b"a,\"x\ny\"\nnext", b',', b'"'), Some(8));
        // A quoted field is only opened at a field start (after a
        // delimiter): the second field's quote must be honoured.
        assert_eq!(csv_record_end(b"1,\"p\nq\"\n", b',', b'"'), Some(8));
        // Doubled quote inside a quoted field stays inside.
        assert_eq!(csv_record_end(b"\"a\"\"b\"\nx", b',', b'"'), Some(7));
        // Incomplete: unterminated quote → need more bytes.
        assert_eq!(csv_record_end(b"\"unterminated\n", b',', b'"'), None);
        // Incomplete: no newline yet.
        assert_eq!(csv_record_end(b"a,b", b',', b'"'), None);
    }
}

/// Encode one row's selected cells as a COPY text-format line —
/// the inverse of [`decode_copy_text_row`]: tab-separated, `\N`
/// for NULL, C-style backslash escapes for the control characters
/// the decoder understands.
#[must_use]
pub fn encode_copy_text_cells(cells: &[Option<String>]) -> String {
    encode_copy_text_cells_opts(cells, '\t', "\\N")
}

/// Encode one row's cells as a COPY text-format line with a custom
/// delimiter and NULL marker (PG `COPY … WITH (FORMAT text, DELIMITER
/// 'c', NULL 'str')`). The named C-escapes (`\t \n \r \b \f \v \\`) are
/// always applied; a delimiter character that is not itself one of those
/// gets a literal `\<char>` escape so it round-trips.
#[must_use]
pub fn encode_copy_text_cells_opts(
    cells: &[Option<String>],
    delimiter: char,
    null_str: &str,
) -> String {
    let mut out = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push(delimiter);
        }
        match cell {
            None => out.push_str(null_str),
            Some(s) => {
                for c in s.chars() {
                    match c {
                        '\\' => out.push_str("\\\\"),
                        '\t' => out.push_str("\\t"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\u{08}' => out.push_str("\\b"),
                        '\u{0c}' => out.push_str("\\f"),
                        '\u{0b}' => out.push_str("\\v"),
                        other if other == delimiter => {
                            out.push('\\');
                            out.push(other);
                        }
                        other => out.push(other),
                    }
                }
            }
        }
    }
    out
}

/// Encode one row's cells as a CSV line (PG `COPY … WITH (FORMAT csv)`).
/// A non-NULL field is quoted when it contains the delimiter, the quote
/// character, a CR or LF, or when its text equals `null_str` — so an
/// empty string under the default empty NULL, or any value that collides
/// with the NULL marker, reads back as itself rather than as NULL. The
/// quote character is doubled inside a quoted field. NULL is emitted as
/// `null_str`, unquoted.
#[must_use]
pub fn encode_copy_csv_cells(
    cells: &[Option<String>],
    delimiter: char,
    quote: char,
    null_str: &str,
) -> String {
    let mut out = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push(delimiter);
        }
        match cell {
            None => out.push_str(null_str),
            Some(s) => {
                let needs_quote = s.as_str() == null_str
                    || s.chars().any(|c| {
                        c == delimiter || c == quote || c == '\n' || c == '\r'
                    });
                if needs_quote {
                    out.push(quote);
                    for c in s.chars() {
                        if c == quote {
                            out.push(quote);
                        }
                        out.push(c);
                    }
                    out.push(quote);
                } else {
                    out.push_str(s);
                }
            }
        }
    }
    out
}
