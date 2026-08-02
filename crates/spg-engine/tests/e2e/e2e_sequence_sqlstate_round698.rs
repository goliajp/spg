//! Round 698 — `DROP SEQUENCE nosuch` reported a CORRUPTION to the client.
//!
//! Measured against PG18:
//!
//!   PG18  `ERROR: 42P01: sequence "nosuch" does not exist`
//!   SPG   `ERROR: 42000: corrupt on-disk format: sequence "nosuch" does not exist`
//!
//! An operator reading that goes looking for a damaged file. Nothing was
//! damaged; a name was misspelled.
//!
//! The chain is worth keeping, because none of its three links is wrong on
//! its own:
//!
//!   1. The sequence not-found rides `StorageError::Corrupt`, whose Display
//!      prefixes `corrupt on-disk format: `. Several catalog errors do —
//!      `type "x" does not exist` among them.
//!   2. pgwire strips SPG's internal prefixes before a message reaches a
//!      client, but ONLY when the SQLSTATE is not the generic 42000: a
//!      typed state means the error was understood, and understanding it is
//!      what earns the right to rewrite the text.
//!   3. The classifier recognised `table "`, `relation "` and `view "`
//!      followed by "does not exist" — and not `sequence "`.
//!
//! So an unclassified error kept the banner, and the missing arm in (3) was
//! the only thing standing between a routine typo and a corruption report.
//! Adding `sequence "` fixes both the SQLSTATE and the text at once.
//!
//! The sweep afterwards found nothing else: seven shapes (missing sequence
//! / view / type, duplicate sequence / type, nextval and currval over a
//! missing sequence) now match PG18 on BOTH the state and the message.

use spg_engine::Engine;

/// The engine-side halves. The banner is a wire-layer artifact, so what an
/// engine test can pin is that the message underneath is PG's.
#[test]
fn round698_a_missing_sequence_says_only_that() {
    let mut e = Engine::new();
    for sql in [
        "DROP SEQUENCE nosuch698",
        "ALTER SEQUENCE nosuch698 RESTART",
        "SELECT nextval('nosuch698')",
    ] {
        let err = format!("{}", e.execute(sql).expect_err(sql));
        assert!(err.contains("nosuch698"), "{sql}: {err}");
        assert!(
            err.contains("does not exist"),
            "{sql}: should read like PG's: {err}"
        );
    }
}

/// And `IF EXISTS` still means do not complain.
#[test]
fn round698_if_exists_is_still_silent() {
    let mut e = Engine::new();
    e.execute("DROP SEQUENCE IF EXISTS nosuch698").unwrap();
}

/// The shapes the sweep covered, so a later change to any of them has to
/// answer for all of them together.
#[test]
fn round698_the_neighbouring_catalog_errors_still_name_their_object() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE dup698").unwrap();
    e.execute("CREATE TYPE dup698t AS ENUM ('a')").unwrap();
    for (sql, want) in [
        ("CREATE SEQUENCE dup698", "already exists"),
        ("CREATE TYPE dup698t AS ENUM ('b')", "already exists"),
        ("DROP VIEW nosuch698", "does not exist"),
        ("DROP TYPE nosuch698", "does not exist"),
    ] {
        let err = format!("{}", e.execute(sql).expect_err(sql));
        assert!(err.contains(want), "{sql}: {err}");
    }
}
