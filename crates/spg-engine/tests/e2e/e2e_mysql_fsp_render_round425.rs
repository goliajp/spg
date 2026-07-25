//! read01 round 425 (MySQL differential) — rendering a temporal column at
//! its DECLARED fractional-seconds precision.
//!
//! Round 424 made the stored INSTANT match MariaDB (write-path truncation).
//! This round makes the TEXT match: MariaDB prints exactly as many
//! fractional digits as the column declares, zero-padded — `DATETIME(3)`
//! shows `.250` for a quarter second and `.000` for a whole one, where PG's
//! renderer trims trailing zeros. Two clients comparing rendered datetimes
//! saw different strings for the same instant.
//!
//! The precision now rides the RESULT schema: `ProjectedItem.mysql_fsp` (and
//! from there `ColumnSchema.mysql_fsp` on the result) carries it through the
//! projection, exactly as `user_enum_type` carries enum identity. The MySQL
//! wire encoder reads it per column; `value_to_text_with_fsp` is the shared
//! contract, and this test exercises the same call the encoder makes.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

/// Render the first row the way the MySQL wire encoder does: per column,
/// through `value_to_text_with_fsp` with that column's declared precision.
fn wire_row(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { columns, rows } => rows[0]
            .values
            .iter()
            .enumerate()
            .map(|(i, v)| match v {
                Value::Null => "NULL".to_string(),
                other => spg_engine::eval::value_to_text_with_fsp(
                    other,
                    columns.get(i).and_then(|c| c.mysql_fsp),
                ),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = mysql();
    e.execute("CREATE TABLE c(d0 DATETIME, d3 DATETIME(3), d6 DATETIME(6), t2 TIME(2))")
        .unwrap();
    e.execute(
        "INSERT INTO c VALUES('2020-01-01 00:00:00.25','2020-01-01 00:00:00.25',\
         '2020-01-01 00:00:00.25','10:00:00.5')",
    )
    .unwrap();
    e
}

/// A value with fewer significant digits than the declared precision is
/// zero-padded to exactly that many.
#[test]
fn fraction_pads_to_declared_precision() {
    let mut e = seeded();
    assert_eq!(wire_row(&mut e, "SELECT d3 FROM c"), vec!["2020-01-01 00:00:00.250"]);
    assert_eq!(
        wire_row(&mut e, "SELECT d6 FROM c"),
        vec!["2020-01-01 00:00:00.250000"]
    );
    assert_eq!(wire_row(&mut e, "SELECT t2 FROM c"), vec!["10:00:00.50"]);
}

/// Precision 0 prints no fraction at all.
#[test]
fn precision_zero_prints_no_fraction() {
    let mut e = seeded();
    assert_eq!(wire_row(&mut e, "SELECT d0 FROM c"), vec!["2020-01-01 00:00:00"]);
}

/// A whole second still shows the declared digits, all zeros.
#[test]
fn whole_second_still_pads() {
    let mut e = mysql();
    e.execute("CREATE TABLE w(d3 DATETIME(3), d6 DATETIME(6))").unwrap();
    e.execute("INSERT INTO w VALUES('2020-01-01 00:00:00','2020-01-01 00:00:00')")
        .unwrap();
    assert_eq!(
        wire_row(&mut e, "SELECT d3, d6 FROM w"),
        vec!["2020-01-01 00:00:00.000", "2020-01-01 00:00:00.000000"]
    );
}

/// The precision survives `SELECT *`, an alias, and a derived expression
/// that reads the column (MariaDB keeps the source column's digits there).
#[test]
fn precision_rides_the_projection() {
    let mut e = seeded();
    // SELECT * keeps every column's own precision.
    assert_eq!(
        wire_row(&mut e, "SELECT * FROM c"),
        vec![
            "2020-01-01 00:00:00",
            "2020-01-01 00:00:00.250",
            "2020-01-01 00:00:00.250000",
            "10:00:00.50",
        ]
    );
    // An alias keeps it.
    assert_eq!(
        wire_row(&mut e, "SELECT d3 AS x FROM c"),
        vec!["2020-01-01 00:00:00.250"]
    );
    // A derived expression keeps the source column's three digits.
    assert_eq!(
        wire_row(&mut e, "SELECT d3 + INTERVAL 1 SECOND FROM c"),
        vec!["2020-01-01 00:00:01.250"]
    );
}

/// A non-temporal column and a PG session are untouched — `mysql_fsp` is
/// `None` there, and the renderer falls straight through.
#[test]
fn untouched_where_no_precision_is_declared() {
    let mut e = mysql();
    e.execute("CREATE TABLE n(i INT, d DATETIME(3))").unwrap();
    e.execute("INSERT INTO n VALUES(7,'2020-01-01 00:00:00.25')").unwrap();
    assert_eq!(
        wire_row(&mut e, "SELECT i, d FROM n"),
        vec!["7", "2020-01-01 00:00:00.250"]
    );

    let mut pg = Engine::new();
    pg.execute("CREATE TABLE p(a TIMESTAMP, b TIME)").unwrap();
    pg.execute("INSERT INTO p VALUES('2020-01-01 00:00:00.25','10:00:00.5')")
        .unwrap();
    // PG trims trailing zeros and keeps full microseconds.
    assert_eq!(
        wire_row(&mut pg, "SELECT a, b FROM p"),
        vec!["2020-01-01 00:00:00.25", "10:00:00.5"]
    );
}
