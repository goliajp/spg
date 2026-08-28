//! v7.39.2 — a written byte-order collation beats the database's.
//!
//! `'B' COLLATE utf8mb4_bin < 'a'` is 1 on MySQL 9.7.2 — 0x42 before
//! 0x61 — and answered 0 here, with `>` wrong to match. The FOLD was
//! already suppressed, so `=` was right and only the ordering
//! comparisons were wrong; the half that worked is what hid it.
//!
//! It only appears on a database that collates by a LOCALE, which is
//! what the shipped image does (`LANG=en_US.utf8`) and what
//! `Engine::new()` does not. On a byte-ordered database every one of
//! these rows was already right, which is why the earlier pins in this
//! area could not have caught it — the same reason the COLLATE-node
//! pins had to be built on a locale database too.
//!
//! The mechanism: `COLLATE utf8mb4_bin` lowers onto `CAST(x AS BINARY)`,
//! so the collation derivation never saw a collation at all and the
//! comparison fell back to the DATABASE's.

use spg_engine::{Engine, QueryResult};

/// The shipped image's own default, and the only configuration in which
/// this defect exists.
fn locale_mysql() -> Engine {
    let mut e = Engine::new();
    e.set_database_collation("en_US.utf8")
        .expect("the shipped image's own default");
    e.set_mysql_dialect(true);
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) if !rows.is_empty() => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        Ok(_) => "<none>".to_string(),
        Err(err) => panic!("{sql}: {err}"),
    }
}

#[test]
fn a_written_byte_order_collation_decides_the_ordering() {
    let mut e = locale_mysql();
    for (sql, want) in [
        // 0x42 < 0x61. Under the locale, 'B' sorts after 'a'.
        ("SELECT 'B' COLLATE utf8mb4_bin < 'a'", "true"),
        ("SELECT 'B' COLLATE utf8mb4_bin > 'a'", "false"),
        ("SELECT 'B' COLLATE utf8mb4_bin <= 'a'", "true"),
        // The two other spellings of the same request.
        ("SELECT BINARY 'B' < 'a'", "true"),
        ("SELECT CAST('B' AS BINARY) < 'a'", "true"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

#[test]
fn without_one_the_database_still_decides() {
    // The negative control: a comparison that asks for nothing must NOT
    // become byte order.
    //
    // `'B' < 'a'` cannot say so — MySQL's default folds, and 'b' < 'a'
    // is false the same way byte order says 0x42 < 0x61 is true only
    // WITHOUT the fold. `'é'` against `'z'` separates them: it is 0xC3A9,
    // so byte order puts it AFTER 'z' (0x7A) while both the locale and
    // the accent-insensitive fold put it before. Measured on MySQL
    // 9.7.2: 1 plain, 0 under `utf8mb4_bin`.
    let mut e = locale_mysql();
    assert_eq!(one(&mut e, "SELECT 'é' < 'z'"), "true");
    assert_eq!(one(&mut e, "SELECT 'é' COLLATE utf8mb4_bin < 'z'"), "false");
    assert_eq!(one(&mut e, "SELECT 'B' > 'a'"), "true");
}

#[test]
fn the_fold_is_unchanged() {
    // Equality was already right, and a fix aimed at ordering must not
    // move it: the default folds, the written binary one does not.
    let mut e = locale_mysql();
    assert_eq!(one(&mut e, "SELECT 'B' = 'b'"), "true");
    assert_eq!(one(&mut e, "SELECT 'B' COLLATE utf8mb4_bin = 'b'"), "false");
}

#[test]
fn a_column_compares_the_same_way() {
    let mut e = locale_mysql();
    e.execute("CREATE TABLE bo (s VARCHAR(4))").expect("create");
    e.execute("INSERT INTO bo VALUES ('B'),('a')")
        .expect("insert");
    assert_eq!(
        one(
            &mut e,
            "SELECT COUNT(*) FROM bo WHERE s COLLATE utf8mb4_bin < 'a'"
        ),
        "1"
    );
    // Without it, the locale decides and 'B' is not below 'a'.
    assert_eq!(one(&mut e, "SELECT COUNT(*) FROM bo WHERE s < 'a'"), "0");
}
