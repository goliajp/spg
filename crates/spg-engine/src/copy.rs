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

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// 9.0.3 — a parsed `COPY <table> [(cols)] FROM STDIN [options]`: the
/// head of a COPY whose rows follow out of band (CopyData frames on the
/// wire, the lines after the statement in a dump).
///
/// It was read by hand from lowercased text, which dropped the schema
/// (`COPY sa.t (…) FROM stdin`, pg_dump's spelling for every table, loaded
/// `sa`'s rows into `public.t`), lost a quoted name's case, and ignored
/// every option. The statement grammar reads it now.
#[derive(Debug)]
pub struct CopyFromStdinSpec {
    pub table: String,
    pub table_qualified: bool,
    pub columns: Option<Vec<String>>,
    pub options: spg_sql::ast::CopyOptions,
}

/// Parse `sql` as `COPY … FROM STDIN`; any other statement is `None`.
///
/// # Errors
/// A COPY FROM STDIN whose option list is malformed, in PostgreSQL's
/// words — so a bad option is reported rather than the statement being
/// taken for something else.
pub fn parse_copy_from_stdin(sql: &str) -> Result<Option<CopyFromStdinSpec>, crate::EngineError> {
    match spg_sql::parser::parse_statement(sql) {
        Ok(spg_sql::ast::Statement::CopyFromStdin {
            table,
            table_qualified,
            columns,
            options,
        }) => Ok(Some(CopyFromStdinSpec {
            table,
            table_qualified,
            columns,
            options,
        })),
        Ok(_) => Ok(None),
        Err(e) if head_is_copy_from_stdin(sql) => Err(crate::EngineError::Parse(e)),
        Err(_) => Ok(None),
    }
}

/// `COPY … FROM STDIN` by its words alone, so a malformed option list is
/// reported as such.
fn head_is_copy_from_stdin(sql: &str) -> bool {
    let lower = sql.trim_start().to_ascii_lowercase();
    lower.starts_with("copy")
        && lower
            .split_ascii_whitespace()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|w| w[0] == "from" && w[1].starts_with("stdin"))
}

/// v7.39 (round 252) — a parsed `COPY … TO '<file>'` (table or query
/// form). The HOST renders via `Engine::copy_to_buffer` and writes
/// `path` itself.
#[derive(Debug)]
pub struct CopyToFileSpec {
    pub table: String,
    pub table_qualified: bool,
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
            table_qualified,
            columns,
            query,
            path,
            options,
        }) => Some(CopyToFileSpec {
            table,
            table_qualified,
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
    // 9.0.3 — the options that only make sense while reading rows in.
    // PostgreSQL 18.6: `ON_ERROR` names itself; `HEADER match` answers
    // `cannot use "match" with HEADER in COPY TO`. `SKIP` and `FORMAT
    // json` are SPG's own and follow the same rule.
    if to_direction {
        if options.on_error.is_some() {
            return Err(wrong_way("ON_ERROR"));
        }
        if options.header_match {
            return Err(crate::EngineError::Unsupported(
                "cannot use \"match\" with HEADER in COPY TO".into(),
            ));
        }
        if options.skip > 0 {
            return Err(wrong_way("SKIP"));
        }
        if options.format == spg_sql::ast::CopyFormat::Json {
            return Err(crate::EngineError::Unsupported(
                "COPY FORMAT json cannot be used with COPY TO".into(),
            ));
        }
    }
    Ok(())
}

/// A column a COPY option lists (`FORCE_QUOTE`, `FORCE_NULL`, …) must be
/// one of the relation's, and one the COPY moves. PostgreSQL checks in
/// that order: `column "x" of relation "t" does not exist` (42703), then
/// `<OPTION> column "x" not referenced by COPY` (42P10).
///
/// # Errors
/// Whichever of the two the column fails.
pub fn check_listed_column(
    option: &str,
    column: &str,
    target: &crate::copy_from::CopyTarget,
) -> Result<(), crate::EngineError> {
    if !target
        .table_columns
        .iter()
        .any(|c| c.eq_ignore_ascii_case(column))
    {
        return Err(crate::EngineError::Unsupported(alloc::format!(
            "column \"{column}\" of relation \"{}\" does not exist",
            spg_sql::namespace::display_key(&target.table)
        )));
    }
    if !target.names.iter().any(|c| c.eq_ignore_ascii_case(column)) {
        return Err(crate::EngineError::Unsupported(alloc::format!(
            "{option} column \"{column}\" not referenced by COPY"
        )));
    }
    Ok(())
}

/// 9.0.3 — how a COPY TO writes its lines, set up once from the options:
/// the engine's own COPY TO and the wire's both encode through it.
#[derive(Debug)]
pub struct CopyToEncoder {
    csv: bool,
    delimiter: char,
    quote: char,
    escape: char,
    force: Option<Vec<bool>>,
    null_str: String,
}

impl CopyToEncoder {
    /// `out_names` are the columns in the order they are written; `target`
    /// is the relation a table COPY reads (none for `COPY (query)`), which
    /// the `FORCE_QUOTE` columns are checked against.
    ///
    /// # Errors
    /// PostgreSQL's refusals for an option COPY TO cannot take.
    pub fn new(
        options: &spg_sql::ast::CopyOptions,
        out_names: &[String],
        target: Option<&crate::copy_from::CopyTarget>,
    ) -> Result<Self, crate::EngineError> {
        validate_copy_option_direction(options, true)?;
        let csv = options.format == spg_sql::ast::CopyFormat::Csv;
        if !csv {
            if options.quote.is_some() {
                return Err(crate::EngineError::Unsupported(
                    "COPY QUOTE requires CSV mode".into(),
                ));
            }
            if options.escape.is_some() {
                return Err(crate::EngineError::Unsupported(
                    "COPY ESCAPE requires CSV mode".into(),
                ));
            }
        }
        let quote = options.quote.unwrap_or('"');
        let force = match &options.force_quote {
            None => None,
            Some(cols) if cols.is_empty() => Some(alloc::vec![true; out_names.len()]),
            Some(cols) => {
                let mut mask = alloc::vec![false; out_names.len()];
                for c in cols {
                    if let Some(t) = target {
                        check_listed_column("FORCE_QUOTE", c, t)?;
                    }
                    let pos = out_names
                        .iter()
                        .position(|n| n.eq_ignore_ascii_case(c))
                        .ok_or_else(|| {
                            crate::EngineError::Unsupported(alloc::format!(
                                "FORCE_QUOTE column \"{c}\" not referenced by COPY"
                            ))
                        })?;
                    mask[pos] = true;
                }
                Some(mask)
            }
        };
        Ok(Self {
            csv,
            delimiter: options.delimiter.unwrap_or(if csv { ',' } else { '\t' }),
            quote,
            escape: options.escape.unwrap_or(quote),
            force,
            null_str: options
                .null_str
                .clone()
                .unwrap_or_else(|| String::from(if csv { "" } else { "\\N" })),
        })
    }

    /// One line, without its newline.
    #[must_use]
    pub fn encode(&self, cells: &[Option<String>]) -> String {
        if self.csv {
            encode_copy_csv_cells_opts(
                cells,
                self.delimiter,
                self.quote,
                self.escape,
                self.force.as_deref(),
                &self.null_str,
            )
        } else {
            encode_copy_text_cells_opts(cells, self.delimiter, &self.null_str)
        }
    }
}

/// v7.39 (round 249) — a parsed `COPY <table> [(cols)] FROM '<path>'`.
/// The engine is no_std: the HOST reads `path` and hands the bytes to
/// `Engine::copy_from_buffer` (or reads them with
/// [`crate::copy_from::CopyFromReader`] itself).
#[derive(Debug)]
pub struct CopyFromFileSpec {
    pub table: String,
    pub table_qualified: bool,
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
            table_qualified,
            columns,
            path,
            options,
        }) => Some(CopyFromFileSpec {
            table,
            table_qualified,
            columns,
            path,
            options,
        }),
        _ => None,
    }
}

/// Decode one COPY text-format data row: tab-separated cells,
/// `\N` = NULL, C-style backslash escapes.
#[must_use]
pub fn decode_copy_text_row(line: &str) -> Vec<Option<String>> {
    decode_copy_text_row_opts(line, '\t', "\\N")
}

/// v7.40.12 — the same, with PG's `DELIMITER` and `NULL` options.
///
/// They apply to the TEXT format too, not only to CSV: measured on
/// PG 18.6, `COPY t FROM stdin WITH (DELIMITER '|')` loads
/// `1|hello` as two columns, and `WITH (NULL 'NIL')` turns the token
/// `NIL` into a SQL NULL. SPG parsed both options and used them only on
/// the CSV path, so the first ERRORED with `missing data for column
/// "b"` and the second silently stored the literal text `NIL`. The
/// COPY TO side has honoured both since round 94; only the FROM side's
/// text decoder still had the defaults written into it.
pub fn decode_copy_text_row_opts(
    line: &str,
    delimiter: char,
    null_string: &str,
) -> Vec<Option<String>> {
    line.split(delimiter)
        .map(|cell| {
            if cell == null_string {
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
    decode_copy_csv_record_escaped(record, delimiter, quote, quote, null_str)
}

/// [`decode_copy_csv_record`] with PostgreSQL's `ESCAPE` character.
#[must_use]
pub fn decode_copy_csv_record_escaped(
    record: &str,
    delimiter: char,
    quote: char,
    escape: char,
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
                if c == escape && i + 1 < n && (chars[i + 1] == quote || chars[i + 1] == escape) {
                    content.push(chars[i + 1]);
                    i += 2;
                } else if c == quote {
                    i += 1; // closing quote
                    break;
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
    csv_record_end_escaped(buf, delimiter, quote, quote)
}

/// [`csv_record_end`] with PostgreSQL's `ESCAPE`: inside a quoted field
/// the escape byte takes the next quote or escape byte literally (the
/// default escape is the quote itself — a doubled quote).
#[must_use]
pub fn csv_record_end_escaped(buf: &[u8], delimiter: u8, quote: u8, escape: u8) -> Option<usize> {
    let mut in_quote = false;
    let mut at_field_start = true;
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        if in_quote {
            if b == escape && matches!(buf.get(i + 1), Some(&n) if n == quote || n == escape) {
                i += 2; // escaped byte, still inside the field
                continue;
            }
            if b == quote {
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

/// Build `INSERT INTO <table> [(cols)] VALUES (…)` from a decoded row.
///
/// 9.0.3 — every value is a string literal, so the column's own type
/// reads it the way PostgreSQL's input function does. Cells that looked
/// numeric went in bare, and the INSERT then converted a NUMBER: an
/// `integer` column stored `1.5` as 2 where PostgreSQL refuses it, and a
/// `text` column refused `+5`, which PostgreSQL stores as written.
///
/// The relation is written the way the COPY wrote it (`written_sql`):
/// `public.t` stays public's table whatever the search path says, and a
/// quoted mixed-case name keeps its case.
#[must_use]
pub fn build_copy_insert(
    table: &str,
    table_qualified: bool,
    columns: Option<&[String]>,
    values: &[Option<String>],
) -> String {
    let mut sql = alloc::format!(
        "INSERT INTO {} ",
        spg_sql::namespace::written_sql(table, table_qualified)
    );
    if let Some(cols) = columns {
        sql.push('(');
        for (i, c) in cols.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&spg_sql::namespace::written_sql(c, false));
        }
        sql.push_str(") ");
    }
    // 9.0.0 — COPY may supply a value for a `GENERATED ALWAYS AS
    // IDENTITY` column; INSERT may not. Measured on PostgreSQL 18.6:
    // `COPY t (id, n) FROM stdin` into such a column takes the value,
    // and the identical `INSERT` answers `cannot insert a non-DEFAULT
    // value into column "id"`. That is what `pg_dump` relies on — it
    // writes the data as COPY — and COPY rides the INSERT path here, so
    // it inherited a refusal PostgreSQL does not make and a dump of an
    // identity table would not restore. `OVERRIDING SYSTEM VALUE` is
    // PostgreSQL's own spelling for exactly this permission, and both
    // engines accept it on a table that has no identity column at all.
    sql.push_str("OVERRIDING SYSTEM VALUE VALUES (");
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        match v {
            None => sql.push_str("NULL"),
            Some(s) => {
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
    sql.push(')');
    sql
}

/// v7.39 (read01 round 94) — render a value as the RAW COPY cell text
/// (`None` for SQL NULL), BEFORE any format-specific escaping. The engine's
/// `encode_copy_{text,csv}_cells` apply the delimiter/quote/null escaping on
/// top, so keeping escaping out of here is what lets the same cell feed both
/// the text and csv encoders without double-escaping.
///
/// 9.0.3 — one renderer for every COPY TO: the wire's, which reads the
/// session's render style and time zone, moved here so the engine's own
/// COPY TO (a file endpoint, the embedded host) renders the same cells.
///
/// `ty` exists only to tell `timestamptz` from `timestamp`: PG's COPY renders
/// the former with its offset (`2024-01-15 10:30:00+00`), the latter without.
#[must_use]
pub fn copy_cell_text(
    v: &spg_storage::Value<'_>,
    ty: Option<spg_storage::DataType>,
    style: &crate::eval::RenderStyle,
    tz: &crate::SessionTz,
) -> Option<String> {
    use spg_storage::Value;
    let s = match v {
        Value::Null => return None,
        Value::Bool(b) => if *b { "t" } else { "f" }.to_string(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Float(x) => crate::eval::format_float_styled(*x, style),
        Value::Real(x) => crate::eval::format_real_styled(*x, style),
        Value::Text(s) | Value::Json(s) => s.to_string(),
        // v7.39 (bpchar epic) — COPY emits the padded stored form.
        Value::BpChar(s) => s.to_string(),
        // v7.39 (FTS) — canonical text forms for COPY too.
        Value::TsVector(lexs) => crate::eval::format_tsvector(lexs),
        Value::TsQuery(ast) => crate::eval::format_tsquery(ast),
        Value::Numeric {
            scaled,
            scale,
            kind,
        } => crate::eval::format_numeric_kind(*kind, *scaled, *scale),
        Value::Date(d) => crate::eval::format_date_styled(*d, style),
        Value::Timestamp(t) => {
            if matches!(ty, Some(spg_storage::DataType::Timestamptz)) {
                let abbr = tz.abbrev_at(*t);
                crate::eval::format_timestamptz_tz(*t, style, tz.offset_at(*t), abbr.as_deref())
            } else {
                crate::eval::format_timestamp_styled(*t, style)
            }
        }
        Value::Interval {
            months,
            days,
            micros,
            kind,
        } if kind.is_finite() => {
            crate::eval::format_interval_styled(*months, *days, *micros, style)
        }
        Value::Interval { kind, .. } => crate::eval::format_interval_kinded(0, 0, 0, *kind),
        Value::Vector(v) => {
            let parts: Vec<alloc::string::String> =
                v.iter().map(alloc::string::ToString::to_string).collect();
            alloc::format!("[{}]", parts.join(","))
        }
        // v6.0.1: COPY OUT a `VECTOR(N) USING SQ8` column — dequantise to f32
        // so the COPY text stream stays pgvector-compatible.
        Value::Sq8Vector(q) => {
            let parts: Vec<alloc::string::String> = spg_storage::quantize::dequantize(q)
                .iter()
                .map(alloc::string::ToString::to_string)
                .collect();
            alloc::format!("[{}]", parts.join(","))
        }
        // v6.0.3: COPY OUT for `VECTOR(N) USING HALF` — bit-exact dequantise.
        Value::HalfVector(h) => {
            let parts: Vec<alloc::string::String> = h
                .to_f32_vec()
                .iter()
                .map(alloc::string::ToString::to_string)
                .collect();
            alloc::format!("[{}]", parts.join(","))
        }
        // v7.5.0 — Value is #[non_exhaustive].
        other => crate::eval::value_to_text(other),
    };
    Some(s)
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
                let forced = force_quote.and_then(|f| f.get(i)).copied().unwrap_or(false);
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
        let spec = parse_copy_from_stdin("COPY public.messages (id, subject, body) FROM stdin")
            .unwrap()
            .unwrap();
        assert_eq!(spec.table, "messages");
        assert!(spec.table_qualified);
        assert_eq!(
            spec.columns.as_deref(),
            Some(&["id".to_string(), "subject".to_string(), "body".to_string()][..])
        );
        // A schema other than public stays in the key.
        let sa = parse_copy_from_stdin("COPY sa.t (id) FROM stdin")
            .unwrap()
            .unwrap();
        assert_eq!(sa.table, spg_sql::namespace::qualified_key("sa", "t"));
        // No column list.
        let bare = parse_copy_from_stdin("copy t from stdin").unwrap().unwrap();
        assert_eq!(bare.table, "t");
        assert!(!bare.table_qualified);
        assert_eq!(bare.columns, None);
        // Options are read, not refused.
        let csv = parse_copy_from_stdin("COPY t FROM stdin WITH (FORMAT csv)")
            .unwrap()
            .unwrap();
        assert_eq!(csv.options.format, spg_sql::ast::CopyFormat::Csv);
        // Not this shape.
        assert!(parse_copy_from_stdin("COPY t TO stdout").unwrap().is_none());
        assert!(
            parse_copy_from_stdin("COPY t FROM '/tmp/f.csv'")
                .unwrap()
                .is_none()
        );
        // A malformed option list is reported, not taken for another statement.
        assert!(parse_copy_from_stdin("COPY t FROM stdin (NOSUCH 1)").is_err());
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
        // 9.0.0 — `OVERRIDING SYSTEM VALUE` is here because COPY may
        // fill a `GENERATED ALWAYS AS IDENTITY` column and INSERT may
        // not; see `build_copy_insert`. 9.0.3 — every value is a string
        // literal the column's type reads.
        let cols = vec!["id".to_string(), "note".to_string()];
        let row = vec![Some("7".to_string()), Some("it's".to_string())];
        assert_eq!(
            build_copy_insert("t", false, Some(&cols), &row),
            "INSERT INTO t (id, note) OVERRIDING SYSTEM VALUE VALUES ('7', 'it''s')"
        );
        assert_eq!(
            build_copy_insert("t", true, None, &[None, Some("0042".to_string())]),
            "INSERT INTO public.t OVERRIDING SYSTEM VALUE VALUES (NULL, '0042')"
        );
        let mixed = vec!["Id".to_string()];
        assert_eq!(
            build_copy_insert("MixedCase", false, Some(&mixed), &[Some("1".to_string())]),
            "INSERT INTO \"MixedCase\" (\"Id\") OVERRIDING SYSTEM VALUE VALUES ('1')"
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
