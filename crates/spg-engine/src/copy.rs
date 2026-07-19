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


/// v7.39 (round 252) — a parsed `COPY … TO '<file>'` (table or query
/// form). The HOST renders via `Engine::copy_to_buffer` and writes
/// `path` itself.
#[derive(Debug)]
pub struct CopyToFileSpec {
    pub table: String,
    pub columns: Option<Vec<String>>,
    pub query: Option<alloc::boxed::Box<spg_sql::ast::Statement>>,
    pub path: String,
    pub options: spg_sql::ast::CopyOptions,
}

/// Parse `sql` and return its parts when it is a `COPY … TO '<file>'`
/// statement (any other statement, or a parse error, returns `None`).
#[must_use]
pub fn parse_copy_to_file(sql: &str) -> Option<CopyToFileSpec> {
    match spg_sql::parser::parse_statement(sql) {
        Ok(spg_sql::ast::Statement::CopyToFile {
            table,
            columns,
            query,
            path,
            options,
        }) => Some(CopyToFileSpec {
            table,
            columns,
            query,
            path,
            options,
        }),
        _ => None,
    }
}

/// v7.39 (round 265) — the COPY option rules that depend on DIRECTION,
/// probed against live PG18.4:
///
///   * `FORCE_QUOTE` is COPY TO only; `FORCE_NOT_NULL` and `FORCE_NULL`
///     are COPY FROM only. PG checks the CSV requirement FIRST, so a
///     non-CSV `FORCE_NOT_NULL` on a TO reports "requires CSV mode",
///     not the direction (probed both orders).
///   * `HEADER match` is COPY FROM only.
///
/// `to_direction` is true for COPY TO. Returns `Ok(())` when the
/// combination is legal.
///
/// # Errors
/// PG's wording for whichever rule the options break.
pub fn validate_copy_option_direction(
    options: &spg_sql::ast::CopyOptions,
    to_direction: bool,
) -> Result<(), crate::EngineError> {
    let is_csv = options.format == spg_sql::ast::CopyFormat::Csv;
    let csv_only = |name: &str| {
        crate::EngineError::Unsupported(alloc::format!("COPY {name} requires CSV mode"))
    };
    let wrong_way = |name: &str| {
        crate::EngineError::Unsupported(alloc::format!(
            "COPY {name} cannot be used with COPY {}",
            if to_direction { "TO" } else { "FROM" }
        ))
    };
    for (present, name, to_only) in [
        (options.force_quote.is_some(), "FORCE_QUOTE", true),
        (options.force_not_null.is_some(), "FORCE_NOT_NULL", false),
        (options.force_null.is_some(), "FORCE_NULL", false),
    ] {
        if !present {
            continue;
        }
        if !is_csv {
            return Err(csv_only(name));
        }
        if to_only != to_direction {
            return Err(wrong_way(name));
        }
    }
    Ok(())
}

/// v7.39 (round 249) — a parsed `COPY <table> [(cols)] FROM '<path>'`.
/// The engine is no_std: the HOST reads `path` and hands the bytes to
/// [`copy_buffer_inserts`] / `Engine::copy_from_buffer`.
#[derive(Debug)]
pub struct CopyFromFileSpec {
    pub table: String,
    pub columns: Option<Vec<String>>,
    pub path: String,
    pub options: spg_sql::ast::CopyOptions,
}

/// Parse `sql` and return its parts when it is a `COPY … FROM '<file>'`
/// statement — the host-side sniff for the file endpoint (any other
/// statement, or a parse error, returns `None` and the caller executes
/// normally).
#[must_use]
pub fn parse_copy_from_file(sql: &str) -> Option<CopyFromFileSpec> {
    match spg_sql::parser::parse_statement(sql) {
        Ok(spg_sql::ast::Statement::CopyFromFile {
            table,
            columns,
            path,
            options,
        }) => Some(CopyFromFileSpec {
            table,
            columns,
            path,
            options,
        }),
        _ => None,
    }
}

/// v7.39 (round 249) — decode a whole `COPY … FROM '<file>'` buffer
/// (the HOST read the file; the engine is no_std and performs no I/O)
/// into the per-row INSERT statements both hosts drive. Text and CSV
/// formats honour DELIMITER / NULL / HEADER / QUOTE; the text-format
/// `\.` terminator ends the data early, as in PG.
///
/// # Errors
/// Non-UTF-8 CSV input is refused (the text path takes `&str` too, so
/// it can't arise there).
pub fn copy_buffer_inserts(
    table: &str,
    columns: Option<&[String]>,
    target_cols: &[String],
    options: &spg_sql::ast::CopyOptions,
    data: &str,
) -> Result<Vec<String>, crate::EngineError> {
    // PG validates each row's field count against the target column
    // list before any type conversion: too many fields is "extra data
    // after last expected column", too few names the first column left
    // unfilled (both 22P04).
    let check_row = |values: &Vec<Option<String>>| -> Result<(), crate::EngineError> {
        if values.len() > target_cols.len() {
            return Err(crate::EngineError::Unsupported(String::from(
                "extra data after last expected column",
            )));
        }
        if values.len() < target_cols.len() {
            return Err(crate::EngineError::Unsupported(format!(
                "missing data for column \"{}\"",
                target_cols[values.len()]
            )));
        }
        Ok(())
    };
    use spg_sql::ast::CopyFormat;
    let is_csv = options.format == CopyFormat::Csv;
    let delimiter = options.delimiter.unwrap_or(if is_csv { ',' } else { '\t' });
    let quote = options.quote.unwrap_or('"');
    let null_str = options
        .null_str
        .clone()
        .unwrap_or_else(|| String::from(if is_csv { "" } else { "\\N" }));
    // v7.39 (round 265) — the direction rules, then the two CSV column
    // lists. `*` (an empty vec) means every column.
    validate_copy_option_direction(options, false)?;
    let in_list = |list: &Option<alloc::vec::Vec<String>>, idx: usize| -> bool {
        match list {
            None => false,
            Some(cols) if cols.is_empty() => true,
            Some(cols) => target_cols
                .get(idx)
                .is_some_and(|c| cols.iter().any(|w| w.eq_ignore_ascii_case(c))),
        }
    };
    let mut inserts = Vec::new();
    let mut first = true;
    if is_csv {
        let mut buf: Vec<u8> = data.as_bytes().to_vec();
        if !buf.is_empty() && !buf.ends_with(b"\n") {
            buf.push(b'\n');
        }
        let d8 = u8::try_from(delimiter as u32).unwrap_or(b',');
        let q8 = u8::try_from(quote as u32).unwrap_or(b'"');
        let mut start = 0;
        while let Some(len) = csv_record_end(&buf[start..], d8, q8) {
            let mut rec = &buf[start..start + len - 1];
            start += len;
            if rec.last() == Some(&b'\r') {
                rec = &rec[..rec.len() - 1];
            }
            if rec.is_empty() {
                continue;
            }
            if first && options.header {
                first = false;
                continue;
            }
            first = false;
            let rec_str = core::str::from_utf8(rec).map_err(|_| {
                crate::EngineError::Unsupported("COPY FROM: non-UTF-8 input".into())
            })?;
            let mut values = decode_copy_csv_record(rec_str, delimiter, quote, &null_str);
            // v7.39 (round 265) — FORCE_NOT_NULL turns a field that decoded
            // as NULL into the empty string; FORCE_NULL turns one that
            // decoded as the null token's text (a QUOTED empty under the
            // CSV default) into NULL. Probed: with neither, `1,` is NULL
            // and `2,""` is the empty string; FORCE_NOT_NULL makes both
            // non-NULL and FORCE_NULL makes both NULL.
            if options.force_not_null.is_some() || options.force_null.is_some() {
                for (idx, cell) in values.iter_mut().enumerate() {
                    if in_list(&options.force_not_null, idx) && cell.is_none() {
                        *cell = Some(String::new());
                    }
                    if in_list(&options.force_null, idx)
                        && cell.as_deref() == Some(null_str.as_str())
                    {
                        *cell = None;
                    }
                }
            }
            check_row(&values)?;
            inserts.push(build_copy_insert(table, columns, &values));
        }
    } else {
        for line in data.lines() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() {
                continue;
            }
            if first && options.header {
                first = false;
                continue;
            }
            first = false;
            if line == "\\." {
                break;
            }
            let values = decode_copy_text_row(line);
            check_row(&values)?;
            inserts.push(build_copy_insert(table, columns, &values));
        }
    }
    Ok(inserts)
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
        } else {
            at_field_start = b == delimiter;
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
    encode_copy_csv_cells_opts(cells, delimiter, quote, quote, None, null_str)
}

/// v7.39 (round 247) — the full CSV cell encoder: `escape` is the
/// character that precedes a quote (or itself) inside a quoted cell
/// (PG's default is the quote itself — doubling), and `force_quote`
/// marks per-column forced quoting (NULLs stay bare, as PG's
/// FORCE_QUOTE does).
pub fn encode_copy_csv_cells_opts(
    cells: &[Option<String>],
    delimiter: char,
    quote: char,
    escape: char,
    force_quote: Option<&[bool]>,
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
                let forced = force_quote
                    .and_then(|f| f.get(i))
                    .copied()
                    .unwrap_or(false);
                let needs_quote = forced
                    || s.as_str() == null_str
                    || s.chars().any(|c| {
                        c == delimiter || c == quote || c == escape || c == '\n' || c == '\r'
                    });
                if needs_quote {
                    out.push(quote);
                    for c in s.chars() {
                        if c == quote || c == escape {
                            out.push(escape);
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
