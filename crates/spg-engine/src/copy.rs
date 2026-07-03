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
}

/// Encode one row's selected cells as a COPY text-format line —
/// the inverse of [`decode_copy_text_row`]: tab-separated, `\N`
/// for NULL, C-style backslash escapes for the control characters
/// the decoder understands.
#[must_use]
pub fn encode_copy_text_cells(cells: &[Option<String>]) -> String {
    let mut out = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push('\t');
        }
        match cell {
            None => out.push_str("\\N"),
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
                        other => out.push(other),
                    }
                }
            }
        }
    }
    out
}
