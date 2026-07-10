//! COPY table [(cols)] TO STDOUT — text-format row stream as a
//! single-text-column result set.

use spg_engine::{Engine, QueryResult};

fn lines(e: &mut Engine, sql: &str) -> Vec<String> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, columns } = r else {
        panic!("expected Rows");
    };
    assert_eq!(columns.len(), 1);
    rows.iter()
        .map(|row| match &row.values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect()
}

#[test]
fn copy_text_format_basics() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ct (id INT, t TEXT)").unwrap();
    e.execute("INSERT INTO ct VALUES (1, 'plain'), (2, NULL)")
        .unwrap();
    let got = lines(&mut e, "COPY ct TO STDOUT");
    assert_eq!(got, ["1\tplain", "2\t\\N"]);
}

#[test]
fn copy_escapes_and_column_list() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ce (id INT, t TEXT)").unwrap();
    // Tab and newline in the cell must escape; the column list
    // narrows and reorders.
    e.execute("INSERT INTO ce VALUES (1, concat('a', chr(9), 'b', chr(10), 'c'))")
        .unwrap();
    let got = lines(&mut e, "COPY ce (t, id) TO STDOUT");
    assert_eq!(got, ["a\\tb\\nc\t1"]);
    // Round-trip through the decoder recovers the original.
    let cells = spg_engine::copy::decode_copy_text_row(&got[0]);
    assert_eq!(cells[0].as_deref(), Some("a\tb\nc"));
    assert_eq!(cells[1].as_deref(), Some("1"));
}

#[test]
fn copy_file_endpoint_and_bad_option_error() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE co2 (id INT)").unwrap();
    // File endpoints are still unsupported (STDOUT only).
    assert!(e.execute("COPY co2 TO '/tmp/f.csv'").is_err());
    // Unknown format / option are honest errors.
    assert!(
        e.execute("COPY co2 TO STDOUT WITH (FORMAT binary)")
            .is_err()
    );
    assert!(e.execute("COPY co2 TO STDOUT WITH (BOGUS)").is_err());
}

// read01 A-group U9 — COPY … TO STDOUT WITH (FORMAT csv, …). All
// expected output asserted byte-for-byte against live PostgreSQL 18.4.

fn csv_fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ct (a INT, b TEXT, c TEXT, d BOOL)")
        .unwrap();
    // b has a comma + a double-quote + an embedded newline across rows;
    // c is '' (empty) on row 3; d exercises bool t/f + NULL.
    e.execute(
        "INSERT INTO ct VALUES \
         (1, 'he,llo', 'x\"y', true), \
         (2, concat('line', chr(10), 'break'), NULL, false), \
         (3, 'plain', '', NULL)",
    )
    .unwrap();
    e
}

#[test]
fn copy_csv_basic_quoting() {
    let mut e = csv_fixture();
    let got = lines(&mut e, "COPY ct TO STDOUT WITH (FORMAT csv)");
    assert_eq!(
        got,
        [
            "1,\"he,llo\",\"x\"\"y\",t",
            "2,\"line\nbreak\",,f",
            "3,plain,\"\",",
        ]
    );
}

#[test]
fn copy_csv_header() {
    let mut e = csv_fixture();
    let got = lines(&mut e, "COPY ct TO STDOUT WITH (FORMAT csv, HEADER true)");
    assert_eq!(got[0], "a,b,c,d");
    assert_eq!(got.len(), 4);
}

#[test]
fn copy_csv_delimiter_and_null() {
    let mut e = csv_fixture();
    let got = lines(
        &mut e,
        "COPY ct TO STDOUT WITH (FORMAT csv, DELIMITER ';', NULL 'NULO')",
    );
    assert_eq!(
        got,
        [
            "1;he,llo;\"x\"\"y\";t",
            "2;\"line\nbreak\";NULO;f",
            "3;plain;;NULO",
        ]
    );
}

#[test]
fn copy_csv_custom_quote() {
    let mut e = csv_fixture();
    let got = lines(
        &mut e,
        "COPY ct (a, b) TO STDOUT WITH (FORMAT csv, QUOTE '#')",
    );
    assert_eq!(got, ["1,#he,llo#", "2,#line\nbreak#", "3,plain"]);
}

#[test]
fn copy_legacy_csv_header_spelling() {
    let mut e = csv_fixture();
    // `COPY … TO STDOUT CSV HEADER` (no WITH, no parens).
    let got = lines(&mut e, "COPY ct TO STDOUT CSV HEADER");
    assert_eq!(got[0], "a,b,c,d");
    assert_eq!(got[1], "1,\"he,llo\",\"x\"\"y\",t");
}

#[test]
fn copy_text_custom_delimiter_escapes() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ctd (a INT, b TEXT)").unwrap();
    e.execute("INSERT INTO ctd VALUES (1, 'a|b')").unwrap();
    // A literal delimiter char in the data is backslash-escaped.
    let got = lines(
        &mut e,
        "COPY ctd TO STDOUT WITH (FORMAT text, DELIMITER '|')",
    );
    assert_eq!(got, ["1|a\\|b"]);
}

#[test]
fn copy_text_bool_renders_t_f() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ctb (a BOOL)").unwrap();
    e.execute("INSERT INTO ctb VALUES (true), (false)").unwrap();
    // PG COPY renders bool via its output function: t / f, not
    // true / false.
    assert_eq!(lines(&mut e, "COPY ctb TO STDOUT"), ["t", "f"]);
}
