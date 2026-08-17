//! Record-level sqllogictest parser. Format we accept:
//!
//! ```text
//! # comment
//! statement ok
//! CREATE TABLE t (a INT)
//!
//! statement error
//! INSERT INTO nope VALUES (1)
//!
//! query IT rowsort
//! SELECT id, name FROM t ORDER BY id
//! ----
//! 1 alice
//! 2 bob
//!
//! halt
//!
//! skipif spg
//! statement ok
//! SELECT some_pg_only_thing()
//!
//! onlyif spg
//! statement ok
//! SELECT 1
//! ```
//!
//! Records are separated by blank lines. A `# comment` line that doesn't sit
//! between `statement`/`query` and its body is dropped.

use std::path::Path;

/// One parsed record from a `.test` file.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Statement {
        directive: Directive,
        sql: String,
        expect_error: bool,
    },
    Query {
        directive: Directive,
        sql: String,
        /// `type-string` of the spec: each char is one column's type.
        /// `I` = int, `T` = text, `R` = real, `B` = bool. Unknown chars are
        /// tolerated as `T`.
        type_string: String,
        sort: SortMode,
        /// Expected rows: each entry is one cell, tab-flattened in the order
        /// `(row0 col0, row0 col1, …, row1 col0, …)`.
        expected: ExpectedQuery,
    },
    Halt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Directive {
    pub skip: bool,
    pub only: bool,
    /// 1-based source line of the record's header — r1052 (S2.3), so a
    /// failure report can point at the file location pg_regress-style.
    pub line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    #[default]
    NoSort,
    RowSort,
    ValueSort,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExpectedQuery {
    /// Inline result: one cell per line.
    Values(Vec<String>),
    /// `<n> values hashing to <hash>` form. We don't implement hashing — we
    /// just track that it's a hash record so the runner can mark it as a
    /// "skip: hashed result" rather than fail on missing values.
    Hash { value_count: usize, hex: String },
}

#[derive(Debug)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Read and parse a `.test` file from `path`. Convenience wrapper around
/// `parse_str` that reads the file.
pub fn parse_file(path: &Path) -> Result<Vec<Record>, ParseError> {
    let bytes = std::fs::read(path).map_err(|e| ParseError {
        line: 0,
        message: format!("read {}: {e}", path.display()),
    })?;
    let text = String::from_utf8(bytes).map_err(|_| ParseError {
        line: 0,
        message: "file is not valid UTF-8".into(),
    })?;
    parse_str(&text)
}

/// Parse sqllogictest source text into a sequence of records.
pub fn parse_str(text: &str) -> Result<Vec<Record>, ParseError> {
    let mut out = Vec::new();
    let mut lines = text.lines().enumerate().peekable();
    let mut pending_directive = Directive::default();

    while let Some(&(_lineno, raw)) = lines.peek() {
        // Skip blank lines and # comments between records.
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            lines.next();
            continue;
        }

        // Directive line (skipif/onlyif) attaches to the next record.
        if trimmed.starts_with("skipif ") {
            // We only recognise the SPG-targeting form (or any engine name we
            // don't care about). Treat any `skipif <engine>` as "skip this
            // next record on us" only when the engine is `spg`; otherwise
            // accept (we don't know that other engine).
            let engine = trimmed.trim_start_matches("skipif ").trim();
            if engine.eq_ignore_ascii_case("spg") {
                pending_directive.skip = true;
            }
            lines.next();
            continue;
        }
        if trimmed.starts_with("onlyif ") {
            let engine = trimmed.trim_start_matches("onlyif ").trim();
            if engine.eq_ignore_ascii_case("spg") {
                pending_directive.only = true;
            } else {
                pending_directive.skip = true;
            }
            lines.next();
            continue;
        }

        // Halt: terminate parsing for this file.
        if trimmed == "halt" {
            out.push(Record::Halt);
            lines.next();
            return Ok(out);
        }

        // Directive-only records we don't act on, but eat.
        if trimmed.starts_with("hash-threshold ") || trimmed.starts_with("mode ") {
            lines.next();
            continue;
        }

        // statement <ok|error>
        if let Some(rest) = trimmed.strip_prefix("statement ") {
            let expect_error = match rest.trim() {
                "ok" => false,
                "error" => true,
                other => {
                    return Err(ParseError {
                        line: _lineno + 1,
                        message: format!("expected `statement ok|error`, got {other:?}"),
                    });
                }
            };
            lines.next();
            pending_directive.line = _lineno + 1;
            let sql = collect_sql_until_blank(&mut lines);
            out.push(Record::Statement {
                directive: pending_directive,
                sql,
                expect_error,
            });
            pending_directive = Directive::default();
            continue;
        }

        // query <type-string> [sort-mode] [label]
        if let Some(rest) = trimmed.strip_prefix("query ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            let type_string = parts.first().copied().unwrap_or("").to_string();
            let sort = match parts.get(1).copied() {
                Some("rowsort") => SortMode::RowSort,
                Some("valuesort") => SortMode::ValueSort,
                Some("nosort") | None => SortMode::NoSort,
                Some(other) => {
                    // Label fields (third token) appear sometimes; if the
                    // second token isn't a sort mode and isn't a label
                    // pattern, treat as nosort + unknown label.
                    if other.starts_with("label-") {
                        SortMode::NoSort
                    } else {
                        SortMode::NoSort
                    }
                }
            };
            lines.next();
            pending_directive.line = _lineno + 1;
            let sql = collect_sql_until_separator(&mut lines);
            let expected = collect_expected(&mut lines);
            out.push(Record::Query {
                directive: pending_directive,
                sql,
                type_string,
                sort,
                expected,
            });
            pending_directive = Directive::default();
            continue;
        }

        return Err(ParseError {
            line: _lineno + 1,
            message: format!("unknown record header: {trimmed:?}"),
        });
    }

    Ok(out)
}

fn collect_sql_until_blank<'a, I>(lines: &mut std::iter::Peekable<I>) -> String
where
    I: Iterator<Item = (usize, &'a str)>,
{
    let mut out = String::new();
    while let Some(&(_, line)) = lines.peek() {
        if line.trim().is_empty() {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        lines.next();
    }
    out
}

fn collect_sql_until_separator<'a, I>(lines: &mut std::iter::Peekable<I>) -> String
where
    I: Iterator<Item = (usize, &'a str)>,
{
    let mut out = String::new();
    while let Some(&(_, line)) = lines.peek() {
        if line.trim() == "----" {
            lines.next(); // consume the separator
            break;
        }
        if line.trim().is_empty() {
            // Sometimes a query record has no expected (the SQL is whatever
            // ran successfully). Terminate.
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        lines.next();
    }
    out
}

fn collect_expected<'a, I>(lines: &mut std::iter::Peekable<I>) -> ExpectedQuery
where
    I: Iterator<Item = (usize, &'a str)>,
{
    let mut values = Vec::new();
    while let Some(&(_, line)) = lines.peek() {
        if line.trim().is_empty() {
            break;
        }
        // Check the `<n> values hashing to <hex>` form.
        if let Some(rest) = line.strip_suffix(" values hashing to") {
            // Two-line form: this line ends, next line is the hash.
            let count: usize = rest.trim().parse().unwrap_or(0);
            lines.next();
            let hex = if let Some(&(_, h)) = lines.peek() {
                lines.next();
                h.trim().to_string()
            } else {
                String::new()
            };
            return ExpectedQuery::Hash {
                value_count: count,
                hex,
            };
        }
        // One-line form `<n> values hashing to <hex>`.
        if let Some(idx) = line.find(" values hashing to ") {
            let count: usize = line[..idx].trim().parse().unwrap_or(0);
            let hex = line[idx + " values hashing to ".len()..].trim().to_string();
            lines.next();
            return ExpectedQuery::Hash {
                value_count: count,
                hex,
            };
        }
        // Each line is one cell value.
        values.push(line.to_string());
        lines.next();
    }
    ExpectedQuery::Values(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_statement_ok() {
        let src = "statement ok\nCREATE TABLE t (a INT)\n";
        let recs = parse_str(src).unwrap();
        assert_eq!(recs.len(), 1);
        assert!(matches!(
            recs[0],
            Record::Statement { expect_error: false, ref sql, .. } if sql == "CREATE TABLE t (a INT)"
        ));
    }

    #[test]
    fn parses_statement_error() {
        let src = "statement error\nNOT VALID SQL\n";
        let recs = parse_str(src).unwrap();
        assert!(matches!(
            recs[0],
            Record::Statement {
                expect_error: true,
                ..
            }
        ));
    }

    #[test]
    fn parses_query_with_values() {
        let src = "query IT rowsort\nSELECT a, b FROM t\n----\n1\nalice\n2\nbob\n";
        let recs = parse_str(src).unwrap();
        let Record::Query { expected, sort, .. } = &recs[0] else {
            panic!("expected Query")
        };
        assert_eq!(*sort, SortMode::RowSort);
        match expected {
            ExpectedQuery::Values(v) => {
                assert_eq!(v, &["1", "alice", "2", "bob"]);
            }
            _ => panic!("expected Values"),
        }
    }

    #[test]
    fn parses_query_with_inline_hash() {
        let src = "query I nosort\nSELECT a FROM t\n----\n4 values hashing to deadbeef\n";
        let recs = parse_str(src).unwrap();
        let Record::Query { expected, .. } = &recs[0] else {
            panic!()
        };
        match expected {
            ExpectedQuery::Hash { value_count, hex } => {
                assert_eq!(*value_count, 4);
                assert_eq!(hex, "deadbeef");
            }
            _ => panic!("expected Hash"),
        }
    }

    #[test]
    fn skipif_spg_attaches_to_next_record() {
        let src = "skipif spg\nstatement ok\nSELECT 1\n";
        let recs = parse_str(src).unwrap();
        let Record::Statement { directive, .. } = &recs[0] else {
            panic!()
        };
        assert!(directive.skip);
    }

    #[test]
    fn onlyif_other_engine_marks_skip() {
        let src = "onlyif duckdb\nstatement ok\nSELECT 1\n";
        let recs = parse_str(src).unwrap();
        let Record::Statement { directive, .. } = &recs[0] else {
            panic!()
        };
        assert!(directive.skip);
    }

    #[test]
    fn halt_terminates_parsing() {
        let src = "statement ok\nSELECT 1\n\nhalt\n\nstatement ok\nSELECT 2\n";
        let recs = parse_str(src).unwrap();
        // halt is included, but anything after it isn't.
        assert_eq!(recs.len(), 2);
        assert!(matches!(recs[1], Record::Halt));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let src = "# a comment\n\n# another\nstatement ok\nSELECT 1\n\n# trailing\n";
        let recs = parse_str(src).unwrap();
        assert_eq!(recs.len(), 1);
    }
}
