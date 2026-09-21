//! 9.0.1 — `name` carries the `C` collation, whatever the database
//! collates as.
//!
//! `name` is the catalog's identifier type, and PostgreSQL gives it `C`
//! at the TYPE level so a catalog listing is ordered the same way on
//! every server. SPG compared it under the database's collation, so
//! every `ORDER BY relname` — which is what psql's listings, a
//! schema-diff tool and a dump's object order are built on — came back
//! in a different order from PostgreSQL's whenever two names differed
//! by punctuation.
//!
//! Found by the differential corpus, not by a test here: with a table
//! `t`, its primary-key index `t_pkey` and an index `tj`, PostgreSQL
//! 18.6 lists `t_pkey` before `tj` (bytes: `_` is 0x5F, `j` is 0x6A)
//! and SPG listed `tj` first (the locale ignores the punctuation).
//!
//! Measured on PostgreSQL 18.6 in an `en_US.utf8` database:
//!
//! ```text
//!   'tj'::name < 't_pkey'::name           f
//!   'tj'::name < 't_pkey'                 f
//!   'tj'::name < 't_pkey'::text           f
//!   'tj'::text < 't_pkey'::text           t     ← the locale's answer
//!   min over name                         t_pkey
//!   … COLLATE "en_US.utf8"                t     ← an explicit name wins
//! ```

use spg_engine::{Engine, QueryResult};

fn cells(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(" "),
        other => panic!("{sql}: {other:?}"),
    }
}

/// An engine whose DATABASE collates by a locale — the shipped
/// configuration, and the only one under which this defect exists. On a
/// byte-ordering database every answer below is already right, which is
/// why no pin caught it.
fn locale_engine() -> Engine {
    let mut e = Engine::new();
    e.set_database_collation("en_US.UTF-8")
        .expect("the shipped collation");
    e
}

#[test]
fn a_name_compares_by_bytes_while_text_compares_by_the_locale() {
    let mut e = locale_engine();
    // The control sits in the same row: `text` still answers the
    // locale's order, so a `false` beside it is the TYPE's doing and
    // not a collator that stopped being consulted.
    assert_eq!(
        cells(
            &mut e,
            "SELECT 'tj'::name < 't_pkey'::name, 'tj'::text < 't_pkey'::text"
        ),
        "false|true"
    );
    // A `name` takes the comparison with it, whatever the other side is.
    assert_eq!(
        cells(
            &mut e,
            "SELECT 'tj'::name < 't_pkey', 'tj'::name < 't_pkey'::text"
        ),
        "false|false"
    );
}

#[test]
fn an_explicit_collation_still_wins_over_the_types_own() {
    let mut e = locale_engine();
    assert_eq!(
        cells(
            &mut e,
            "SELECT x FROM (VALUES ('tj'::name),('t_pkey'::name)) v(x) \
             ORDER BY x COLLATE \"en_US.UTF-8\""
        ),
        "tj t_pkey"
    );
    // …and without it, the type's own `C`.
    assert_eq!(
        cells(
            &mut e,
            "SELECT x FROM (VALUES ('tj'::name),('t_pkey'::name)) v(x) ORDER BY x"
        ),
        "t_pkey tj"
    );
}

#[test]
fn a_synthetic_source_sorts_the_way_a_table_does() {
    // 9.0.1 — four sorts over a synthetic source (`unnest`,
    // `generate_series`, `jsonb_each_text`, and the derived-table / SRF
    // projection) compared their keys collation-blind, and the
    // combined sort of a UNION did too. The same rows read from a
    // TABLE were already right, which is what kept it out of sight.
    //
    // PostgreSQL 18.6 on an `en_US.utf8` database answers `tj` first to
    // every one of these: the locale ignores the punctuation, and
    // bytes do not (`_` is 0x5F, `j` is 0x6A).
    let mut e = locale_engine();
    e.execute("CREATE TABLE w(x text)").unwrap();
    e.execute("INSERT INTO w VALUES ('tj'),('t_pkey')").unwrap();
    // The control: from a table, which was right before this.
    assert_eq!(cells(&mut e, "SELECT x FROM w ORDER BY x"), "tj t_pkey");
    for sql in [
        "SELECT x FROM (VALUES ('tj'::text),('t_pkey'::text)) v(x) ORDER BY x",
        "SELECT x FROM w UNION ALL SELECT x FROM w ORDER BY x",
        "SELECT unnest(ARRAY['tj','t_pkey']) u ORDER BY u",
    ] {
        assert!(
            cells(&mut e, sql).starts_with("tj"),
            "{sql}: {}",
            cells(&mut e, sql)
        );
    }
}

#[test]
fn a_whole_row_key_compares_its_fields_the_way_a_column_does() {
    // 9.0.1 — a composite's sort key is an Array of its fields' keys,
    // and the Array comparison dropped the collation, so a whole-row
    // `ORDER BY` compared a text field by BYTES while the same field
    // as a column compared under the database's collation.
    // PostgreSQL 18.6: `(tj) (t_pkey)`; SPG answered `(t_pkey) (tj)`.
    let mut e = locale_engine();
    e.execute("CREATE TABLE w(x text)").unwrap();
    e.execute("INSERT INTO w VALUES ('tj'),('t_pkey')").unwrap();
    assert_eq!(cells(&mut e, "SELECT w FROM w ORDER BY w"), "(tj) (t_pkey)");
    assert_eq!(
        cells(&mut e, "SELECT w FROM w ORDER BY w DESC"),
        "(t_pkey) (tj)"
    );
    // The control, in the same engine: the field itself.
    assert_eq!(cells(&mut e, "SELECT x FROM w ORDER BY x"), "tj t_pkey");
}

#[test]
fn a_collation_comes_from_the_inputs_and_rides_through_a_cast() {
    // PostgreSQL derives an expression's collation from its INPUTS and
    // carries it through a cast; the result type's own applies only
    // when no input has one. Measured on 18.6 in an `en_US.utf8`
    // database, with `x` a text column and `nn.x` a `name` column:
    //
    //   min(x::name)      tj       ← x's, through the cast
    //   min(nn.x)         t_pkey   ← the name type's own `C`
    //   'tj'::name < …    f        ← literals carry none, so `C`
    let mut e = locale_engine();
    e.execute("CREATE TABLE nc(x text)").unwrap();
    e.execute("INSERT INTO nc VALUES ('tj'),('t_pkey')")
        .unwrap();
    e.execute("CREATE TABLE nn(x name)").unwrap();
    e.execute("INSERT INTO nn VALUES ('tj'),('t_pkey')")
        .unwrap();
    assert_eq!(cells(&mut e, "SELECT min(x::name) FROM nc"), "tj");
    assert_eq!(
        cells(&mut e, "SELECT x FROM nc ORDER BY x::name"),
        "tj t_pkey"
    );
    assert_eq!(cells(&mut e, "SELECT min(x), max(x) FROM nn"), "t_pkey|tj");
    assert_eq!(cells(&mut e, "SELECT x FROM nn ORDER BY x"), "t_pkey tj");
}

#[test]
fn min_and_max_derive_their_argument_the_way_an_order_by_key_does() {
    // 9.0.1 — only a BARE COLUMN took a collation here; the comment
    // that stood beside it said an expression argument "has none
    // (derivation is unbuilt)", and by then it had been built for an
    // ORDER BY key. PostgreSQL 18.6 answers `tj` to all three.
    let mut e = locale_engine();
    e.execute("CREATE TABLE nc(x text)").unwrap();
    e.execute("INSERT INTO nc VALUES ('tj'),('t_pkey')")
        .unwrap();
    assert_eq!(
        cells(&mut e, "SELECT min(x), min(x||''), min(upper(x)) FROM nc"),
        "tj|tj|TJ"
    );
    // …and an explicit `C` on the argument still wins.
    assert_eq!(
        cells(&mut e, "SELECT min(x COLLATE \"C\") FROM nc"),
        "t_pkey"
    );
}

#[test]
fn a_catalog_listing_is_ordered_the_way_postgresql_orders_it() {
    let mut e = locale_engine();
    e.execute("CREATE TABLE t(id int primary key, a text)")
        .unwrap();
    e.execute("CREATE INDEX tj ON t(a)").unwrap();
    assert_eq!(
        cells(
            &mut e,
            "SELECT relname FROM pg_class WHERE relname IN ('t_pkey','tj') ORDER BY relname"
        ),
        "t_pkey tj"
    );
    // The floor: both rows are there, so the order above is an order
    // and not a filter.
    assert_eq!(
        cells(
            &mut e,
            "SELECT count(*) FROM pg_class WHERE relname IN ('t_pkey','tj')"
        ),
        "2"
    );
}
