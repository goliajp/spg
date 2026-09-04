//! v7.39.13 — an instant written into a column keeps being an instant,
//! and a naive column gets the wall clock PostgreSQL puts there.
//!
//! Reported by sentori against 7.39.12, from three writes in their own
//! tree (`backfill_split.rs:165` and `:174`) whose targets are all
//! `timestamp with time zone`. Under a non-UTC session every one of
//! them shifted by the session's offset — silently, on data.
//!
//! The boundary, measured against PG 18.6 under `Asia/Tokyo` with one
//! instant (epoch 1767225600) copied six ways:
//!
//! ```text
//!                                   SPG 7.39.12   PG 18.6
//!   INSERT INTO d SELECT ts          1767193200   1767225600
//!   UPDATE d SET v = (SELECT ts …)   1767193200   1767225600
//!   UPDATE d SET v = s.ts FROM src   1767193200   1767225600
//!   CREATE TABLE d AS SELECT ts      1767225600   1767225600
//!   INSERT … VALUES ('…+00')         1767225600   1767225600
//!   INSERT INTO naive SELECT ts      1767225600   1767258000
//! ```
//!
//! The third row contains no subquery, which is what rules out "a
//! value that round-trips through a literal" as the boundary: it is
//! every assignment whose source is a query expression. The last row
//! is the same defect from the other side and was not reported —
//! PostgreSQL renders an instant in the session zone on its way into a
//! naive column, and SPG stored the UTC unchanged. The localisation
//! was, in effect, applied in exactly the two places it did not belong
//! and neither of the two where it did.
//!
//! Three causes behind one symptom. `expr_names_an_instant` asked the
//! SYNTAX — it recognised a string literal carrying an offset and a
//! `::timestamptz` cast, and answered "not an instant" for a column
//! reference and for a scalar subquery. It also did not recognise the
//! same cast written as `CastTarget::Named("timestamptz")`, which is
//! the form `value_to_literal_expr_typed` produces — so the type was
//! carried the whole way and dropped at the last step. And
//! `INSERT … SELECT` materialised its rows with a literal builder that
//! was never told the source column's type, though the type sat in the
//! same `QueryResult` behind a `..` pattern.

use spg_engine::{Engine, QueryResult};

/// The instant in the fixture, and what a nine-hour slip looks like.
const INSTANT: i64 = 1_767_225_600;
const SLIPPED_BACK: i64 = 1_767_193_200;
const AS_TOKYO_WALL: i64 = 1_767_258_000;

fn epoch_of(e: &mut Engine, sql: &str) -> i64 {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    match &rows[0].values[0] {
        spg_storage::Value::BigInt(n) => *n,
        spg_storage::Value::Int(n) => i64::from(*n),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The zone lookups come from the host's zoneinfo, which the server and
/// the embedded surface both inject. Without them `SET TimeZone` reaches
/// nothing and EVERY row below passes whether or not the defect is
/// present — which is how the first cut of this file reported five
/// greens against an engine that could not have been wrong.
fn host_has_tzdata() -> bool {
    spg_tzif::tz_offset_at("Asia/Tokyo", 0).is_some()
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.set_tz_fns(
        spg_tzif::tz_offset_at,
        spg_tzif::tz_local_to_utc,
        spg_tzif::tz_canonical,
        spg_tzif::tz_abbrev_at,
    );
    for sql in [
        "SET TimeZone = 'Asia/Tokyo'",
        "CREATE TABLE src (ts timestamptz)",
        "INSERT INTO src VALUES ('2026-01-01 00:00:00+00')",
    ] {
        e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"));
    }
    e
}

#[test]
fn the_fixture_itself_holds_the_instant() {
    if !host_has_tzdata() {
        return;
    }
    // Without this the rows below could all agree on a wrong number.
    let mut e = seeded();
    assert_eq!(
        epoch_of(&mut e, "SELECT extract(epoch FROM ts)::bigint FROM src"),
        INSTANT
    );
}

#[test]
fn insert_select_into_a_timestamptz_column_keeps_the_instant() {
    if !host_has_tzdata() {
        return;
    }
    let mut e = seeded();
    e.execute("CREATE TABLE d (v timestamptz)").unwrap();
    e.execute("INSERT INTO d SELECT ts FROM src").unwrap();
    let got = epoch_of(&mut e, "SELECT extract(epoch FROM v)::bigint FROM d");
    assert_ne!(got, SLIPPED_BACK, "the session offset was subtracted again");
    assert_eq!(got, INSTANT);
}

#[test]
fn a_scalar_subquery_assignment_keeps_the_instant() {
    if !host_has_tzdata() {
        return;
    }
    let mut e = seeded();
    e.execute("CREATE TABLE d (id int, v timestamptz)").unwrap();
    e.execute("INSERT INTO d VALUES (1, NULL)").unwrap();
    e.execute("UPDATE d SET v = (SELECT ts FROM src LIMIT 1) WHERE id = 1")
        .unwrap();
    let got = epoch_of(&mut e, "SELECT extract(epoch FROM v)::bigint FROM d");
    assert_ne!(got, SLIPPED_BACK);
    assert_eq!(got, INSTANT);
}

/// The row with no subquery in it. `UPDATE … FROM` is an ordinary join,
/// and it shifted too — which is why the boundary is the source's TYPE
/// and not the shape of the statement.
#[test]
fn a_join_source_keeps_the_instant() {
    if !host_has_tzdata() {
        return;
    }
    let mut e = seeded();
    e.execute("CREATE TABLE d (id int, v timestamptz)").unwrap();
    e.execute("INSERT INTO d VALUES (1, NULL)").unwrap();
    e.execute("UPDATE d SET v = s.ts FROM src s WHERE d.id = 1")
        .unwrap();
    let got = epoch_of(&mut e, "SELECT extract(epoch FROM v)::bigint FROM d");
    assert_ne!(got, SLIPPED_BACK);
    assert_eq!(got, INSTANT);
}

/// The other direction, which nobody reported: PostgreSQL renders the
/// instant in the session zone on its way into a NAIVE column. Storing
/// the UTC unchanged is a different wrong answer, not a safe one.
#[test]
fn an_instant_into_a_naive_column_becomes_the_session_wall_clock() {
    if !host_has_tzdata() {
        return;
    }
    let mut e = seeded();
    e.execute("CREATE TABLE naive (v timestamp)").unwrap();
    e.execute("INSERT INTO naive SELECT ts FROM src").unwrap();
    assert_eq!(
        epoch_of(&mut e, "SELECT extract(epoch FROM v)::bigint FROM naive"),
        AS_TOKYO_WALL,
        "PG 18.6 stores the wall clock a reader in the session zone sees"
    );
}

/// And the two that were already right stay right — a fix that stopped
/// localising ANYTHING would pass every row above.
#[test]
fn a_naive_literal_is_still_read_in_the_session_zone() {
    if !host_has_tzdata() {
        return;
    }
    let mut e = seeded();
    e.execute("CREATE TABLE d (v timestamptz)").unwrap();
    // No offset in the text: PG reads it as a wall clock in Asia/Tokyo,
    // so the stored instant is nine hours EARLIER than the same digits
    // read as UTC.
    e.execute("INSERT INTO d VALUES ('2026-01-01 00:00:00')")
        .unwrap();
    assert_eq!(
        epoch_of(&mut e, "SELECT extract(epoch FROM v)::bigint FROM d"),
        SLIPPED_BACK,
        "a naive literal must still be localised into the session zone"
    );
}

#[test]
fn a_literal_carrying_an_offset_is_stored_as_written() {
    if !host_has_tzdata() {
        return;
    }
    let mut e = seeded();
    e.execute("CREATE TABLE d (v timestamptz)").unwrap();
    e.execute("INSERT INTO d VALUES ('2026-01-01 00:00:00+00')")
        .unwrap();
    assert_eq!(
        epoch_of(&mut e, "SELECT extract(epoch FROM v)::bigint FROM d"),
        INSTANT
    );
}
