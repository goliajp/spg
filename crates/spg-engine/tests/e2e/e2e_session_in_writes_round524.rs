//! v7.39 (round 524) — a write statement sees the session.
//!
//! Round 523 left the UPDATE assignment path unmeasured. Measuring it
//! found the cause is not about timestamps at all: every write path
//! built a BARE evaluation context, so each expression in an UPDATE's
//! SET or a DELETE's WHERE was evaluated by an engine that knew nothing
//! about the connection. Measured against PG18:
//!
//!     SELECT current_user                       PG bench  SPG unmei
//!     UPDATE w SET a = current_user             PG bench  SPG admin
//!     UPDATE w SET b = current_setting('app…')  PG zzz    SPG '' (empty)
//!     UPDATE … WHERE current_setting(…) = 'zzz' PG 1 row  SPG 0 rows
//!     DELETE … WHERE current_setting(…) = 'zzz' PG 0 left SPG 2 left
//!
//! The predicate cases are the worst of it: a write gated on a
//! request-context GUC — the shape RLS policies and multi-tenant code
//! are written in — silently touched nothing. Nothing errored anywhere.
//!
//! Two session-driven READINGS travelled with it, both of which change
//! what gets STORED rather than what is displayed: an ambiguous date is
//! read in the session's order, and a naive value bound for a
//! timestamptz column is a wall-clock reading in the session zone.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    Engine::new()
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// A value written by an UPDATE is the one a SELECT would have read.
/// This is the audit-column case: `SET updated_by = current_user` stored
/// the engine's default role rather than the connected one.
#[test]
fn round524_update_set_sees_the_session() {
    let mut e = engine();
    e.execute("SET application_name = 'zzz'").unwrap();
    e.execute("CREATE TABLE w (a TEXT, b TEXT)").unwrap();
    e.execute("INSERT INTO w VALUES ('x', 'y')").unwrap();
    e.execute("UPDATE w SET a = current_user, b = current_setting('application_name')")
        .unwrap();
    let expect = text(
        &mut e,
        "SELECT current_user, current_setting('application_name')",
    );
    assert_eq!(text(&mut e, "SELECT a, b FROM w"), expect);
}

/// A write gated on a request-context GUC. This matched nothing, so the
/// statement reported success and changed no rows.
#[test]
fn round524_update_where_sees_the_session() {
    let mut e = engine();
    e.execute("SET app.tenant = 'acme'").unwrap();
    e.execute("CREATE TABLE t (a INT, tenant TEXT)").unwrap();
    e.execute("INSERT INTO t VALUES (1, 'acme'), (2, 'other')")
        .unwrap();
    e.execute("UPDATE t SET a = 99 WHERE tenant = current_setting('app.tenant')")
        .unwrap();
    assert_eq!(text(&mut e, "SELECT a FROM t WHERE tenant = 'acme'"), "99");
    // And the other tenant's row is untouched.
    assert_eq!(text(&mut e, "SELECT a FROM t WHERE tenant = 'other'"), "2");
}

/// The same for DELETE, which took its predicate down the same bare path.
#[test]
fn round524_delete_where_sees_the_session() {
    let mut e = engine();
    e.execute("SET app.tenant = 'acme'").unwrap();
    e.execute("CREATE TABLE t2 (a INT, tenant TEXT)").unwrap();
    e.execute("INSERT INTO t2 VALUES (1, 'acme'), (2, 'other')")
        .unwrap();
    e.execute("DELETE FROM t2 WHERE tenant = current_setting('app.tenant')")
        .unwrap();
    assert_eq!(text(&mut e, "SELECT count(*) FROM t2"), "1");
    assert_eq!(text(&mut e, "SELECT tenant FROM t2"), "other");
}

/// `01/02/2020` is February 1st in a DMY session. The write path read
/// every written date as MDY, so a SELECT of the literal answered PG's
/// value while the same literal INSERTed stored the day and month
/// swapped — and once stored the two readings are indistinguishable.
#[test]
fn round524_written_dates_use_the_session_order() {
    let mut e = engine();
    e.execute("SET DateStyle = 'ISO, DMY'").unwrap();
    e.execute("CREATE TABLE d (a DATE, b TIMESTAMP)").unwrap();
    // The reading a SELECT gives.
    assert_eq!(text(&mut e, "SELECT '01/02/2020'::date::text"), "2020-02-01");
    // …is the one the write stores.
    e.execute("INSERT INTO d VALUES ('01/02/2020', '01/02/2020 10:00')")
        .unwrap();
    assert_eq!(
        text(&mut e, "SELECT a::text, b::text FROM d"),
        "2020-02-01|2020-02-01 10:00:00"
    );
    e.execute("UPDATE d SET a = '03/04/2020'").unwrap();
    assert_eq!(text(&mut e, "SELECT a::text FROM d"), "2020-04-03");
    // An MDY session is unchanged.
    let mut m = engine();
    m.execute("CREATE TABLE d2 (a DATE)").unwrap();
    m.execute("INSERT INTO d2 VALUES ('01/02/2020')").unwrap();
    assert_eq!(text(&mut m, "SELECT a::text FROM d2"), "2020-01-02");
}

/// `SET bytea_output = 'escape'` was accepted and never read, so a
/// client that asked for escape got the form it had just said it did not
/// want.
#[test]
fn round524_bytea_output_escape_is_honoured() {
    let mut e = engine();
    e.execute("SET bytea_output = 'escape'").unwrap();
    // Printable as itself, a backslash doubled, the rest three-digit
    // octal.
    assert_eq!(
        text(&mut e, r"SELECT '\x41425c00ff'::bytea::text"),
        r"AB\\\000\377"
    );
    // The default is still hex.
    let mut h = engine();
    assert_eq!(
        text(&mut h, r"SELECT '\x41425c00ff'::bytea::text"),
        r"\x41425c00ff"
    );
}

/// The case round 523 recorded and did not measure: an UPDATE assigning
/// a naive value to a timestamptz column reads it in the session zone,
/// exactly as INSERT does. The stored INSTANT was nine hours out.
#[test]
fn round524_update_to_timestamptz_reads_the_session_zone() {
    if spg_tzif::tz_offset_at("Asia/Tokyo", 0).is_none() {
        return;
    }
    let mut e = Engine::new();
    e.set_tz_fns(
        spg_tzif::tz_offset_at,
        spg_tzif::tz_local_to_utc,
        spg_tzif::tz_canonical,
        spg_tzif::tz_abbrev_at,
    );
    e.execute("SET TimeZone = 'Asia/Tokyo'").unwrap();
    e.execute("CREATE TABLE u (a timestamptz)").unwrap();
    e.execute("INSERT INTO u VALUES (TIMESTAMPTZ '2000-01-01 00:00:00Z')")
        .unwrap();
    for (rhs, expect) in [
        ("'2020-01-01 00:00:00'", "2020-01-01 00:00:00+09"),
        ("TIMESTAMP '2020-01-01 00:00:00'", "2020-01-01 00:00:00+09"),
        // Already an instant — not shifted a second time.
        ("TIMESTAMPTZ '2020-01-01 00:00:00Z'", "2020-01-01 09:00:00+09"),
        ("'2020-01-01 00:00:00+05'", "2020-01-01 04:00:00+09"),
    ] {
        e.execute(&format!("UPDATE u SET a = {rhs}")).unwrap();
        assert_eq!(
            text(&mut e, "SELECT a::text FROM u"),
            expect,
            "UPDATE u SET a = {rhs}"
        );
    }
}
