//! v7.39 (round 249) — `COPY … FROM '<file>'`, differentially probed
//! against live PG18.4 (2026-07-19). The engine is no_std and performs
//! no I/O: the parser produces `Statement::CopyFromFile`, the HOST
//! (embedded / probe / tooling) reads the path and hands the bytes to
//! `Engine::copy_from_buffer`, which lowers to per-row INSERTs and —
//! outside an explicit transaction — wraps them in one, so a bad row
//! aborts the whole COPY exactly as in PG. These pins drive the buffer
//! endpoint directly; the file endpoint itself is pinned on the
//! embedded host (`copy_from_file_round249.rs` there), where WAL
//! durability is also proven.
//!
//! Gaps the sweep closed:
//!   * the statement did not parse at all (only TO STDOUT / FROM stdin);
//!   * a row with too FEW fields was silently accepted (SPG's INSERT
//!     allows short tuples) — PG refuses: `missing data for column "v"`;
//!   * too many fields now takes PG's "extra data after last expected
//!     column" instead of the INSERT arity wording;
//!   * relation / column existence and duplicate-column checks run
//!     before the file is read, in PG's order.
//!
//! Recorded divergence: on a row that is BOTH ill-typed and
//! wrong-arity, PG converts fields left-to-right and reports the type
//! error first; SPG checks the field count first. Both refuse the row
//! and the COPY stays atomic either way.

use spg_engine::{Engine, QueryResult};
use spg_sql::ast::{CopyFormat, CopyOptions};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ct (id int, name text, v int)")
        .unwrap();
    e
}

fn text_opts() -> CopyOptions {
    CopyOptions::default()
}

fn csv_opts() -> CopyOptions {
    CopyOptions {
        format: CopyFormat::Csv,
        ..CopyOptions::default()
    }
}

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn text_format_decodes_tabs_nulls_and_the_terminator() {
    let mut e = seeded();
    // Tab-separated, \N nulls, backslash escapes inside a cell.
    let r = e
        .copy_from_buffer(
            "ct",
            None,
            &text_opts(),
            "1\ta\t10\n2\tb\t\\N\n3\tc\\ttab\t30\n",
        )
        .unwrap();
    assert!(matches!(r, QueryResult::CommandOk { affected: 3, .. }));
    assert_eq!(
        rows(&mut e, "SELECT * FROM ct ORDER BY id"),
        ["1|a|10", "2|b|NULL", "3|c\ttab|30"]
    );
    // The \. terminator ends the data early (PG: COPY 1).
    e.execute("DELETE FROM ct").unwrap();
    let r = e
        .copy_from_buffer("ct", None, &text_opts(), "1\tx\t5\n\\.\n9\tskipped\t9\n")
        .unwrap();
    assert!(matches!(r, QueryResult::CommandOk { affected: 1, .. }));
    assert_eq!(rows(&mut e, "SELECT * FROM ct"), ["1|x|5"]);
}

#[test]
fn csv_format_honours_header_delimiter_null_and_quoting() {
    let mut e = seeded();
    // Quoted comma survives; the empty cell is the CSV null.
    e.copy_from_buffer("ct", None, &csv_opts(), "1,a,10\n2,b,\n3,\"c,comma\",30\n")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT * FROM ct ORDER BY id"),
        ["1|a|10", "2|b|NULL", "3|c,comma|30"]
    );
    // HEADER skips the first record.
    e.execute("DELETE FROM ct").unwrap();
    let opts = CopyOptions {
        header: true,
        ..csv_opts()
    };
    e.copy_from_buffer("ct", None, &opts, "id,name,v\n7,h1,70\n8,h2,80\n")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT * FROM ct ORDER BY id"),
        ["7|h1|70", "8|h2|80"]
    );
    // Custom DELIMITER / NULL; multi-line and doubled-quote cells.
    e.execute("DELETE FROM ct").unwrap();
    let opts = CopyOptions {
        delimiter: Some(';'),
        null_str: Some("NUL".into()),
        ..csv_opts()
    };
    e.copy_from_buffer(
        "ct",
        None,
        &opts,
        "1;a;10\n2;\"q;uote\";NUL\n3;\"multi\nline\";30\n4;\"em\"\"bed\";40\n",
    )
    .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT id, name, v FROM ct ORDER BY id"),
        [
            "1|a|10",
            "2|q;uote|NULL",
            "3|multi\nline|30",
            "4|em\"bed|40"
        ]
    );
}

#[test]
fn field_count_takes_pgs_wordings() {
    let mut e = seeded();
    // Too few fields — used to be silently accepted (short INSERT).
    for (opts, data) in [(text_opts(), "1\ta\n"), (csv_opts(), "1,a\n")] {
        let got = format!(
            "{}",
            e.copy_from_buffer("ct", None, &opts, data).unwrap_err()
        );
        assert!(got.contains("missing data for column \"v\""), "{got}");
    }
    // Too many fields against an explicit column list.
    let cols = ["id".to_string(), "name".to_string()];
    let got = format!(
        "{}",
        e.copy_from_buffer("ct", Some(&cols), &csv_opts(), "7,h1,70\n")
            .unwrap_err()
    );
    assert!(
        got.contains("extra data after last expected column"),
        "{got}"
    );
    assert_eq!(rows(&mut e, "SELECT count(*) FROM ct"), ["0"]);
}

#[test]
fn a_bad_row_aborts_the_whole_copy() {
    let mut e = seeded();
    let got = format!(
        "{}",
        e.copy_from_buffer("ct", None, &csv_opts(), "1,a,10\n2,b,notanint\n")
            .unwrap_err()
    );
    assert!(
        got.contains("invalid input syntax for type integer: \"notanint\""),
        "{got}"
    );
    // PG: COPY is all-or-nothing — row 1 must NOT survive.
    assert_eq!(rows(&mut e, "SELECT count(*) FROM ct"), ["0"]);
}

#[test]
fn pre_file_checks_run_in_pgs_order() {
    let mut e = seeded();
    // Relation existence, explicit-column existence, duplicate column —
    // all validated by copy_target_columns before any data (or file).
    let got = format!("{}", e.copy_target_columns("nope", None).unwrap_err());
    assert!(got.contains("relation \"nope\" does not exist"), "{got}");
    let cols = ["id".to_string(), "nope".to_string()];
    let got = format!("{}", e.copy_target_columns("ct", Some(&cols)).unwrap_err());
    assert!(
        got.contains("column \"nope\" of relation \"ct\" does not exist"),
        "{got}"
    );
    let cols = ["id".to_string(), "id".to_string()];
    let got = format!("{}", e.copy_target_columns("ct", Some(&cols)).unwrap_err());
    assert!(
        got.contains("column \"id\" specified more than once"),
        "{got}"
    );
    // The happy path resolves the schema order.
    assert_eq!(
        e.copy_target_columns("ct", None).unwrap(),
        ["id", "name", "v"]
    );
}

#[test]
fn the_raw_statement_parses_and_the_engine_names_the_host_contract() {
    let mut e = seeded();
    // The parser accepts the file form; the no_std engine cannot read
    // the file, so executing it directly names the host contract.
    let got = format!(
        "{}",
        e.execute("COPY ct FROM '/tmp/nope.csv' WITH (FORMAT csv)")
            .unwrap_err()
    );
    assert!(got.contains("copy_from_buffer"), "{got}");
    // parse_copy_from_file round-trips table / columns / path / options.
    let spec = spg_engine::copy::parse_copy_from_file(
        "COPY ct (id, name) FROM '/x/y.csv' WITH (FORMAT csv, HEADER, DELIMITER ';')",
    )
    .expect("should parse");
    assert_eq!(spec.table, "ct");
    assert_eq!(
        spec.columns.as_deref(),
        Some(&["id".to_string(), "name".to_string()][..])
    );
    assert_eq!(spec.path, "/x/y.csv");
    assert!(spec.options.header);
    assert_eq!(spec.options.delimiter, Some(';'));
    // Non-file COPY shapes are not this statement.
    assert!(spg_engine::copy::parse_copy_from_file("COPY ct TO STDOUT").is_none());
    assert!(spg_engine::copy::parse_copy_from_file("SELECT 1").is_none());
}
