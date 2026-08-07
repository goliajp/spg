//! Round 690 — `min`/`max` and a window's ORDER BY honour a declared
//! collation, and a collation tie is broken by the bytes.
//!
//! Round 688 reached every ordinary SORT. These two shapes do not sort the
//! rows: `min`/`max` fold a running extreme, and a window sorts a key tuple
//! it built for itself. Both compare text, so both were still answering by
//! bytes — `min(loc), max(loc)` gave `Banana | Ápple` where PG18 gives
//! `apple | Zebra`.
//!
//! Two things worth recording about the fix:
//!
//!   * The collation resolver for `min`/`max` first went beside the one for
//!     enum labels, which is where it belongs — both are facts about the
//!     ARGUMENT that the comparison cannot look up for itself. But that
//!     resolver sits inside `if catalog has any enum type`, so with no enum
//!     in the database it never ran. A collation has nothing to do with
//!     enums; it needed its own unconditional pass.
//!
//!   * The fused aggregate lane sends an ENUM argument to the generic path,
//!     because it does not carry member order. A collation could have taken
//!     the same exit. It rides along instead, so a collated column keeps the
//!     shard-parallel scan.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
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
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seed(e: &mut Engine) {
    e.execute("CREATE TABLE m690(id INT, loc TEXT COLLATE \"en_US.utf8\", plain TEXT)")
        .unwrap();
    e.execute(
        "INSERT INTO m690 VALUES (1,'apple','apple'),(2,'Banana','Banana'),\
         (3,'cherry','cherry'),(4,'Ápple','Ápple'),(5,'Zebra','Zebra')",
    )
    .unwrap();
}

/// PG18: `apple | Zebra`. Byte order gives `Banana | Ápple`, so this pin
/// fails loudly if the collation stops reaching the extreme fold.
#[test]
fn round690_min_max_honour_the_collation() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        one(&mut e, "SELECT min(loc), max(loc) FROM m690"),
        "apple|Zebra"
    );
    // The undeclared column keeps byte order — without this the pin above
    // would pass equally well if the collation were applied to everything.
    assert_eq!(
        one(&mut e, "SELECT min(plain), max(plain) FROM m690"),
        "Banana|Ápple"
    );
}

/// The grouped path folds its extremes elsewhere than the single-group fused
/// lane, so it is pinned separately — this is the shape that stayed wrong
/// after the fused lane was fixed, and vice versa.
#[test]
fn round690_grouped_min_max_honour_the_collation() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        one(
            &mut e,
            "SELECT min(loc), max(loc) FROM m690 GROUP BY (id > 0)"
        ),
        "apple|Zebra"
    );
}

/// A window's ORDER BY sorts a key tuple built per row; the collation is
/// resolved from the key's bound column position.
#[test]
fn round690_window_order_by_honours_the_collation() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        one(
            &mut e,
            "SELECT loc FROM (SELECT loc, row_number() OVER (ORDER BY loc) rn \
             FROM m690) s ORDER BY rn"
        ),
        "apple,Ápple,Banana,cherry,Zebra"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT plain FROM (SELECT plain, row_number() OVER (ORDER BY plain) rn \
             FROM m690) s ORDER BY rn"
        ),
        "Banana,Zebra,apple,cherry,Ápple"
    );
}

/// The deterministic tiebreak. `e` + U+0301 and U+00E9 are canonically
/// equivalent, so ICU calls them Equal; PG18's `en_US.utf8` is
/// `collisdeterministic = t` and orders the decomposed form first (0x65 <
/// 0xC3). Measured against PG18, not assumed.
#[test]
fn round690_a_collation_tie_is_broken_by_the_bytes() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d690(v TEXT COLLATE \"en_US.utf8\")")
        .unwrap();
    e.execute("INSERT INTO d690 VALUES (U&'\\00E9'), ('e\u{301}'), ('f')")
        .unwrap();
    // Decomposed (2 chars) before precomposed (1 char), both before `f`.
    assert_eq!(
        one(&mut e, "SELECT length(v) FROM d690 ORDER BY v"),
        "2,1,1"
    );
}
