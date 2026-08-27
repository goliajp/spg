//! v7.39 — "what version are you", asked every way a MySQL client can.
//!
//! SPG answered this three ways on one wire: the handshake advertised
//! `8.0.0-spg-v…`, `@@version` said `8.0.35-spg`, and `version()`
//! answered `PostgreSQL 18.6 (spg)` — a MySQL driver that branches on
//! `version()` was told it had reached the wrong product entirely.
//!
//! The same defect had just been fixed on the PostgreSQL wire
//! (`every_server_version_surface_agrees`, v7.38.25) and this side was
//! left as it was, which is the pattern this repository keeps meeting:
//! one implementation of a shared idea gets fixed and its siblings do
//! not. So this pins the agreement rather than any one value.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    // The MySQL dialect is what `backslash_escapes` selects.
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

/// Every cell of the first row, joined — `SHOW VARIABLES` answers with a
/// name/value pair, the other two with one column, and what this test
/// asserts is that the version string is in there either way.
fn row_text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        Value::Text(t) => t.to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default(),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

#[test]
fn every_mysql_version_surface_agrees() {
    let mut e = mysql();
    let want = spg_engine::MYSQL_SERVER_VERSION;
    for sql in [
        "SELECT version()",
        "SELECT @@version",
        "SHOW VARIABLES LIKE 'version'",
    ] {
        let got = row_text(&mut e, sql);
        assert!(
            got.contains(want),
            "{sql} answered {got}, which does not carry {want}"
        );
    }
}

#[test]
fn the_mysql_version_tracks_the_oracle_it_is_measured_against() {
    // `xtests/oracle/mysql/Dockerfile` pins `mysql:9.7.2`. The rule is
    // that SPG advertises the release it is actually differentiated
    // against, and both move together — the literal here is the reminder
    // that the Dockerfile is the other half.
    let v = spg_engine::MYSQL_SERVER_VERSION;
    assert!(
        v.starts_with("9."),
        "SPG advertises the current stable MySQL line; got {v}"
    );
    assert!(
        v.contains("spg"),
        "a client must be able to tell which server answered: {v}"
    );
}

/// `SET NAMES` used to be parsed and thrown away.
///
/// The comment that justified it — "SPG stores UTF-8 always and orders
/// bytewise; accept as a no-op" — stopped being true when collations
/// arrived, and became a wrong answer once `collation_connection` began
/// driving comparison: `SET NAMES utf8mb4 COLLATE utf8mb4_general_ci`
/// reported back `utf8mb4_0900_ai_ci` and compared as NO PAD, without a
/// word of complaint.
///
/// Every expectation below is from MySQL 9.7.2, measured:
///
///     SET NAMES utf8mb4                       utf8mb4_0900_ai_ci   'a'='a ' 0
///     SET NAMES latin1                        latin1_swedish_ci    'a'='a ' 1
///     SET NAMES utf8mb4 COLLATE utf8mb4_bin   utf8mb4_bin          'a'='a ' 1
///     SET NAMES utf8mb4 COLLATE …_0900_ai_ci  utf8mb4_0900_ai_ci   'a'='a ' 0
///
/// The two bare forms are why the charset→default-collation table is
/// read from `information_schema.CHARACTER_SETS` rather than assumed:
/// the same statement shape pads or does not depending on the charset.
#[test]
fn set_names_carries_its_charset_and_collation() {
    let mut e = mysql();

    for (stmt, want_collation, want_pad) in [
        ("SET NAMES utf8mb4", "utf8mb4_0900_ai_ci", false),
        ("SET NAMES latin1", "latin1_swedish_ci", true),
        ("SET NAMES utf8mb4 COLLATE utf8mb4_bin", "utf8mb4_bin", true),
        (
            "SET NAMES utf8mb4 COLLATE utf8mb4_0900_ai_ci",
            "utf8mb4_0900_ai_ci",
            false,
        ),
    ] {
        e.execute(stmt)
            .unwrap_or_else(|err| panic!("{stmt}: {err}"));
        let got = row_text(&mut e, "SELECT @@collation_connection");
        assert!(
            got.contains(want_collation),
            "{stmt} left collation_connection at {got}, wanted {want_collation}"
        );
        let pad = row_text(&mut e, "SELECT 'a' = 'a '");
        assert_eq!(
            pad.contains("true") || pad.contains("Bool(true)"),
            want_pad,
            "{stmt} → {want_collation}: 'a' = 'a ' should be {want_pad}, got {pad}"
        );
    }

    // The charset trio moves too.
    e.execute("SET NAMES latin1").unwrap();
    for v in [
        "@@character_set_client",
        "@@character_set_connection",
        "@@character_set_results",
    ] {
        let got = row_text(&mut e, &format!("SELECT {v}"));
        assert!(got.contains("latin1"), "{v} is {got}");
    }
}

/// `SHOW VARIABLES LIKE 'x'` and `@@x` must not disagree.
///
/// They did, four times over, in the same pair of files
/// (`show.rs` and `eval/functions.rs`):
///
///     version                8.0.35-spg  vs  8.0.35-spg  … and version() said PostgreSQL
///     collation_server       utf8mb4_0900_ai_ci  vs  utf8mb4_general_ci
///     max_allowed_packet     67108864            vs  16777216
///     version_comment        "SPG dual-stack engine"  vs  "SPG (MySQL-compatible)"
///     sql_mode               2 flags             vs  3 flags
///
/// Pinning the VALUES would need updating every time one moves; what
/// has to hold is that the two surfaces answer the same. Measured on
/// MySQL 9.7.2, they agree on every variable tested here except
/// `autocommit`, where MySQL itself answers `ON` to `SHOW` and `1` to
/// `@@` — so that one is the documented exception rather than a fifth
/// defect, which is only knowable by asking MySQL.
#[test]
fn the_two_variable_surfaces_agree() {
    let mut e = mysql();
    // `autocommit` is excluded on purpose: MySQL 9.7.2 answers ON / 1.
    for v in [
        "version",
        "version_comment",
        "collation_server",
        "character_set_server",
        "max_allowed_packet",
        "transaction_isolation",
    ] {
        let show = row_text(&mut e, &format!("SHOW VARIABLES LIKE '{v}'"));
        let at = row_text(&mut e, &format!("SELECT @@{v}"));
        let shown = show.trim_start_matches(v).trim();
        assert_eq!(
            shown, at,
            "{v}: SHOW VARIABLES says {shown:?} and @@{v} says {at:?}"
        );
    }

    // `have_ssl` was removed in MySQL 8.0.26 and 9.7.2 answers nothing
    // for it. SPG answered `YES`, which is a variable the engine it
    // claims to be does not have.
    assert!(
        e.execute("SELECT @@have_ssl").is_err(),
        "have_ssl does not exist in MySQL 9.7.2 and must not exist here"
    );
}

/// One question, three surfaces, and until v7.39 three separate hard-coded
/// literals — two of which contradicted the engine. The spellings are
/// MySQL 9.7.2's own, read back after setting each level; note only
/// SERIALIZABLE has no hyphen, because it is one word.
#[test]
fn the_isolation_level_is_reported_live_on_every_surface() {
    for (level, mysql_spelling, pg_spelling) in [
        ("READ UNCOMMITTED", "READ-UNCOMMITTED", "read uncommitted"),
        ("READ COMMITTED", "READ-COMMITTED", "read committed"),
        ("REPEATABLE READ", "REPEATABLE-READ", "repeatable read"),
        ("SERIALIZABLE", "SERIALIZABLE", "serializable"),
    ] {
        let mut e = mysql();
        e.execute(&format!("BEGIN ISOLATION LEVEL {level}"))
            .unwrap_or_else(|err| panic!("BEGIN {level}: {err}"));
        let show = row_text(&mut e, "SHOW VARIABLES LIKE 'transaction_isolation'");
        let at = row_text(&mut e, "SELECT @@transaction_isolation");
        let shown = show.trim_start_matches("transaction_isolation").trim();
        assert_eq!(shown, mysql_spelling, "SHOW VARIABLES under {level}");
        assert_eq!(at, mysql_spelling, "@@transaction_isolation under {level}");

        // PG's two surfaces make the same promise, in PG's spelling.
        // Measured on PG 18.6: inside BEGIN ISOLATION LEVEL SERIALIZABLE
        // both `SHOW` and `current_setting` answer `serializable`.
        let mut p = Engine::new();
        p.execute(&format!("BEGIN ISOLATION LEVEL {level}"))
            .unwrap_or_else(|err| panic!("pg BEGIN {level}: {err}"));
        let show_pg = row_text(&mut p, "SHOW transaction_isolation");
        let cs = row_text(&mut p, "SELECT current_setting('transaction_isolation')");
        assert_eq!(show_pg, pg_spelling, "SHOW under {level}");
        assert_eq!(cs, pg_spelling, "current_setting() under {level}");
    }
}

/// MySQL 8.0.3 removed `tx_isolation`. SPG answered it, which is the same
/// defect as answering `have_ssl`: a variable the engine SPG claims to be
/// does not have. Measured on 9.7.2: `@@tx_isolation` errors with
/// `Unknown system variable`, and `SHOW VARIABLES LIKE 'tx_isolation'`
/// returns zero rows.
#[test]
fn a_variable_mysql_removed_is_not_answered_here() {
    let mut e = mysql();
    assert!(
        e.execute("SELECT @@tx_isolation").is_err(),
        "tx_isolation was removed in MySQL 8.0.3 and must not answer here"
    );
}
