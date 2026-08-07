//! v7.39 (round 265) — COPY option validation, probed against live
//! PG18.4 (2026-07-20). Closes the residual round 250 recorded (the wire
//! parser silently ignored unrecognized options) and adds the two CSV
//! FROM-side column lists.
//!
//! `FORCE_NOT_NULL` / `FORCE_NULL` were unknown to the parser. Their
//! semantics, probed: with neither, an UNQUOTED empty CSV field is NULL
//! and a QUOTED empty (`""`) is the empty string; `FORCE_NOT_NULL(col)`
//! makes the unquoted one the empty string too; `FORCE_NULL(col)` makes
//! the quoted one NULL as well.
//!
//! The direction rules, also probed: `FORCE_QUOTE` is COPY TO only,
//! `FORCE_NOT_NULL` and `FORCE_NULL` are COPY FROM only, and all three
//! require CSV mode — with the CSV check taking precedence, so a
//! non-CSV `FORCE_NOT_NULL` on a TO reports "requires CSV mode" rather
//! than the direction.
//!
//! Wordings that were SPG's own now match PG: `option "x" not
//! recognized` and `COPY format "x" not recognized`.
//!
//! Recorded residual: `FORCE_NULL` is applied against the decoded null
//! token, so with a CUSTOM `NULL 'tok'` it also converts an unquoted
//! `tok`; PG distinguishes the quoted form. The CSV default (empty
//! string) — which is what the probe exercises — behaves identically.

use spg_engine::{Engine, QueryResult};

fn rows_of(e: &mut Engine, sql: &str) -> Vec<String> {
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

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

const CSV: &str = "1,\n2,\"\"\n";

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE fnn2 (id int, s text)").unwrap();
    e
}

fn csv_opts(extra: impl FnOnce(&mut spg_sql::ast::CopyOptions)) -> spg_sql::ast::CopyOptions {
    let mut o = spg_sql::ast::CopyOptions {
        format: spg_sql::ast::CopyFormat::Csv,
        ..spg_sql::ast::CopyOptions::default()
    };
    extra(&mut o);
    o
}

#[test]
fn force_not_null_and_force_null_change_csv_null_reading() {
    let mut e = seeded();
    let load = |e: &mut Engine, o: &spg_sql::ast::CopyOptions| {
        e.execute("DELETE FROM fnn2").unwrap();
        e.copy_from_buffer("fnn2", None, o, CSV).unwrap();
    };
    // Baseline: unquoted empty is NULL, quoted empty is the empty string.
    load(&mut e, &csv_opts(|_| {}));
    assert_eq!(
        rows_of(&mut e, "SELECT id, s IS NULL FROM fnn2 ORDER BY id"),
        ["1|true", "2|false"]
    );
    // FORCE_NOT_NULL: neither is NULL.
    load(
        &mut e,
        &csv_opts(|o| o.force_not_null = Some(vec!["s".to_string()])),
    );
    assert_eq!(
        rows_of(&mut e, "SELECT id, s IS NULL FROM fnn2 ORDER BY id"),
        ["1|false", "2|false"]
    );
    // FORCE_NULL: both are NULL.
    load(
        &mut e,
        &csv_opts(|o| o.force_null = Some(vec!["s".to_string()])),
    );
    assert_eq!(
        rows_of(&mut e, "SELECT id, s IS NULL FROM fnn2 ORDER BY id"),
        ["1|true", "2|true"]
    );
}

#[test]
fn the_direction_and_mode_rules_are_pgs() {
    let mut e = seeded();
    // CSV requirement is checked FIRST, so this reports the mode.
    let mut text_opts = spg_sql::ast::CopyOptions::default();
    text_opts.force_not_null = Some(vec!["s".to_string()]);
    let got = format!(
        "{}",
        e.copy_from_buffer("fnn2", None, &text_opts, CSV)
            .unwrap_err()
    );
    assert!(
        got.contains("COPY FORCE_NOT_NULL requires CSV mode"),
        "{got}"
    );
    // Then the direction.
    let got = err(
        &mut e,
        "COPY fnn2 TO STDOUT WITH (FORMAT csv, FORCE_NOT_NULL (s))",
    );
    assert!(
        got.contains("COPY FORCE_NOT_NULL cannot be used with COPY TO"),
        "{got}"
    );
    let got = err(
        &mut e,
        "COPY fnn2 TO STDOUT WITH (FORMAT csv, FORCE_NULL (s))",
    );
    assert!(
        got.contains("COPY FORCE_NULL cannot be used with COPY TO"),
        "{got}"
    );
    let got = err(&mut e, "COPY fnn2 TO STDOUT WITH (FORCE_QUOTE (s))");
    assert!(got.contains("COPY FORCE_QUOTE requires CSV mode"), "{got}");
}

#[test]
fn unknown_options_and_formats_take_pgs_wordings() {
    let mut e = seeded();
    let got = err(&mut e, "COPY fnn2 TO STDOUT WITH (NOSUCHOPT true)");
    assert!(got.contains("option \"nosuchopt\" not recognized"), "{got}");
    let got = err(&mut e, "COPY fnn2 TO STDOUT WITH (FORMAT nosuchfmt)");
    assert!(
        got.contains("COPY format \"nosuchfmt\" not recognized"),
        "{got}"
    );
}
