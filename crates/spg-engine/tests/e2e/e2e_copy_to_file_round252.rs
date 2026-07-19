//! v7.39 (round 252) — `COPY … TO '<file>'`, differentially probed
//! against live PG18.4 (2026-07-19): text (tab + `\N` + backslash
//! escapes), CSV with HEADER, the query form and the column-list form
//! all byte-identical to PG's written files. The engine is no_std:
//! `Engine::copy_to_buffer` renders the payload and reports the DATA
//! row count (HEADER line included in the payload, excluded from the
//! count — PG's `COPY n` tag), and the HOST writes the path. Executing
//! the raw statement on the engine names that contract.

use spg_engine::Engine;
use spg_sql::ast::{CopyFormat, CopyOptions};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ct (id int, name text, v int)").unwrap();
    e.execute("INSERT INTO ct VALUES (1,'a',10),(2,'b',NULL),(3,E'c\\ttab',30)")
        .unwrap();
    e
}

#[test]
fn text_and_csv_payloads_match_pg_byte_for_byte() {
    let mut e = seeded();
    // Bare text: tab-separated, \N null, embedded tab escaped.
    let (payload, n) = e
        .copy_to_buffer("ct", None, None, &CopyOptions::default())
        .unwrap();
    assert_eq!(payload, "1\ta\t10\n2\tb\t\\N\n3\tc\\ttab\t30\n");
    assert_eq!(n, 3);
    // CSV + HEADER: header line in the payload, NOT in the count; the
    // embedded tab is raw (CSV quotes only delimiter/quote/newline).
    let opts = CopyOptions {
        format: CopyFormat::Csv,
        header: true,
        ..CopyOptions::default()
    };
    let (payload, n) = e.copy_to_buffer("ct", None, None, &opts).unwrap();
    assert_eq!(payload, "id,name,v\n1,a,10\n2,b,\n3,c\ttab,30\n");
    assert_eq!(n, 3);
    // Column-list form.
    let cols = ["name".to_string()];
    let (payload, n) = e
        .copy_to_buffer("ct", Some(&cols), None, &CopyOptions::default())
        .unwrap();
    assert_eq!(payload, "a\nb\nc\\ttab\n");
    assert_eq!(n, 3);
}

#[test]
fn the_query_form_renders_through_the_same_encoder() {
    let mut e = seeded();
    let inner = spg_sql::parser::parse_statement("SELECT id, name FROM ct ORDER BY id").unwrap();
    let opts = CopyOptions { format: CopyFormat::Csv, ..CopyOptions::default() };
    let (payload, n) = e.copy_to_buffer("", None, Some(&inner), &opts).unwrap();
    assert_eq!(payload, "1,a\n2,b\n3,c\ttab\n");
    assert_eq!(n, 3);
}

#[test]
fn the_raw_statement_parses_and_the_engine_names_the_host_contract() {
    let mut e = seeded();
    let got = format!("{}", e.execute("COPY ct TO '/tmp/x.csv'").unwrap_err());
    assert!(got.contains("copy_to_buffer"), "{got}");
    // The facade round-trips both forms.
    let spec = spg_engine::copy::parse_copy_to_file(
        "COPY ct (id) TO '/x/y.csv' WITH (FORMAT csv, HEADER)",
    )
    .expect("table form should parse");
    assert_eq!(spec.table, "ct");
    assert_eq!(spec.columns.as_deref(), Some(&["id".to_string()][..]));
    assert_eq!(spec.path, "/x/y.csv");
    assert!(spec.options.header);
    let spec = spg_engine::copy::parse_copy_to_file("COPY (SELECT 1) TO '/x/q.txt'")
        .expect("query form should parse");
    assert!(spec.query.is_some());
    assert_eq!(spec.path, "/x/q.txt");
    // STDOUT and FROM shapes are not this statement.
    assert!(spg_engine::copy::parse_copy_to_file("COPY ct TO STDOUT").is_none());
    assert!(spg_engine::copy::parse_copy_to_file("COPY ct FROM '/x/y.csv'").is_none());
}
